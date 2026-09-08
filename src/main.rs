//! Startup checks, wiring and shutdown.
//!
//! Everything that can be wrong with a deployment is checked here, before the
//! first poll, and reported with the variable or binary at fault (§10.3).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use wavo::config::Config;
use wavo::health::{self, Health};
use wavo::jobs::{JobManager, ORPHAN_MAX_AGE};
use wavo::llm::openai::OpenAi;
use wavo::llm::prompt::SYSTEM_PROMPT;
use wavo::llm::Agent;
use wavo::mcp::McpClient;
use wavo::session::Sessions;
use wavo::telegram::api::Telegram;
use wavo::telegram::Bot;
use wavo::tools::plainsong::PlainsongClient;
use wavo::tools::{ToolBox, Tools};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("wavo=info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // One line, naming what is wrong. Secrets never reach an error value.
            tracing::error!("wavo cannot start: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let config = Arc::new(Config::from_env()?);
    tracing::info!(
        model = %config.openai_model,
        fallback = %config.openai_model_fallback,
        chats = config.allowed_chat_ids.len(),
        work_dir = %config.work_dir.display(),
        "starting wavo"
    );

    check_work_dir(&config.work_dir).await?;
    check_binaries(&config)?;
    report_downloader().await;

    let telegram = Telegram::new(&config.telegram_api_base, &config.telegram_token);
    let username = telegram
        .get_me()
        .await
        .map_err(|e| anyhow::anyhow!("TELEGRAM_BOT_TOKEN was not accepted by Telegram: {e}"))?;
    tracing::info!(bot = %username, "telegram token accepted");

    let plainsong = Arc::new(PlainsongClient::new(
        config.plainsong_url.clone(),
        config.plainsong_public_url.clone(),
        config.plainsong_token.clone(),
        config.max_upload_bytes(),
    ));
    plainsong.list(None).await.map_err(|e| {
        anyhow::anyhow!(
            "plainsong is not reachable at PLAINSONG_URL={}: {e}",
            config.plainsong_url
        )
    })?;
    plainsong
        .probe_token()
        .await
        .map_err(|e| anyhow::anyhow!("PLAINSONG_TOKEN was not accepted: {e}"))?;
    tracing::info!(url = %config.plainsong_url, "plainsong reachable and token accepted");

    let mcp = Arc::new(
        McpClient::connect(&config.mcp_command, &config.work_dir)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    );
    let discovered = mcp.tools().await;
    if !discovered.iter().any(|tool| tool.name == "process_audio") {
        anyhow::bail!(
            "the MCP server `{}` does not expose `process_audio`; wavo has nothing to process audio with",
            config.mcp_command
        );
    }

    let jobs = Arc::new(JobManager::new(
        config.jobs_dir(),
        config.max_concurrent_jobs,
        config.keep_job_files,
    ));
    let swept = jobs.sweep_orphans(ORPHAN_MAX_AGE);
    if swept > 0 {
        tracing::info!(swept, "removed orphaned job directories");
    }

    let tools = Arc::new(Tools::new(mcp.clone(), plainsong.clone(), jobs.clone(), &config).await);
    tracing::info!(
        tools = %tools.catalogue().iter().map(|t| t.name()).collect::<Vec<_>>().join(", "),
        "tool catalogue ready"
    );

    let llm = Arc::new(OpenAi::new(
        config.openai_base_url.clone(),
        config.openai_api_key.clone(),
        config.llm_timeout,
    ));
    let agent = Arc::new(Agent::new(
        llm,
        tools,
        config.openai_model.clone(),
        config.openai_model_fallback.clone(),
        config.max_tool_iterations,
        config.turn_timeout,
    ));
    let sessions = Arc::new(Sessions::new(
        SYSTEM_PROMPT,
        config.history_turns,
        config.session_ttl,
    ));

    let shutdown = CancellationToken::new();
    let health_state = Arc::new(Health::new(mcp.clone()));
    health_state.note_plainsong_ok().await;

    let prober = health::spawn_prober(health_state.clone(), plainsong.clone(), shutdown.clone());
    let health_addr = config.health_addr.clone();
    let health_server = {
        let health_state = health_state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = health::serve(&health_addr, health_state, shutdown).await {
                tracing::error!(error = %e, "health endpoint stopped");
            }
        })
    };

    let bot = Arc::new(Bot {
        config: config.clone(),
        telegram,
        sessions: sessions.clone(),
        inbox: Arc::new(wavo::telegram::collect::Inbox::new()),
        agent,
        jobs,
        plainsong,
        started: Instant::now(),
    });

    let polling = {
        let bot = bot.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move { bot.run(shutdown).await })
    };

    wait_for_signal().await;
    tracing::info!("shutting down");
    shutdown.cancel();
    let _ = polling.await;

    // Let in-flight turns finish before the MCP child is closed under them (§8.3).
    let deadline = Instant::now() + config.shutdown_grace;
    while sessions.busy_count().await > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let stragglers = sessions.busy_count().await;
    if stragglers > 0 {
        tracing::warn!(
            stragglers,
            "shutdown grace expired with turns still running"
        );
    }

    mcp.shutdown().await;
    prober.abort();
    health_server.abort();
    tracing::info!("stopped");
    Ok(())
}

async fn check_work_dir(work_dir: &Path) -> anyhow::Result<()> {
    let jobs = work_dir.join("jobs");
    tokio::fs::create_dir_all(&jobs)
        .await
        .map_err(|e| anyhow::anyhow!("WAVO_WORK_DIR={} is not usable: {e}", work_dir.display()))?;

    let probe = jobs.join(".writable");
    tokio::fs::write(&probe, b"wavo").await.map_err(|e| {
        anyhow::anyhow!("WAVO_WORK_DIR={} is not writable: {e}", work_dir.display())
    })?;
    let _ = tokio::fs::remove_file(&probe).await;
    Ok(())
}

