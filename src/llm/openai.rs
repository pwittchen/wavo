//! The three-field subset of OpenAI Chat Completions wavo actually uses, plus a
//! client with the retry policy of §8.1. Kept hand-rolled so `OPENAI_BASE_URL`
//! can point at any OpenAI-compatible server without fighting a wrapper crate.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Secret;
use crate::error::LlmError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// A JSON *string*, per the API — and not necessarily valid JSON, which is
    /// exactly the failure §8.1 has to survive.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: FunctionCall,
}

fn function_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn tool_calls(&self) -> &[ToolCall] {
        self.tool_calls.as_deref().unwrap_or(&[])
    }
}

/// An OpenAI function definition. MCP tool schemas convert into this
/// mechanically (`mcp::schema`), native tools build one by hand.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FunctionDef {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

impl ToolDef {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            kind: "function",
            function: FunctionDef {
                name: name.into(),
                description: Some(description.into()),
                parameters,
            },
        }
    }

    pub fn name(&self) -> &str {
        &self.function.name
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub message: ChatMessage,
    pub usage: Usage,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
}

pub struct OpenAi {
    http: reqwest::Client,
    base_url: String,
    api_key: Secret,
    timeout: Duration,
    /// `gpt-5*` rejects an explicit `temperature`. The spec asks for 0.2, so we
    /// send it, and on the one 400 that says otherwise we stop sending it for
    /// the rest of the process rather than failing every turn.
    send_temperature: AtomicBool,
}

impl OpenAi {
    pub fn new(base_url: String, api_key: Secret, timeout: Duration) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
            api_key,
            timeout,
            send_temperature: AtomicBool::new(true),
        }
    }

    /// One completion, with the retry policy of §8.1 applied to transport
    /// errors, `5xx` and `429`.
    pub async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        user: &str,
    ) -> Result<Completion, LlmError> {
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            match self.chat_once(model, messages, tools, user).await {
                Ok(completion) => return Ok(completion),

                // A rejected `temperature` is a permanent property of the model,
                // not a transient failure: drop it and retry immediately.
                Err(LlmError::Status { status: 400, body })
                    if body.contains("temperature")
                        && self.send_temperature.swap(false, Ordering::Relaxed) =>
                {
                    tracing::info!(
                        model,
                        "model rejected an explicit temperature; omitting it from now on"
                    );
                }

                Err(LlmError::Status { status: 429, body }) if attempt < 3 => {
                    let wait = retry_after(&body).unwrap_or(Duration::from_secs(5));
                    tracing::warn!(model, attempt, ?wait, "rate limited by the LLM provider");
                    tokio::time::sleep(wait).await;
                }

                Err(e @ LlmError::Transport(_)) | Err(e @ LlmError::Status { .. })
                    if attempt <= 2 && is_retryable(&e) =>
                {
                    let wait = if attempt == 1 {
                        Duration::from_secs(1)
                    } else {
                        Duration::from_secs(4)
                    };
                    tracing::warn!(model, attempt, error = %e, "retrying the LLM request");
                    tokio::time::sleep(wait).await;
                }

                Err(e) => return Err(e),
            }
        }
    }

    async fn chat_once(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        user: &str,
    ) -> Result<Completion, LlmError> {
        let mut body = json!({
            "model": model,
            "messages": messages,
            "max_completion_tokens": 1500,
            "user": user,
        });
        if self.send_temperature.load(Ordering::Relaxed) {
            body["temperature"] = json!(0.2);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
            body["parallel_tool_calls"] = json!(true);
        }

        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(self.api_key.expose())
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.without_url().to_string()))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| LlmError::Transport(e.without_url().to_string()))?;

        if !status.is_success() {
            return Err(LlmError::Status {
                status: status.as_u16(),
                // Provider error bodies are short and carry no credentials, but they
                // are still only ever logged, never forwarded to a user.
                body: text.chars().take(500).collect(),
            });
        }

        let parsed: ChatResponse =
            serde_json::from_str(&text).map_err(|e| LlmError::Decode(e.to_string()))?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or(LlmError::NoChoices)?;

        Ok(Completion {
            message: choice.message,
            usage: parsed.usage.unwrap_or_default(),
        })
    }
}

fn is_retryable(error: &LlmError) -> bool {
    match error {
        LlmError::Transport(_) => true,
        LlmError::Status { status, .. } => *status >= 500,
        _ => false,
    }
}

/// `Retry-After` as the provider reports it inside the error body. The header
/// itself is gone by the time we have the body, and every provider that sets one
/// repeats the delay in the message.
fn retry_after(body: &str) -> Option<Duration> {
    let marker = body.find("try again in")?;
    let rest = &body[marker + "try again in".len()..];
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let value: f64 = digits.parse().ok()?;
    let seconds = if rest.contains("ms") && !rest.contains("s.") {
        value / 1000.0
    } else {
        value
    };
    Some(Duration::from_millis((seconds * 1000.0).max(100.0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_messages_serialize_with_their_call_id() {
        let message = ChatMessage::tool_result("call_1", "{\"ok\":true}");
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_1");
        assert!(json.get("tool_calls").is_none());
    }

    #[test]
    fn plain_messages_omit_the_tool_fields_entirely() {
        let json = serde_json::to_value(ChatMessage::user("hi")).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hi");
        assert!(json.get("tool_call_id").is_none());
    }

    #[test]
    fn retry_after_is_read_out_of_the_provider_message() {
        assert_eq!(
            retry_after("Rate limit reached. Please try again in 12s."),
            Some(Duration::from_secs(12))
        );
        assert_eq!(retry_after("Rate limit reached."), None);
    }
}
