//! `GET /healthz`, used as the container health check (§13).
//!
//! A bare `tokio` listener answering one fixed shape. wavo serves exactly one
//! endpoint, and a web framework for one endpoint is more dependency than
//! feature (§2).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::mcp::McpClient;
use crate::tools::plainsong::PlainsongClient;

/// How recently plainsong must have answered for wavo to call itself healthy.
const PLAINSONG_FRESH_FOR: Duration = Duration::from_secs(60);

pub struct Health {
    mcp: Arc<McpClient>,
    plainsong_seen: Mutex<Option<Instant>>,
}

impl Health {
    pub fn new(mcp: Arc<McpClient>) -> Self {
        Self {
            mcp,
            plainsong_seen: Mutex::new(None),
        }
    }

    pub async fn note_plainsong_ok(&self) {
        *self.plainsong_seen.lock().await = Some(Instant::now());
    }

    async fn plainsong_up(&self) -> bool {
        self.plainsong_seen
            .lock()
            .await
            .is_some_and(|seen| seen.elapsed() < PLAINSONG_FRESH_FOR)
    }

    async fn body(&self) -> (u16, String) {
        let mcp = self.mcp.is_up();
        let plainsong = self.plainsong_up().await;
        let status = if mcp && plainsong { 200 } else { 503 };
        let body = format!(
            "{{\"ok\":{},\"mcp\":\"{}\",\"plainsong\":\"{}\"}}",
            mcp && plainsong,
            if mcp { "up" } else { "down" },
            if plainsong { "up" } else { "down" }
        );
        (status, body)
    }
}

/// Keep the plainsong side of the health answer fresh without waiting for a user
/// to ask for something.
pub fn spawn_prober(
    health: Arc<Health>,
    plainsong: Arc<PlainsongClient>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match plainsong.list(None).await {
                Ok(_) => health.note_plainsong_ok().await,
                Err(e) => tracing::debug!(error = %e, "plainsong health probe failed"),
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                _ = shutdown.cancelled() => return,
            }
        }
    })
}

pub async fn serve(
    addr: &str,
    health: Arc<Health>,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(addr, "health endpoint listening");

    loop {
        let (mut socket, _) = tokio::select! {
            accepted = listener.accept() => accepted?,
            _ = shutdown.cancelled() => return Ok(()),
        };
        let health = health.clone();

        tokio::spawn(async move {
            // Read just enough to see the request line; the body is irrelevant.
            let mut buffer = [0u8; 1024];
            let read = match socket.read(&mut buffer).await {
                Ok(read) => read,
                Err(_) => return,
            };
            let request = String::from_utf8_lossy(&buffer[..read]);
            let healthz = request.starts_with("GET /healthz");

            let (status, body) = if healthz {
                health.body().await
            } else {
                (404, "{\"error\":\"not found\"}".to_string())
            };
            let reason = if status == 200 {
                "OK"
            } else if status == 404 {
                "Not Found"
            } else {
                "Service Unavailable"
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
    }
}
