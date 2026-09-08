//! The MCP layer against a stub MCP server, including a child that exits
//! mid-call (§14). Nothing here needs demix, spleeter or a network.

use std::path::PathBuf;

use serde_json::{json, Map, Value};
use wavo::config::{Proxy, Secret};
use wavo::error::McpError;
use wavo::mcp::McpClient;

fn stub_server() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stub_mcp_server.py")
}

/// The stub runs under `python3`; without one there is nothing to test against.
fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

fn arguments(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect()
}

#[tokio::test]
async fn tools_are_discovered_at_startup() {
    if !python3_available() {
        eprintln!("skipping: python3 is not available");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let client = McpClient::connect(stub_server().to_str().unwrap(), work.path(), None)
        .await
        .unwrap();

    let tools = client.tools().await;
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(names.contains(&"process_audio"));
    assert!(client.is_up());

    let process_audio = tools
        .iter()
        .find(|tool| tool.name == "process_audio")
        .unwrap();
    assert_eq!(
        process_audio.input_schema["properties"]["mode"]["default"],
        "nosplit"
    );
    assert!(process_audio.description.is_some());

    client.shutdown().await;
}

#[tokio::test]
async fn a_call_returns_the_tools_own_json() {
    if !python3_available() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let client = McpClient::connect(stub_server().to_str().unwrap(), work.path(), None)
        .await
        .unwrap();

    let value = client
        .call(
            "process_audio",
            arguments(&[
                ("search", json!("Queen - Bohemian Rhapsody")),
                ("mode", json!("2stems")),
                ("output_dir", json!("/work/jobs/abc")),
                ("cwd", json!(work.path().to_string_lossy())),
            ]),
        )
        .await
        .unwrap();

    assert_eq!(value["ok"], true);
    assert_eq!(
        value["files"]["music/mp3/song_vocals.mp3"],
        "/work/jobs/abc/music/mp3/song_vocals.mp3"
    );
    // The injected arguments really reach the server.
    assert_eq!(value["cwd_seen"], work.path().to_string_lossy().to_string());

    client.shutdown().await;
}

/// The proxy is only useful if the process that reaches YouTube can see it, so
/// this asks the child what it was started with (§9).
#[tokio::test]
async fn a_configured_proxy_reaches_the_child() {
    if !python3_available() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let proxy = Proxy::new(
        "127.0.0.1".to_string(),
        12321,
        Some("wavo".to_string()),
        Some(Secret::new("pass word")),
    )
    .unwrap();
    let client = McpClient::connect(stub_server().to_str().unwrap(), work.path(), Some(proxy))
        .await
        .unwrap();

    let value = client.call("env", Map::new()).await.unwrap();
    for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        assert_eq!(
            value["env"][key], "http://wavo:pass%20word@127.0.0.1:12321",
            "{key}"
        );
    }
    // The secrets that are not the child's business still stay behind.
    assert_eq!(value["env"]["TELEGRAM_BOT_TOKEN"], "");

    client.shutdown().await;
}

/// With the proxy off, the child's environment is wavo's own — whatever the
/// host set, unchanged.
#[tokio::test]
async fn no_proxy_configured_overrides_nothing() {
    if !python3_available() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let client = McpClient::connect(stub_server().to_str().unwrap(), work.path(), None)
        .await
        .unwrap();

    let value = client.call("env", Map::new()).await.unwrap();
    assert_eq!(
        value["env"]["HTTPS_PROXY"],
        std::env::var("HTTPS_PROXY").unwrap_or_default()
    );

    client.shutdown().await;
}

#[tokio::test]
async fn a_child_that_dies_mid_call_is_restarted_for_the_next_one() {
    if !python3_available() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let client = McpClient::connect(stub_server().to_str().unwrap(), work.path(), None)
        .await
        .unwrap();

    // The server exits without answering; the call fails, but wavo stays up.
    assert!(client.call("die", Map::new()).await.is_err());

    // The next call transparently gets a fresh server (acceptance criterion 9).
    let value = client
        .call(
            "process_audio",
            arguments(&[
                ("search", json!("x")),
                ("output_dir", json!("/work/jobs/abc")),
            ]),
        )
        .await
        .unwrap();
    assert_eq!(value["ok"], true);

    client.shutdown().await;
}

#[tokio::test]
async fn a_server_that_keeps_dying_is_reported_as_unavailable() {
    if !python3_available() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let client = McpClient::connect(stub_server().to_str().unwrap(), work.path(), None)
        .await
        .unwrap();

    // Each failed call burns a restart; after three in an hour wavo stops trying
    // and says so, instead of respawning a broken server forever (§8.3).
    let mut unavailable = false;
    for _ in 0..5 {
        if let Err(McpError::Unavailable) = client.call("die", Map::new()).await {
            unavailable = true;
            break;
        }
    }
    assert!(unavailable, "the restart budget was never exhausted");
    assert!(!client.is_up());

    client.shutdown().await;
}
