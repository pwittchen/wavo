//! The song-name lookup as the child process it really is: wavo's arguments,
//! yt-dlp's JSON, the ways a lookup fails, and the proxy the child inherits —
//! all against a stub, because no test may reach YouTube (§14).

use std::path::PathBuf;

use wavo::config::{Proxy, Secret};
use wavo::tools::youtube::{SongNameLookup, YtDlp};

fn stub_yt_dlp() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/stub_yt_dlp.py")
        .to_string_lossy()
        .to_string()
}

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

#[tokio::test]
async fn a_videos_names_come_back_out_of_the_json() {
    if !python3_available() {
        return;
    }
    let names = YtDlp::new(stub_yt_dlp(), None)
        .lookup("https://youtu.be/fJ9rUzIMcZQ")
        .await
        .expect("the lookup came back empty");

    assert_eq!(names.title, "Queen - Bohemian Rhapsody (Official Video)");
    assert_eq!(names.artist.as_deref(), Some("Queen"));
    assert_eq!(names.track.as_deref(), Some("Bohemian Rhapsody"));
}

#[tokio::test]
async fn a_refused_or_unreadable_lookup_is_no_answer_rather_than_a_failure() {
    if !python3_available() {
        return;
    }
    let lookup = YtDlp::new(stub_yt_dlp(), None);

    assert_eq!(lookup.lookup("https://youtu.be/blocked").await, None);
    assert_eq!(lookup.lookup("https://youtu.be/garbage").await, None);

    // And a yt-dlp that is not installed at all is the same non-event.
    let missing = YtDlp::new("wavo-no-such-yt-dlp", None);
    assert_eq!(missing.lookup("https://youtu.be/fJ9rUzIMcZQ").await, None);
}

/// Asking what a video is called is a request to YouTube like any other, so it
/// goes out the way the downloads do (§9).
#[tokio::test]
async fn the_lookup_goes_through_the_configured_proxy() {
    if !python3_available() {
        return;
    }
    let proxy = Proxy::new(
        "geo.iproyal.com".to_string(),
        12321,
        Some("wavo".to_string()),
        Some(Secret::new("p@ss:word")),
    )
    .unwrap();

    let names = YtDlp::new(stub_yt_dlp(), Some(proxy))
        .lookup("https://youtu.be/proxy")
        .await
        .expect("the lookup came back empty");

    // The child saw the proxy, credentials percent-encoded the way yt-dlp
    // expects them.
    assert_eq!(
        names.title,
        "http://wavo:p%40ss%3Aword@geo.iproyal.com:12321"
    );

    // Without one, wavo sets nothing and the host's own environment stands —
    // whatever it is on the machine running the test.
    let inherited = std::env::var("HTTPS_PROXY").unwrap_or_else(|_| "direct".to_string());
    let names = YtDlp::new(stub_yt_dlp(), None)
        .lookup("https://youtu.be/proxy")
        .await
        .expect("the lookup came back empty");
    assert_eq!(names.title, inherited);
}
