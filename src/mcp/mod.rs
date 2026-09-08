//! The MCP client: one `demix-mcp` child process spoken to over stdio, its tool
//! catalogue discovered at startup, and the restart policy of §8.3.

pub mod schema;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use crate::config::{Proxy, Secret};
use crate::error::McpError;

/// How often a dead child may be restarted before wavo stops trying (§8.3).
const MAX_RESTARTS_PER_HOUR: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(3600);

/// A tool as the server described it, kept in wavo's own shape so the rest of
/// the code does not depend on `rmcp`'s types.
#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

type Service = RunningService<RoleClient, ()>;

pub struct McpClient {
    command: String,
    work_dir: PathBuf,
    proxy: Option<Proxy>,
    service: Mutex<Option<Arc<Service>>>,
    tools: Mutex<Vec<McpTool>>,
    restarts: Mutex<Vec<Instant>>,
    up: AtomicBool,
}

impl McpClient {
    /// Spawn `demix-mcp` and discover its tools. Failing here is a startup
    /// failure (§10.3) — wavo has nothing to offer without them.
    ///
    /// `proxy` is where an outbound proxy takes effect: the child is the only
    /// process that fetches from the open web, so it is the only one that gets
    /// one (§9).
    pub async fn connect(
        command: &str,
        work_dir: &Path,
        proxy: Option<Proxy>,
    ) -> Result<Self, McpError> {
        let client = Self {
            command: command.to_string(),
            work_dir: work_dir.to_path_buf(),
            proxy,
            service: Mutex::new(None),
            tools: Mutex::new(Vec::new()),
            restarts: Mutex::new(Vec::new()),
            up: AtomicBool::new(false),
        };
        client.start().await?;
        Ok(client)
    }

    pub fn is_up(&self) -> bool {
        self.up.load(Ordering::Relaxed)
    }

    pub async fn tools(&self) -> Vec<McpTool> {
        self.tools.lock().await.clone()
    }

    /// Call a tool, restarting the child once if it died in the meantime.
    ///
    /// The returned `Value` is the tool's own JSON result. A tool that reports
    /// failure through `{"ok": false, …}` is *not* an error here: §6.4 wants
    /// those handed back to the model, not raised.
    pub async fn call(&self, tool: &str, arguments: Map<String, Value>) -> Result<Value, McpError> {
        match self.call_once(tool, arguments.clone()).await {
            Ok(value) => Ok(value),
            Err(first) => {
                tracing::warn!(tool, error = %first, "MCP call failed; restarting the server");
                self.up.store(false, Ordering::Relaxed);
                {
                    let mut service = self.service.lock().await;
                    *service = None;
                }
                self.start().await?;
                self.call_once(tool, arguments).await
            }
        }
    }

