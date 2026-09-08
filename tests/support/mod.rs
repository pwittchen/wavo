//! Test doubles shared by the integration tests: a scriptable HTTP server (for
//! the OpenAI and plainsong clients) and a scriptable tool box (for the LLM
//! loop). No test in this repo may touch the real OpenAI, Telegram or YouTube.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use wavo::llm::openai::ToolDef;
use wavo::tools::{ToolBox, TurnCtx};

// --- configuration -----------------------------------------------------------

/// A `Config` with the documented defaults and no secrets, for tests that need
/// to build the tool layer without an environment.
pub fn test_config(work_dir: std::path::PathBuf) -> wavo::config::Config {
    use std::time::Duration;
    use wavo::config::Secret;

    wavo::config::Config {
        telegram_token: Secret::new("test-telegram-token"),
        telegram_api_base: "http://127.0.0.1:1".to_string(),
        allowed_chat_ids: vec![42],
        openai_api_key: Secret::new("test-openai-key"),
        openai_base_url: "http://127.0.0.1:1".to_string(),
        openai_model: "test-mini".to_string(),
        openai_model_fallback: "test-large".to_string(),
        plainsong_url: "http://127.0.0.1:1".to_string(),
        plainsong_public_url: "https://music.example.com".to_string(),
        plainsong_token: Secret::new("test-plainsong-token"),
        plainsong_max_upload_mb: 100,
        proxy: None,
        work_dir,
        mcp_command: "demix-mcp".to_string(),
        max_concurrent_jobs: 1,
        max_tool_iterations: 8,
        turn_timeout: Duration::from_secs(1800),
        job_timeout: Duration::from_secs(1200),
        llm_timeout: Duration::from_secs(90),
        history_turns: 12,
        session_ttl: Duration::from_secs(7200),
        coalesce_window: Duration::from_secs(15),
        tool_output_chars: 2000,
        progress_interval: Duration::from_secs(5),
        keep_job_files: false,
        expose_clean: false,
        allow_delete: false,
        health_addr: "127.0.0.1:0".to_string(),
        shutdown_grace: Duration::from_secs(30),
    }
}

// --- a scriptable HTTP server ------------------------------------------------

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub body: String,
    pub authorization: Option<String>,
}

impl RecordedRequest {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

struct StubState {
    scripted: VecDeque<(u16, String)>,
    fallback: (u16, String),
    requests: Vec<RecordedRequest>,
}

/// Answers requests from a script, in order; the last scripted response repeats
/// once the script runs out.
pub struct StubHttp {
    addr: SocketAddr,
    state: Arc<Mutex<StubState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for StubHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl StubHttp {
    pub async fn start(responses: Vec<(u16, &str)>) -> Self {
        let fallback = responses
            .last()
            .map(|(status, body)| (*status, body.to_string()))
            .unwrap_or((200, "{}".to_string()));

        let state = Arc::new(Mutex::new(StubState {
            scripted: responses
                .into_iter()
                .map(|(status, body)| (status, body.to_string()))
                .collect(),
            fallback,
            requests: Vec::new(),
        }));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let task = {
            let state = state.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let state = state.clone();
                    tokio::spawn(async move {
                        let Some(request) = read_request(&mut socket).await else {
                            return;
                        };
                        let (status, body) = {
                            let mut state = state.lock().await;
                            state.requests.push(request);
                            state
                                .scripted
                                .pop_front()
                                .unwrap_or_else(|| state.fallback.clone())
                        };
                        let response = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    });
                }
            })
        };

        Self { addr, state, task }
    }

    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().await.requests.clone()
    }

    pub async fn request_count(&self) -> usize {
        self.state.lock().await.requests.len()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    let header_end = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
    };

    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = headers.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut content_length = 0usize;
    let mut authorization = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse().unwrap_or(0),
            "authorization" => authorization = Some(value.trim().to_string()),
            _ => {}
        }
    }

    while buffer.len() < header_end + content_length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    Some(RecordedRequest {
        method,
        path,
        body: String::from_utf8_lossy(&buffer[header_end..]).to_string(),
        authorization,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// --- scripted chat completions ----------------------------------------------

/// A completion that asks for one tool call.
pub fn tool_call_response(id: &str, name: &str, arguments: &str) -> String {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }]
            }
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    })
    .to_string()
}

/// A completion that answers in words.
pub fn text_response(text: &str) -> String {
    json!({
        "choices": [{"message": {"role": "assistant", "content": text}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    })
    .to_string()
}

// --- a scriptable tool box ---------------------------------------------------

pub struct ScriptedTools {
    catalogue: Vec<ToolDef>,
    results: HashMap<String, Value>,
    calls: Mutex<Vec<(String, Value)>>,
    /// Tracks the loop registers through the context, as `publish_track` would.
    publishes: bool,
}

impl ScriptedTools {
    pub fn new() -> Self {
        Self {
            catalogue: vec![
                ToolDef::new(
                    "process_audio",
                    "process audio",
                    json!({"type": "object", "properties": {"search": {"type": "string"}}}),
                ),
                ToolDef::new(
                    "publish_track",
                    "publish",
                    json!({"type": "object", "properties": {"path": {"type": "string"}}}),
                ),
            ],
            results: HashMap::new(),
            calls: Mutex::new(Vec::new()),
            publishes: false,
        }
    }

    pub fn returning(mut self, tool: &str, result: Value) -> Self {
        self.results.insert(tool.to_string(), result);
        self
    }

    pub async fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().await.clone()
    }
}

impl Default for ScriptedTools {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolBox for ScriptedTools {
    fn catalogue(&self) -> &[ToolDef] {
        &self.catalogue
    }

    async fn call(&self, _ctx: &TurnCtx, name: &str, arguments: Value) -> Value {
        self.calls
            .lock()
            .await
            .push((name.to_string(), arguments.clone()));
        self.results
            .get(name)
            .cloned()
            .unwrap_or_else(|| json!({"ok": true}))
    }
}
