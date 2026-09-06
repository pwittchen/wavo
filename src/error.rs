//! Typed domain errors. `anyhow` is used at the edges (startup, task bodies);
//! everything a caller has to *decide* on has a variant here.
//!
//! No variant ever carries a secret: Telegram's bot token lives in the request
//! URL, so `reqwest` errors from that client are stripped of their URL before
//! they get here (see `telegram::api`).

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is not set")]
    Missing(&'static str),
    #[error("{var} is invalid ({value:?}): {reason}")]
    Invalid {
        var: &'static str,
        value: String,
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum TelegramError {
    #[error("telegram request failed: {0}")]
    Transport(String),
    #[error("telegram API returned an error: {0}")]
    Api(String),
    #[error("telegram response could not be parsed: {0}")]
    Decode(String),
}

#[derive(Debug, thiserror::Error)]
pub enum PlainsongError {
    #[error("plainsong is unreachable: {0}")]
    Transport(String),
    /// 401/403 — the operator's token is wrong. Never shown to a user verbatim.
    #[error("plainsong rejected the token (HTTP {0})")]
    Unauthorized(u16),
    #[error("file is larger than the {0} MB upload limit")]
    TooLarge(u64),
    #[error("plainsong does not accept this file type: {0}")]
    UnsupportedType(String),
    #[error("plainsong returned HTTP {status}: {body}")]
    Server { status: u16, body: String },
    #[error("plainsong response could not be parsed: {0}")]
    Decode(String),
    #[error("{0}")]
    Io(String),
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("could not start the demix MCP server ({command}): {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
    #[error("MCP initialize failed: {0}")]
    Initialize(String),
    #[error("MCP call to `{tool}` failed: {message}")]
    Call { tool: String, message: String },
    #[error("the demix MCP server is unavailable (restarted too many times in the last hour)")]
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("LLM request failed: {0}")]
    Transport(String),
    #[error("LLM returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("LLM response could not be parsed: {0}")]
    Decode(String),
    #[error("LLM returned no choices")]
    NoChoices,
}

/// Reasons `publish_track` refuses a path before anything is opened (§12).
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("`{0}` is not one of the files produced during this conversation")]
    NotInTable(String),
    #[error("`{}` resolves outside the job directory", .0.display())]
    Escapes(PathBuf),
    #[error("`{0}` is not an audio file")]
    NotAudio(String),
    #[error("`{}` does not exist any more", .0.display())]
    Missing(PathBuf),
}
