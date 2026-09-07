//! The plainsong client against a stub returning the statuses §7 has to handle.

mod support;

use support::StubHttp;
use wavo::config::Secret;
use wavo::error::PlainsongError;
use wavo::tools::plainsong::PlainsongClient;

const MAX_UPLOAD: u64 = 10 * 1024 * 1024;

fn client(stub: &StubHttp) -> PlainsongClient {
    PlainsongClient::new(
        stub.base(),
        "https://music.example.com".to_string(),
        Secret::new("plainsong-token"),
        MAX_UPLOAD,
    )
}

fn audio_file(dir: &tempfile::TempDir, name: &str, size: usize) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, vec![0u8; size]).unwrap();
    path
}

#[tokio::test]
async fn a_successful_upload_returns_the_track_and_carries_the_token() {
    let stub = StubHttp::start(vec![(
        201,
        r#"{"id":"7f1c","title":"Vocals","filename":"song_vocals.mp3","size_bytes":9,"content_type":"audio/mpeg","uploaded_at":"2026-09-06T10:00:00Z"}"#,
    )])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let file = audio_file(&dir, "song_vocals.mp3", 9);

    let track = client(&stub).upload(&file, "Vocals").await.unwrap();
    assert_eq!(track.id, "7f1c");

    let requests = stub.requests().await;
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/api/tracks");
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer plainsong-token")
    );
    assert!(requests[0].body.contains("name=\"title\""));
    // The stored name is unique per upload, not demix's `song_vocals.mp3`, which
    // every run of the same stem would produce again (§7).
    assert!(!requests[0].body.contains("filename=\"song_vocals.mp3\""));
    let stored = stored_filename(&requests[0].body);
    let (id, rest) = stored.split_once('_').unwrap();
    assert!(uuid::Uuid::parse_str(id).is_ok(), "{stored}");
    assert_eq!(rest, "Vocals.mp3");
}

/// The `filename=` of the `file` part of a multipart body.
fn stored_filename(body: &str) -> String {
    body.split("filename=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn a_file_over_the_limit_never_leaves_the_machine() {
    let stub = StubHttp::start(vec![(201, "{}")]).await;
    let dir = tempfile::tempdir().unwrap();
    let file = audio_file(&dir, "long.wav", (MAX_UPLOAD + 1) as usize);

    let error = client(&stub).upload(&file, "Too long").await.unwrap_err();
    assert!(matches!(error, PlainsongError::TooLarge(10)));
    assert_eq!(
        stub.request_count().await,
        0,
        "an oversized file was uploaded anyway"
    );
}

#[tokio::test]
async fn a_rejected_token_is_reported_as_such() {
    let stub = StubHttp::start(vec![(401, r#"{"error":"missing Authorization header"}"#)]).await;
    let dir = tempfile::tempdir().unwrap();
    let file = audio_file(&dir, "song.mp3", 9);

    let error = client(&stub).upload(&file, "Song").await.unwrap_err();
    assert!(matches!(error, PlainsongError::Unauthorized(401)));
    // The message the operator sees says nothing about the token's value.
    assert!(!error.to_string().contains("plainsong-token"));
}

#[tokio::test]
async fn the_server_side_size_limit_maps_onto_the_same_message() {
    let stub = StubHttp::start(vec![(413, r#"{"error":"file is larger than the limit"}"#)]).await;
    let dir = tempfile::tempdir().unwrap();
    let file = audio_file(&dir, "song.mp3", 9);

    let error = client(&stub).upload(&file, "Song").await.unwrap_err();
    assert!(matches!(error, PlainsongError::TooLarge(10)));
}

#[tokio::test]
async fn an_unsupported_type_is_distinguished_from_a_server_failure() {
    let stub = StubHttp::start(vec![(415, r#"{"error":"unsupported content type"}"#)]).await;
    let dir = tempfile::tempdir().unwrap();
    let file = audio_file(&dir, "song.mp3", 9);

    let error = client(&stub).upload(&file, "Song").await.unwrap_err();
    assert!(matches!(error, PlainsongError::UnsupportedType(_)));
}

#[tokio::test]
async fn a_server_error_is_retried_exactly_once() {
    let stub = StubHttp::start(vec![
        (503, r#"{"error":"restarting"}"#),
        (
            201,
            r#"{"id":"7f1c","title":"Vocals","filename":"a.mp3","size_bytes":9}"#,
        ),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let file = audio_file(&dir, "song.mp3", 9);

    let track = client(&stub).upload(&file, "Vocals").await.unwrap();
    assert_eq!(track.id, "7f1c");
    assert_eq!(stub.request_count().await, 2);
}

#[tokio::test]
async fn a_server_that_stays_broken_gives_up_after_the_retry() {
    let stub = StubHttp::start(vec![(500, r#"{"error":"still broken"}"#)]).await;
    let dir = tempfile::tempdir().unwrap();
    let file = audio_file(&dir, "song.mp3", 9);

    let error = client(&stub).upload(&file, "Vocals").await.unwrap_err();
    assert!(matches!(error, PlainsongError::Server { status: 500, .. }));
    assert_eq!(stub.request_count().await, 2);
}

#[tokio::test]
async fn listing_passes_the_query_through_and_needs_no_token() {
    let stub = StubHttp::start(vec![(
        200,
        r#"[{"id":"a","title":"One","filename":"one.mp3","size_bytes":1},{"id":"b","title":"Two","filename":"two.mp3","size_bytes":2}]"#,
    )])
    .await;

    let tracks = client(&stub).list(Some("queen")).await.unwrap();
    assert_eq!(tracks.len(), 2);

    let requests = stub.requests().await;
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/api/tracks?q=queen");
}

#[tokio::test]
async fn the_token_probe_accepts_anything_that_is_not_a_rejection() {
    // plainsong answers a POST with no file with 400 — which means the auth layer
    // let it through, which is exactly what the probe is checking.
    let stub = StubHttp::start(vec![(400, r#"{"error":"missing `file` part"}"#)]).await;
    assert!(client(&stub).probe_token().await.is_ok());

    let stub = StubHttp::start(vec![(403, r#"{"error":"invalid token"}"#)]).await;
    assert!(matches!(
        client(&stub).probe_token().await,
        Err(PlainsongError::Unauthorized(403))
    ));
}

#[tokio::test]
async fn deleting_reports_success_only_on_a_2xx() {
    let stub = StubHttp::start(vec![(204, "")]).await;
    assert!(client(&stub).delete("7f1c").await.is_ok());

    let stub = StubHttp::start(vec![(404, r#"{"error":"track not found"}"#)]).await;
    assert!(matches!(
        client(&stub).delete("nope").await,
        Err(PlainsongError::Server { status: 404, .. })
    ));
}
