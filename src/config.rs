//! Configuration is environment variables only, parsed and validated once at
//! startup. A missing or malformed variable is a startup failure with a message
//! naming the variable — never a default that quietly does the wrong thing.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::error::ConfigError;

/// A value that must never reach a log line, a Telegram message or the LLM
/// context. The `Debug` impl is the enforcement point (§12).
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"<redacted>\"")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram_token: Secret,
    /// Overridable so tests can point the client at a stub server. Not part of
    /// the documented deployment surface.
    pub telegram_api_base: String,
    pub allowed_chat_ids: Vec<i64>,

    pub openai_api_key: Secret,
    pub openai_base_url: String,
    pub openai_model: String,
    pub openai_model_fallback: String,

    pub plainsong_url: String,
    pub plainsong_public_url: String,
    pub plainsong_token: Secret,
    pub plainsong_max_upload_mb: u64,

    pub work_dir: PathBuf,
    pub mcp_command: String,
    pub max_concurrent_jobs: usize,
    pub max_tool_iterations: usize,
    pub turn_timeout: Duration,
    pub job_timeout: Duration,
    pub llm_timeout: Duration,
    pub history_turns: usize,
    pub session_ttl: Duration,
    /// How long half a request waits for its other half (§5.6). Zero starts
    /// every message on its own, as wavo did before coalescing existed.
    pub coalesce_window: Duration,
    pub tool_output_chars: usize,
    pub progress_interval: Duration,
    pub keep_job_files: bool,
    pub expose_clean: bool,
    pub allow_delete: bool,
    pub health_addr: String,
    pub shutdown_grace: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let plainsong_url = trim_trailing_slash(&string("PLAINSONG_URL", "http://plainsong:8080"));
        let plainsong_public_url = match var("PLAINSONG_PUBLIC_URL") {
            Some(value) => trim_trailing_slash(&value),
            None => plainsong_url.clone(),
        };

        Ok(Self {
            telegram_token: Secret::new(required("TELEGRAM_BOT_TOKEN")?),
            telegram_api_base: trim_trailing_slash(&string(
                "TELEGRAM_API_BASE",
                "https://api.telegram.org",
            )),
            allowed_chat_ids: chat_ids("WAVO_ALLOWED_CHAT_IDS")?,

            openai_api_key: Secret::new(required("OPENAI_API_KEY")?),
            openai_base_url: trim_trailing_slash(&string(
                "OPENAI_BASE_URL",
                "https://api.openai.com/v1",
            )),
            openai_model: string("OPENAI_MODEL", "gpt-5-mini"),
            openai_model_fallback: string("OPENAI_MODEL_FALLBACK", "gpt-5"),

            plainsong_url,
            plainsong_public_url,
            plainsong_token: Secret::new(required("PLAINSONG_TOKEN")?),
            plainsong_max_upload_mb: parse("PLAINSONG_MAX_UPLOAD_MB", 100)?,

            work_dir: PathBuf::from(string("WAVO_WORK_DIR", "/work")),
            mcp_command: string("WAVO_MCP_COMMAND", "demix-mcp"),
            max_concurrent_jobs: positive("WAVO_MAX_CONCURRENT_JOBS", 1)?,
            max_tool_iterations: positive("WAVO_MAX_TOOL_ITERATIONS", 8)?,
            turn_timeout: seconds("WAVO_TURN_TIMEOUT_SEC", 1800)?,
            job_timeout: seconds("WAVO_JOB_TIMEOUT_SEC", 1200)?,
            llm_timeout: seconds("WAVO_LLM_TIMEOUT_SEC", 90)?,
            history_turns: positive("WAVO_HISTORY_TURNS", 12)?,
            session_ttl: minutes("WAVO_SESSION_TTL_MIN", 120)?,
            coalesce_window: seconds("WAVO_COALESCE_WINDOW_SEC", 15)?,
            tool_output_chars: positive("WAVO_TOOL_OUTPUT_CHARS", 2000)?,
            progress_interval: seconds("WAVO_PROGRESS_INTERVAL_SEC", 5)?,
            keep_job_files: boolean("WAVO_KEEP_JOB_FILES", false)?,
            expose_clean: boolean("WAVO_EXPOSE_CLEAN", false)?,
            allow_delete: boolean("WAVO_ALLOW_DELETE", false)?,
            health_addr: string("WAVO_HEALTH_ADDR", "0.0.0.0:8081"),
            shutdown_grace: seconds("WAVO_SHUTDOWN_GRACE_SEC", 30)?,
        })
    }

    pub fn jobs_dir(&self) -> PathBuf {
        self.work_dir.join("jobs")
    }

    pub fn max_upload_bytes(&self) -> u64 {
        self.plainsong_max_upload_mb * 1024 * 1024
    }

    pub fn is_allowed(&self, chat_id: i64) -> bool {
        self.allowed_chat_ids.contains(&chat_id)
    }
}

