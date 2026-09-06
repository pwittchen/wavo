//! The three Bot API methods wavo needs, over long polling. No framework: the
//! surface is `getMe`, `getUpdates`, `sendMessage` and `editMessageText` (§2).
//!
//! The bot token is part of every request URL, so every `reqwest` error is
//! stripped of its URL before it becomes a `TelegramError` — otherwise the token
//! would end up in a log line the first time the network hiccups (§12).

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::config::Secret;
use crate::error::TelegramError;
use crate::telegram::format::truncate_message;

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T: serde::de::DeserializeOwned"))]
struct ApiResponse<T> {
    ok: bool,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct BotUser {
    #[serde(default)]
    username: Option<String>,
}

#[derive(Clone)]
pub struct Telegram {
    http: reqwest::Client,
    base: String,
}

impl Telegram {
    pub fn new(api_base: &str, token: &Secret) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: format!("{}/bot{}", api_base, token.expose()),
        }
    }

    pub async fn get_me(&self) -> Result<String, TelegramError> {
        let user: BotUser = self
            .call("getMe", json!({}), Duration::from_secs(15))
            .await?;
        Ok(user.username.unwrap_or_else(|| "unknown".to_string()))
    }

    /// One long poll. `timeout` is Telegram's own server-side wait; the HTTP
    /// timeout is deliberately longer so the poll is never cut short locally.
    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u64,
    ) -> Result<Vec<Update>, TelegramError> {
        let mut body = json!({
            "timeout": timeout_secs,
            "allowed_updates": ["message"],
        });
        if let Some(offset) = offset {
            body["offset"] = json!(offset);
        }
        self.call("getUpdates", body, Duration::from_secs(timeout_secs + 15))
            .await
    }

    /// Returns the id of the sent message, so it can be edited later.
    pub async fn send_message(&self, chat_id: i64, html: &str) -> Result<i64, TelegramError> {
        let message: Message = self
            .call(
                "sendMessage",
                json!({
                    "chat_id": chat_id,
                    "text": truncate_message(html),
                    "parse_mode": "HTML",
                    "disable_web_page_preview": true,
                }),
                Duration::from_secs(30),
            )
            .await?;
        Ok(message.message_id)
    }

    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        html: &str,
    ) -> Result<(), TelegramError> {
        // Telegram answers an unchanged edit with an error; it carries no
        // information for us, so it is swallowed rather than retried.
        match self
            .call::<serde_json::Value>(
                "editMessageText",
                json!({
                    "chat_id": chat_id,
                    "message_id": message_id,
                    "text": truncate_message(html),
                    "parse_mode": "HTML",
                    "disable_web_page_preview": true,
                }),
                Duration::from_secs(30),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(TelegramError::Api(message)) if message.contains("message is not modified") => {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: serde_json::Value,
        timeout: Duration,
    ) -> Result<T, TelegramError> {
        let response = self
            .http
            .post(format!("{}/{}", self.base, method))
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(scrub)?;

        let text = response.text().await.map_err(scrub)?;
        let parsed: ApiResponse<T> =
            serde_json::from_str(&text).map_err(|e| TelegramError::Decode(e.to_string()))?;

        if !parsed.ok {
            return Err(TelegramError::Api(
                parsed
                    .description
                    .unwrap_or_else(|| format!("{method} failed")),
            ));
        }
        parsed
            .result
            .ok_or_else(|| TelegramError::Decode(format!("{method} returned no result")))
    }
}

/// `reqwest` puts the request URL in its `Display` output, and ours contains the
/// bot token.
fn scrub(error: reqwest::Error) -> TelegramError {
    TelegramError::Transport(error.without_url().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_update_without_text_still_parses() {
        let update: Update = serde_json::from_str(
            r#"{"update_id":1,"message":{"message_id":9,"chat":{"id":42},"voice":{}}}"#,
        )
        .unwrap();
        let message = update.message.unwrap();
        assert_eq!(message.chat.id, 42);
        assert!(message.text.is_none());
    }

    #[test]
    fn a_non_message_update_parses_as_empty() {
        let update: Update =
            serde_json::from_str(r#"{"update_id":2,"edited_message":{"message_id":1}}"#).unwrap();
        assert!(update.message.is_none());
    }
}