/// demix, its MCP server and the two binaries they shell out to have to be
/// present before the first request, not discovered halfway through one.
fn check_binaries(config: &Config) -> anyhow::Result<()> {
    for binary in [config.mcp_command.as_str(), "demix", "ffmpeg", "yt-dlp"] {
        if which(binary).is_none() {
            anyhow::bail!("`{binary}` was not found on PATH");
        }
    }
    Ok(())
}

/// Say which yt-dlp is installed and what demix will pass it, once, at startup.
///
/// Neither is fatal, and neither is visible anywhere else: the version is baked
/// into the image (the Dockerfile's `YT_DLP_VERSION`) and `DEMIX_YT_DLP_ARGS` is
/// read by demix, not by wavo. But between them they answer nearly every report
/// of "YouTube blocked the download" (§8.2) — a stale yt-dlp, or a cookies file
/// that the container cannot read — and guessing at those from the outside
/// costs a deployment round trip.
async fn report_downloader() {
    match tokio::process::Command::new("yt-dlp")
        .arg("--version")
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            tracing::info!(version = %version, "yt-dlp");
        }
        // `which` already found it, so this is a broken install, not a missing
        // one; downloads will fail, but everything else still works.
        Ok(output) => tracing::warn!(
            status = %output.status,
            "`yt-dlp --version` failed; YouTube downloads are unlikely to work"
        ),
        Err(e) => tracing::warn!(error = %e, "cannot run `yt-dlp --version`"),
    }

    let args = std::env::var("DEMIX_YT_DLP_ARGS").unwrap_or_default();
    if args.trim().is_empty() {
        return;
    }
    tracing::info!(args = %args, "demix will pass extra arguments to yt-dlp");
    for path in cookie_files(&args) {
        // The mount is the part that goes wrong: the container runs as uid
        // 10001, and a cookies file it cannot read is passed to yt-dlp all the
        // same, which then fails with the same 403 as if it had none. A
        // directory here is the usual mistake — a bind mount whose source does
        // not exist on the host makes Docker create one at both ends — and it
        // opens cleanly on Linux, so `is_file` is what settles it.
        let complaint = match std::fs::File::open(&path).and_then(|file| file.metadata()) {
            Ok(meta) if meta.is_file() => None,
            Ok(_) => Some(
                "is not a file (a bind mount with no source file makes Docker \
                           create a directory in its place)"
                    .to_string(),
            ),
            Err(e) => Some(e.to_string()),
        };
        match complaint {
            None => tracing::info!(path = %path, "yt-dlp cookies file is readable"),
            Some(complaint) => tracing::warn!(
                path = %path,
                error = %complaint,
                "DEMIX_YT_DLP_ARGS names a cookies file wavo cannot read; \
                 YouTube downloads will behave as if there were no cookies"
            ),
        }
    }
}

/// The cookies files named in a `DEMIX_YT_DLP_ARGS` value.
fn cookie_files(args: &str) -> Vec<String> {
    let words = split_args(args);
    let mut paths = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let path = match word.strip_prefix("--cookies") {
            Some("") => words.get(index + 1).map(String::as_str),
            // `--cookies-from-browser` lands here too, and is left alone: it
            // names a browser profile, and a container has no browser.
            Some(rest) => rest.strip_prefix('='),
            None => None,
        };
        match path {
            Some(path) if !path.is_empty() => paths.push(path.to_string()),
            _ => {}
        }
    }
    paths
}

/// Split a `DEMIX_YT_DLP_ARGS` value into words the way demix does. demix uses
/// `shlex.split`, so a quoted path holding spaces is one argument; splitting on
/// whitespace instead would have wavo warn about half a path that is fine.
fn split_args(args: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;

    for c in args.chars() {
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => word.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            None => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

fn which(binary: &str) -> Option<PathBuf> {
    if binary.contains('/') {
        let path = PathBuf::from(binary);
        return path.is_file().then_some(path);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(binary))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(e) => {
            tracing::warn!(error = %e, "cannot listen for SIGTERM");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("received SIGTERM"),
        _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT"),
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::{cookie_files, split_args};

    #[test]
    fn a_cookies_file_is_found_however_it_is_spelled() {
        assert_eq!(
            cookie_files("--cookies /work/cookies.txt"),
            ["/work/cookies.txt"]
        );
        assert_eq!(
            cookie_files("--cookies=/work/cookies.txt"),
            ["/work/cookies.txt"]
        );
        assert_eq!(
            cookie_files("-N 4 --cookies '/work/my cookies.txt'"),
            ["/work/my cookies.txt"]
        );
    }

    #[test]
    fn quoting_survives_the_split_as_it_does_in_demix() {
        assert_eq!(split_args("  -N   4 "), ["-N", "4"]);
        assert_eq!(
            split_args("--cookies \"/a b/c.txt\""),
            ["--cookies", "/a b/c.txt"]
        );
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn nothing_is_found_where_there_is_no_cookies_file() {
        assert!(cookie_files("").is_empty());
        // A browser jar is not a path, and inside a container there is no
        // browser to take one from anyway.
        assert!(cookie_files("--cookies-from-browser chrome").is_empty());
        // A trailing `--cookies` names nothing; yt-dlp rejects it itself.
        assert!(cookie_files("--cookies").is_empty());
    }
}