fn var(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => None,
    }
}

fn required(key: &'static str) -> Result<String, ConfigError> {
    var(key).ok_or(ConfigError::Missing(key))
}

fn string(key: &str, default: &str) -> String {
    var(key).unwrap_or_else(|| default.to_string())
}

fn trim_trailing_slash(value: &str) -> String {
    value.trim_end_matches('/').to_string()
}

fn parse<T>(key: &'static str, default: T) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    match var(key) {
        None => Ok(default),
        Some(value) => value.parse().map_err(|e: T::Err| ConfigError::Invalid {
            var: key,
            value,
            reason: e.to_string(),
        }),
    }
}

fn positive(key: &'static str, default: usize) -> Result<usize, ConfigError> {
    let value: usize = parse(key, default)?;
    if value == 0 {
        return Err(ConfigError::Invalid {
            var: key,
            value: "0".to_string(),
            reason: "must be greater than zero".to_string(),
        });
    }
    Ok(value)
}

fn seconds(key: &'static str, default: u64) -> Result<Duration, ConfigError> {
    Ok(Duration::from_secs(parse(key, default)?))
}

fn minutes(key: &'static str, default: u64) -> Result<Duration, ConfigError> {
    Ok(Duration::from_secs(parse::<u64>(key, default)? * 60))
}

fn boolean(key: &'static str, default: bool) -> Result<bool, ConfigError> {
    match var(key) {
        None => Ok(default),
        Some(value) => parse_boolean(&value).ok_or(ConfigError::Invalid {
            var: key,
            value,
            reason: "expected true or false".to_string(),
        }),
    }
}

fn parse_boolean(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The allow-list is the only authentication wavo has, so an unset or unparseable
/// value refuses to start rather than defaulting to "everyone" (§5.3).
fn chat_ids(key: &'static str) -> Result<Vec<i64>, ConfigError> {
    let raw = required(key)?;
    let mut ids = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let id = part.parse::<i64>().map_err(|_| ConfigError::Invalid {
            var: key,
            value: part.to_string(),
            reason: "expected a comma-separated list of numeric chat IDs".to_string(),
        })?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err(ConfigError::Missing(key));
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_in_debug_output() {
        let secret = Secret::new("super-secret-token");
        assert_eq!(format!("{secret:?}"), "\"<redacted>\"");
        assert_eq!(format!("{secret}"), "<redacted>");
        assert!(!format!("{secret:?} {secret}").contains("super-secret"));
    }

    #[test]
    fn boolean_accepts_the_usual_spellings() {
        for raw in ["true", "TRUE", "1", "yes", "on"] {
            assert_eq!(parse_boolean(raw), Some(true), "{raw}");
        }
        for raw in ["false", "FALSE", "0", "no", "off"] {
            assert_eq!(parse_boolean(raw), Some(false), "{raw}");
        }
        assert_eq!(parse_boolean("maybe"), None);
    }

    #[test]
    fn urls_lose_their_trailing_slash_so_paths_join_cleanly() {
        assert_eq!(
            trim_trailing_slash("http://plainsong:8080/"),
            "http://plainsong:8080"
        );
        assert_eq!(
            trim_trailing_slash("http://plainsong:8080"),
            "http://plainsong:8080"
        );
    }
}
