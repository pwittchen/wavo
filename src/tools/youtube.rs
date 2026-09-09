//! What the song is called, asked of YouTube instead of guessed (§7).
//!
//! `process_audio` answers a link with files and nothing else: demix prints the
//! video's title only when it resolved a *search* itself, so a user who sends a
//! bare link leaves the model with nothing to name the track with, and
//! plainsong ends up storing "Unknown artist — song_vocals (original)". So when
//! the source is a YouTube URL, wavo asks yt-dlp — already required on `PATH`
//! (§10.3) — what the video is called, and hands the answer to the model as
//! part of the tool result.
//!
//! The names are facts for the model to compose a title from, not a title:
//! splitting "Queen - Bohemian Rhapsody (Official Video)" into a performer and
//! a song is the model's job, and the composed title is still wavo's (§7).

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::config::Proxy;
use crate::mcp::proxy_env;

/// How long the lookup may take before the turn goes on unnamed. It runs after
/// the download, so the video is known to be reachable; this only bounds a
/// connection that hangs.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest name that reaches the model. A title is a line, not a document, and
/// plainsong caps the composed one at 200 characters anyway (§7).
const MAX_NAME_CHARS: usize = 200;

/// The YouTube player clients the lookup asks through, in order, until one
/// answers.
///
/// yt-dlp's default client is not the one that works from this deployment:
/// YouTube answers it with `ERROR: [youtube] …: This video is not available`
/// for videos that play in a browser and that demix downloads without trouble,
/// because demix's working strategy asks through `tv_simply` (6f27376). A
/// lookup that only ever ran the default client therefore came back empty for
/// exactly the links users send, and the model — handed a result with no names
/// in it — composed a title out of nothing.
///
/// So the lookup asks the way the download that just succeeded asked, and keeps
/// yt-dlp's own choice behind it: which client YouTube is currently willing to
/// talk to is a thing that changes, and a second attempt costs one request on a
/// path that has already failed.
const PLAYER_CLIENTS: [&str; 2] = ["tv_simply", "default"];

/// What YouTube says a video is.
///
/// `title` is the video's own title and is always there. `artist` and `track`
/// come from the music metadata YouTube attaches to YouTube Music entries and
/// "- Topic" uploads, so they are the performer and the song already separated
/// — when they are there at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SongNames {
    pub title: String,
    pub artist: Option<String>,
    pub track: Option<String>,
}

/// The lookup, behind a trait so the tool layer can be exercised without
/// YouTube: no test in this repo may reach it (§14).
#[async_trait]
pub trait SongNameLookup: Send + Sync {
    /// The names behind a YouTube URL, or `None` when the lookup did not
    /// produce any. Never an error: an unnamed track is a worse title, not a
    /// failed turn.
    async fn lookup(&self, url: &str) -> Option<SongNames>;
}

/// Is this a link wavo can ask YouTube about? The host decides, not the text:
/// `https://example.com/?q=youtu.be` is not YouTube.
pub fn is_youtube_url(url: &str) -> bool {
    let lowered = url.trim().to_ascii_lowercase();
    let without_scheme = lowered
        .strip_prefix("https://")
        .or_else(|| lowered.strip_prefix("http://"))
        .unwrap_or(lowered.as_str());
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // Userinfo first (`user@host`), then the port.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);

    matches!(host, "youtu.be" | "youtube.com" | "youtube-nocookie.com")
        || host.ends_with(".youtube.com")
        || host.ends_with(".youtube-nocookie.com")
}

/// Read the names out of one `yt-dlp --dump-single-json` object. A video with
/// no title at all is no answer.
pub fn names_from_json(info: &Value) -> Option<SongNames> {
    Some(SongNames {
        title: text(info.get("title"))?,
        // `artists` is the newer spelling of the same metadata; either may be
        // the one this yt-dlp emits.
        artist: text(info.get("artist")).or_else(|| {
            info.get("artists")
                .and_then(Value::as_array)
                .and_then(|artists| artists.first())
                .and_then(|artist| text(Some(artist)))
        }),
        track: text(info.get("track")),
    })
}

fn text(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| clip(text))
}

/// Head of a name, never its middle: this is one line, and the marker
/// `truncate_middle` leaves in demix's output would read as part of the song.
fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_NAME_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_NAME_CHARS - 1).collect();
    out.push('…');
    out
}

/// The real lookup: a `yt-dlp` that downloads nothing, per player client until
/// one of them answers.
pub struct YtDlp {
    command: String,
    proxy: Option<Proxy>,
}

impl YtDlp {
    /// `proxy` is the same one demix downloads through (§9): a deployment whose
    /// IP YouTube distrusts is distrusted for metadata too, and asking about a
    /// video costs a few kilobytes of the operator's proxy traffic.
    pub fn new(command: impl Into<String>, proxy: Option<Proxy>) -> Self {
        Self {
            command: command.into(),
            proxy,
        }
    }

