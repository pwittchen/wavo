//! End to end against a real stack: demix behind its MCP server, and a running
//! plainsong. Ignored by default — it needs the spleeter models, ffmpeg and a
//! live plainsong, none of which belong in CI (§14).
//!
//! Run it from inside the wavo container, or on a host with demix installed:
//!
//! ```sh
//! docker compose up -d
//! docker compose exec wavo /bin/sh -c \
//!   'PLAINSONG_URL=http://plainsong:8080 PLAINSONG_TOKEN=$PLAINSONG_TOKEN \
//!    cargo test --test e2e_smoke -- --ignored --nocapture'
//! ```

use std::path::PathBuf;

use serde_json::{json, Map};
use wavo::config::Secret;
use wavo::mcp::McpClient;
use wavo::tools::plainsong::PlainsongClient;

fn env_or(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => default.to_string(),
    }
}

/// One second of silence as a 8 kHz mono 16-bit WAV, so the test needs no
/// network, no YouTube and no fixture file in the repository.
fn write_silence(path: &PathBuf) {
    const SAMPLE_RATE: u32 = 8000;
    const SAMPLES: u32 = SAMPLE_RATE;
    let data_len = SAMPLES * 2;

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend(std::iter::repeat_n(0u8, data_len as usize));

    std::fs::write(path, wav).unwrap();
}

#[tokio::test]
#[ignore = "needs a real demix install and a running plainsong"]
async fn a_local_file_is_processed_published_listed_and_playable() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("silence.wav");
    write_silence(&source);

    let plainsong_url = env_or("PLAINSONG_URL", "http://127.0.0.1:8080");
    let plainsong = PlainsongClient::new(
        plainsong_url.clone(),
        plainsong_url.clone(),
        Secret::new(std::env::var("PLAINSONG_TOKEN").expect("PLAINSONG_TOKEN must be set")),
        100 * 1024 * 1024,
    );

    let mcp = McpClient::connect(&env_or("WAVO_MCP_COMMAND", "demix-mcp"), work.path())
        .await
        .expect("demix-mcp must be on PATH");

    let job_dir = work.path().join("jobs/e2e");
    std::fs::create_dir_all(&job_dir).unwrap();

    let mut arguments = Map::new();
    arguments.insert("file".to_string(), json!(source.to_string_lossy()));
    arguments.insert("mode".to_string(), json!("nosplit"));
    arguments.insert("output_dir".to_string(), json!(job_dir.to_string_lossy()));
    arguments.insert("cwd".to_string(), json!(work.path().to_string_lossy()));

    let result = mcp.call("process_audio", arguments).await.unwrap();
    assert_eq!(result["ok"], true, "demix failed: {result}");

    let produced = result["files"]
        .as_object()
        .expect("demix reported no files")
        .iter()
        .find(|(relative, _)| relative.ends_with(".mp3"))
        .map(|(_, absolute)| PathBuf::from(absolute.as_str().unwrap()))
        .expect("demix produced no mp3");

    let track = plainsong
        .upload(&produced, "wavo e2e smoke test")
        .await
        .unwrap();

    let listed = plainsong.list(None).await.unwrap();
    assert!(
        listed.iter().any(|candidate| candidate.id == track.id),
        "the published track is not listed by GET /api/tracks"
    );

    let stream = reqwest::get(format!("{plainsong_url}/api/tracks/{}/stream", track.id))
        .await
        .unwrap();
    assert!(stream.status().is_success(), "the track does not stream");
    assert!(!stream.bytes().await.unwrap().is_empty());

    // Leave the store as we found it.
    plainsong.delete(&track.id).await.unwrap();
    mcp.shutdown().await;
}
