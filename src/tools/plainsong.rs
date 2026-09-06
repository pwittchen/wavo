//! The plainsong REST client. wavo is an ordinary API client: it uploads, lists
//! and (when explicitly enabled) deletes, and never touches plainsong's data
//! directory (§7).

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::config::Secret;
use crate::error::PlainsongError;

/// Extensions plainsong accepts; checked here so an obvious mismatch costs a
/// local check rather than a 100 MB upload and a 415.
const AUDIO_EXTENSIONS: [&str; 11] = [
    "mp3", "wav", "flac", "ogg", "oga", "opus", "m4a", "aac", "aiff", "aif", "wma",
];

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub size_bytes: u64,
}

pub struct PlainsongClient {
    http: reqwest::Client,
    api_base: String,
    public_base: String,
    token: Secret,
    max_upload_bytes: u64,
}

impl PlainsongClient {
    pub fn new(
        api_base: String,
        public_base: String,
        token: Secret,
        max_upload_bytes: u64,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_base,
            public_base,
            token,
            max_upload_bytes,
        }
    }

    pub fn track_url(&self, id: &str) -> String {
        format!("{}/track.html?id={}", self.public_base, id)
    }

    pub fn all_tracks_url(&self) -> String {
        format!("{}/", self.public_base)
    }

    pub fn max_upload_mb(&self) -> u64 {
        self.max_upload_bytes / 1024 / 1024
    }

    /// `GET /api/tracks` — public, and also the reachability probe (§10.3, §13).
    pub async fn list(&self, query: Option<&str>) -> Result<Vec<Track>, PlainsongError> {
        let mut request = self
            .http
            .get(format!("{}/api/tracks", self.api_base))
            .timeout(Duration::from_secs(15));
        if let Some(q) = query.map(str::trim).filter(|q| !q.is_empty()) {
            request = request.query(&[("q", q)]);
        }

        let response = request
            .send()
            .await
            .map_err(|e| PlainsongError::Transport(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(
                status.as_u16(),
                String::new(),
                self.max_upload_mb(),
            ));
        }
        response
            .json::<Vec<Track>>()
            .await
            .map_err(|e| PlainsongError::Decode(e.to_string()))
    }

    /// `POST /api/tracks` with the file and its title, retried once on a 5xx (§7).
    pub async fn upload(&self, path: &Path, title: &str) -> Result<Track, PlainsongError> {
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|e| PlainsongError::Io(format!("cannot read {}: {e}", path.display())))?;
        if metadata.len() > self.max_upload_bytes {
            return Err(PlainsongError::TooLarge(self.max_upload_mb()));
        }

        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "track.mp3".to_string());
        let extension = extension_of(&filename)
            .ok_or_else(|| PlainsongError::UnsupportedType(filename.clone()))?;

        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| PlainsongError::Io(format!("cannot read {}: {e}", path.display())))?;
        let title = sanitize_title(title, &filename);

        let mut attempt = 0;
        loop {
            attempt += 1;
            let part = reqwest::multipart::Part::bytes(bytes.clone())
                .file_name(filename.clone())
                .mime_str(mime_for(extension))
                .map_err(|e| PlainsongError::UnsupportedType(e.to_string()))?;
            let form = reqwest::multipart::Form::new()
                .text("title", title.clone())
                .part("file", part);

            let response = self
                .http
                .post(format!("{}/api/tracks", self.api_base))
                .bearer_auth(self.token.expose())
                .timeout(REQUEST_TIMEOUT)
                .multipart(form)
                .send()
                .await;

            match response {
                Ok(response) if response.status().is_success() => {
                    return response
                        .json::<Track>()
                        .await
                        .map_err(|e| PlainsongError::Decode(e.to_string()));
                }
                Ok(response) => {
                    let status = response.status().as_u16();
                    let body = response.text().await.unwrap_or_default();
                    if status >= 500 && attempt == 1 {
                        tracing::warn!(status, "plainsong upload failed; retrying once");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    return Err(status_error(status, body, self.max_upload_mb()));
                }
                Err(e) if attempt == 1 => {
                    tracing::warn!(error = %e, "plainsong upload failed; retrying once");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                Err(e) => return Err(PlainsongError::Transport(e.to_string())),
            }
        }
    }

    /// `DELETE /api/tracks/{id}` — only reachable when `WAVO_ALLOW_DELETE=true`.
    pub async fn delete(&self, id: &str) -> Result<(), PlainsongError> {
        let response = self
            .http
            .delete(format!("{}/api/tracks/{}", self.api_base, id))
            .bearer_auth(self.token.expose())
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| PlainsongError::Transport(e.to_string()))?;

        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(status_error(status, body, self.max_upload_mb()))
    }

    /// Startup probe for the token (§10.3): a `POST` with nothing attached runs
    /// the auth layer and then fails validation, so a `400` means "token good".
    /// Nothing is created and nothing is deleted.
    pub async fn probe_token(&self) -> Result<(), PlainsongError> {
        let response = self
            .http
            .post(format!("{}/api/tracks", self.api_base))
            .bearer_auth(self.token.expose())
            .timeout(Duration::from_secs(15))
            .multipart(reqwest::multipart::Form::new())
            .send()
            .await
            .map_err(|e| PlainsongError::Transport(e.to_string()))?;

        match response.status().as_u16() {
            401 | 403 => Err(PlainsongError::Unauthorized(response.status().as_u16())),
            _ => Ok(()),
        }
    }
}