    /// One `yt-dlp` run, asked through one player client.
    async fn ask(&self, url: &str, player_client: &str) -> Option<SongNames> {
        let mut command = tokio::process::Command::new(&self.command);
        command
            .arg("--dump-single-json")
            .arg("--skip-download")
            .arg("--no-playlist")
            .arg("--no-warnings")
            .arg("--extractor-args")
            .arg(format!("youtube:player_client={player_client}"))
            // Everything after this is the URL, however it starts.
            .arg("--")
            .arg(url)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        // The same rule the demix child follows: the proxy credentials travel,
        // wavo's own secrets stay behind (§12).
        for secret in ["TELEGRAM_BOT_TOKEN", "OPENAI_API_KEY", "PLAINSONG_TOKEN"] {
            command.env_remove(secret);
        }
        for (key, value) in proxy_env(self.proxy.as_ref()) {
            command.env(key, value.expose());
        }

        let output = match tokio::time::timeout(LOOKUP_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                tracing::warn!(command = %self.command, error = %e, "could not run yt-dlp for the song's name");
                return None;
            }
            Err(_) => {
                tracing::warn!(
                    timeout = ?LOOKUP_TIMEOUT,
                    player_client,
                    "yt-dlp did not say what the video is called in time"
                );
                return None;
            }
        };

        if !output.status.success() {
            // Whatever YouTube refused with belongs in the log, not in a title
            // — and which client it refused is how the next block is told from
            // the last one.
            tracing::warn!(
                status = %output.status,
                player_client,
                reason = %last_line(&String::from_utf8_lossy(&output.stderr)),
                "yt-dlp could not say what the video is called"
            );
            return None;
        }

        let info: Value = match serde_json::from_slice(&output.stdout) {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(error = %e, player_client, "yt-dlp answered with something that is not JSON");
                return None;
            }
        };
        names_from_json(&info)
    }
}

#[async_trait]
impl SongNameLookup for YtDlp {
    async fn lookup(&self, url: &str) -> Option<SongNames> {
        for player_client in PLAYER_CLIENTS {
            if let Some(names) = self.ask(url, player_client).await {
                tracing::debug!(player_client, "YouTube said what the video is called");
                return Some(names);
            }
        }
        None
    }
}

/// yt-dlp's last word on a failure — the `ERROR:` line, with the traceback and
/// the warnings above it dropped.
fn last_line(stderr: &str) -> String {
    let line = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    clip(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_link_is_youtubes_only_when_its_host_is() {
        for url in [
            "https://www.youtube.com/watch?v=fJ9rUzIMcZQ",
            "https://youtu.be/fJ9rUzIMcZQ?t=42",
            "http://music.youtube.com/watch?v=fJ9rUzIMcZQ",
            "https://m.youtube.com/watch?v=fJ9rUzIMcZQ",
            "https://www.youtube-nocookie.com/embed/fJ9rUzIMcZQ",
            "  https://YouTube.com/watch?v=fJ9rUzIMcZQ  ",
        ] {
            assert!(is_youtube_url(url), "{url} was not recognised");
        }

        for url in [
            "https://example.com/?q=youtu.be",
            "https://youtu.be.example.com/watch?v=x",
            "https://vimeo.com/12345",
            "/work/jobs/abc/music.mp3",
            "",
        ] {
            assert!(!is_youtube_url(url), "{url} was taken for a YouTube link");
        }
    }

    #[test]
    fn music_metadata_arrives_as_a_performer_and_a_song() {
        let names = names_from_json(&json!({
            "title": "Bohemian Rhapsody (Official Video Remastered)",
            "artist": "Queen",
            "track": "Bohemian Rhapsody",
            "uploader": "Queen Official"
        }))
        .unwrap();

        assert_eq!(names.artist.as_deref(), Some("Queen"));
        assert_eq!(names.track.as_deref(), Some("Bohemian Rhapsody"));
        assert_eq!(names.title, "Bohemian Rhapsody (Official Video Remastered)");
    }

    #[test]
    fn a_video_without_music_metadata_still_gives_up_its_title() {
        let names = names_from_json(&json!({
            "title": "Kult - Arahja (Live)",
            "artist": "   ",
            "track": null,
            "uploader": "someone"
        }))
        .unwrap();

        assert_eq!(names.title, "Kult - Arahja (Live)");
        // The channel is not the performer, and a guess here would be stored as
        // one: the model splits the title instead.
        assert_eq!(names.artist, None);
        assert_eq!(names.track, None);

        // `artists` is the same metadata under yt-dlp's newer spelling.
        let names = names_from_json(&json!({"title": "x", "artists": ["Kult", "Kazik"]})).unwrap();
        assert_eq!(names.artist.as_deref(), Some("Kult"));
    }

    #[test]
    fn a_video_with_no_title_is_no_answer() {
        assert_eq!(names_from_json(&json!({"artist": "Queen"})), None);
        assert_eq!(names_from_json(&json!({"title": ""})), None);
    }

    #[test]
    fn a_name_long_enough_to_be_a_document_is_cut_down() {
        let names = names_from_json(&json!({"title": "x".repeat(1000)})).unwrap();
        assert_eq!(names.title.chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn the_logged_reason_is_yt_dlps_last_word() {
        let stderr = "WARNING: player = web\nERROR: [youtube] fJ9: Sign in to confirm\n\n";
        assert_eq!(
            last_line(stderr),
            "ERROR: [youtube] fJ9: Sign in to confirm"
        );
        assert_eq!(last_line("   \n\n"), "");
    }
}
