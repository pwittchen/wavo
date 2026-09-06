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