fn status_error(status: u16, body: String, max_mb: u64) -> PlainsongError {
    match status {
        401 | 403 => PlainsongError::Unauthorized(status),
        413 => PlainsongError::TooLarge(max_mb),
        415 => PlainsongError::UnsupportedType(body),
        _ => PlainsongError::Server {
            status,
            body: body.chars().take(200).collect(),
        },
    }
}

fn extension_of(filename: &str) -> Option<&'static str> {
    let raw = filename.rsplit_once('.')?.1.to_lowercase();
    AUDIO_EXTENSIONS.iter().copied().find(|ext| *ext == raw)
}

pub fn is_audio_file(filename: &str) -> bool {
    extension_of(filename).is_some()
}

fn mime_for(extension: &str) -> &'static str {
    match extension {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "m4a" | "aac" => "audio/mp4",
        "aiff" | "aif" => "audio/aiff",
        "wma" => "audio/x-ms-wma",
        _ => "application/octet-stream",
    }
}

/// Titles come from the model, so they are trimmed, stripped of control
/// characters and capped before they are stored anywhere (§7).
pub fn sanitize_title(title: &str, filename: &str) -> String {
    let cleaned: String = title
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(200)
        .collect();

    if !cleaned.is_empty() {
        return cleaned;
    }
    match filename.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => filename.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_trimmed_stripped_and_capped() {
        assert_eq!(
            sanitize_title("  Bohemian\u{7} Rhapsody  ", "x.mp3"),
            "Bohemian Rhapsody"
        );
        assert_eq!(sanitize_title("", "song_vocals.mp3"), "song_vocals");
        assert_eq!(sanitize_title("   ", "song.mp3"), "song");
        assert_eq!(sanitize_title(&"a".repeat(500), "x.mp3").len(), 200);
    }

    #[test]
    fn only_audio_extensions_are_uploaded() {
        assert!(is_audio_file("vocals.mp3"));
        assert!(is_audio_file("VOCALS.WAV"));
        assert!(!is_audio_file("output.mkv"));
        assert!(!is_audio_file("passwd"));
    }

    #[test]
    fn statuses_map_onto_the_documented_errors() {
        assert!(matches!(
            status_error(401, String::new(), 100),
            PlainsongError::Unauthorized(401)
        ));
        assert!(matches!(
            status_error(413, String::new(), 100),
            PlainsongError::TooLarge(100)
        ));
        assert!(matches!(
            status_error(415, "nope".into(), 100),
            PlainsongError::UnsupportedType(_)
        ));
        assert!(matches!(
            status_error(502, "bad gateway".into(), 100),
            PlainsongError::Server { status: 502, .. }
        ));
    }

    #[test]
    fn links_follow_the_public_base_url() {
        let client = PlainsongClient::new(
            "http://plainsong:8080".into(),
            "https://music.example.com".into(),
            Secret::new("t"),
            100 * 1024 * 1024,
        );
        assert_eq!(
            client.track_url("7f1c"),
            "https://music.example.com/track.html?id=7f1c"
        );
        assert_eq!(client.all_tracks_url(), "https://music.example.com/");
        assert_eq!(client.max_upload_mb(), 100);
    }
}