    async fn call_once(
        &self,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<Value, McpError> {
        let service = self.service().await?;
        let mut params = CallToolRequestParams::new(tool.to_string());
        params.arguments = Some(arguments);

        let result = service
            .call_tool(params)
            .await
            .map_err(|e| McpError::Call {
                tool: tool.to_string(),
                message: e.to_string(),
            })?;

        Ok(result_to_json(result))
    }

    async fn service(&self) -> Result<Arc<Service>, McpError> {
        {
            let service = self.service.lock().await;
            if let Some(running) = service.as_ref() {
                if !running.is_closed() {
                    return Ok(running.clone());
                }
            }
        }
        self.start().await?;
        let service = self.service.lock().await;
        service.clone().ok_or(McpError::Unavailable)
    }

    /// Spawn the child and hand it an `initialize` + `tools/list`. Restarts are
    /// rate limited so a server that dies on every call cannot become a fork bomb.
    async fn start(&self) -> Result<(), McpError> {
        {
            let mut restarts = self.restarts.lock().await;
            restarts.retain(|at| at.elapsed() < RESTART_WINDOW);
            if restarts.len() >= MAX_RESTARTS_PER_HOUR {
                tracing::error!(
                    command = %self.command,
                    "demix MCP server restarted {MAX_RESTARTS_PER_HOUR} times in the last hour; giving up until the window rolls over"
                );
                return Err(McpError::Unavailable);
            }
            restarts.push(Instant::now());
        }

        let mut command = tokio::process::Command::new(&self.command);
        // demix caches its spleeter models under the process's cwd, so the whole
        // point of pinning it to /work is that the cache survives restarts (§10.2).
        command.current_dir(&self.work_dir);
        // The child inherits wavo's environment, which is how `DEMIX_YT_DLP_ARGS`
        // reaches demix — but it has no use for the secrets, so they stay behind.
        for secret in ["TELEGRAM_BOT_TOKEN", "OPENAI_API_KEY", "PLAINSONG_TOKEN"] {
            command.env_remove(secret);
        }
        // The one secret that is meant to travel: the proxy credentials, which
        // are the child's to use and nobody else's. They go in the environment
        // rather than into `DEMIX_YT_DLP_ARGS` because an environment variable
        // is not part of any command line demix might echo into its stderr —
        // and demix's stderr is read back by wavo. yt-dlp (urllib) and the
        // pytubefix fallback (requests) both honour these four; the lower-case
        // spellings are the ones requests actually looks for.
        for (key, value) in proxy_env(self.proxy.as_ref()) {
            command.env(key, value.expose());
        }

        let transport = TokioChildProcess::new(command).map_err(|source| McpError::Spawn {
            command: self.command.clone(),
            source,
        })?;

        let service = ().serve(transport).await.map_err(|e| McpError::Initialize(e.to_string()))?;

        let discovered = service
            .list_all_tools()
            .await
            .map_err(|e| McpError::Initialize(e.to_string()))?;

        let tools: Vec<McpTool> = discovered
            .into_iter()
            .map(|tool| McpTool {
                name: tool.name.to_string(),
                description: tool.description.map(|d| d.to_string()),
                input_schema: Value::Object((*tool.input_schema).clone()),
            })
            .collect();

        tracing::info!(
            command = %self.command,
            tools = tools.len(),
            names = %tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "),
            "demix MCP server ready"
        );

        *self.tools.lock().await = tools;
        *self.service.lock().await = Some(Arc::new(service));
        self.up.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Close the child on the way out (§8.3).
    pub async fn shutdown(&self) {
        self.up.store(false, Ordering::Relaxed);
        if let Some(service) = self.service.lock().await.take() {
            service.cancellation_token().cancel();
        }
    }
}

/// The proxy environment the demix child is started with, empty when no proxy
/// is configured — in which case whatever `HTTP_PROXY` the host set is inherited
/// as it always was, since wavo has no business overriding it.
fn proxy_env(proxy: Option<&Proxy>) -> Vec<(&'static str, Secret)> {
    let Some(proxy) = proxy else {
        return Vec::new();
    };
    let url = proxy.url();
    ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"]
        .into_iter()
        .map(|key| (key, url.clone()))
        .collect()
}

/// MCP tools answer with content blocks; FastMCP puts the tool's dict in
/// `structuredContent` and repeats it as JSON text. Prefer the structured form,
/// fall back to parsing the text, and only then to wrapping it.
fn result_to_json(result: CallToolResult) -> Value {
    let text: String = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");

    to_json(
        result.structured_content,
        &text,
        result.is_error.unwrap_or(false),
    )
}

fn to_json(structured: Option<Value>, text: &str, is_error: bool) -> Value {
    if let Some(structured) = structured {
        return structured;
    }
    match serde_json::from_str::<Value>(text) {
        Ok(value) if value.is_object() => value,
        _ => json!({"ok": !is_error, "text": text}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_child_gets_the_proxy_under_every_spelling() {
        let proxy = Proxy::new(
            "geo.iproyal.com".to_string(),
            12321,
            Some("wavo".to_string()),
            Some(Secret::new("secret")),
        )
        .unwrap();

        let env = proxy_env(Some(&proxy));
        let keys: Vec<&str> = env.iter().map(|(key, _)| *key).collect();
        assert_eq!(
            keys,
            ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"]
        );
        assert!(env
            .iter()
            .all(|(_, url)| url.expose() == "http://wavo:secret@geo.iproyal.com:12321"));
    }

    #[test]
    fn no_proxy_configured_leaves_the_inherited_environment_alone() {
        assert!(proxy_env(None).is_empty());
    }

    #[test]
    fn a_structured_result_is_used_as_is() {
        let value = to_json(Some(json!({"ok": true, "files": {}})), "", false);
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn a_json_text_block_is_parsed() {
        let value = to_json(None, "{\"ok\": false, \"error\": \"nope\"}", true);
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"], "nope");
    }

    #[test]
    fn plain_text_is_wrapped_with_the_error_flag() {
        let value = to_json(None, "boom", true);
        assert_eq!(value["ok"], false);
        assert_eq!(value["text"], "boom");

        let ok = to_json(None, "just words", false);
        assert_eq!(ok["ok"], true);
    }
}
