//! The dispatch table: demix's tools as discovered over MCP, plus wavo's own
//! publish/list/delete tools, behind one interface the LLM loop can call.
//!
//! Everything the model is allowed to influence passes through here, so this is
//! also where the two containment rules live: wavo picks the paths (§6.3), and
//! tool failures come back as results the model can read rather than as errors
//! that abort the turn (§6.4).

pub mod plainsong;
pub mod publish;
pub mod youtube;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::Config;
use crate::error::{PathError, PlainsongError};
use crate::jobs::{JobManager, Progress};
use crate::llm::openai::ToolDef;
use crate::mcp::{schema, McpClient};
use crate::session::PublishedTrack;
use crate::telegram::format::{t, t_detail, t_n, t_name, truncate_middle, Lang, Msg};
use crate::tools::plainsong::{is_audio_file, PlainsongClient};
use crate::tools::publish::{compose_track_title, resolve_publish_path};
use crate::tools::youtube::{is_youtube_url, SongNameLookup, YtDlp};

/// Everything one turn accumulates: the paths the model is allowed to name, the
/// tracks it published, and the directories to clean up afterwards.
#[derive(Debug, Default)]
struct TurnState {
    paths: HashMap<String, PathBuf>,
    published: Vec<PublishedTrack>,
    job_dirs: Vec<PathBuf>,
}

pub struct TurnCtx {
    pub chat_id: i64,
    pub turn_id: Uuid,
    pub lang: Lang,
    pub cancel: CancellationToken,
    pub progress: Option<Arc<Progress>>,
    state: Mutex<TurnState>,
}

impl TurnCtx {
    pub fn new(
        chat_id: i64,
        turn_id: Uuid,
        lang: Lang,
        cancel: CancellationToken,
        progress: Option<Arc<Progress>>,
    ) -> Self {
        Self {
            chat_id,
            turn_id,
            lang,
            cancel,
            progress,
            state: Mutex::new(TurnState::default()),
        }
    }

    pub async fn published(&self) -> Vec<PublishedTrack> {
        self.state.lock().await.published.clone()
    }

    /// Record a track published during this turn. The links block is built from
    /// these, by wavo, never by the model (§5.4).
    pub async fn record_published(&self, track: PublishedTrack) {
        self.state.lock().await.published.push(track);
    }

    pub async fn job_dirs(&self) -> Vec<PathBuf> {
        self.state.lock().await.job_dirs.clone()
    }

    async fn remember_path(&self, key: String, path: PathBuf) {
        self.state.lock().await.paths.insert(key, path);
    }

    async fn paths(&self) -> HashMap<String, PathBuf> {
        self.state.lock().await.paths.clone()
    }
}

/// The tool surface the LLM loop talks to. A trait so the loop can be tested
/// against scripted tools without demix, plainsong or a network (§14).
#[async_trait]
pub trait ToolBox: Send + Sync {
    /// The OpenAI `tools` array for this turn.
    fn catalogue(&self) -> &[ToolDef];

    /// Run a tool. Never returns an error: a failure is a JSON result the model
    /// can read and correct itself from (§6.4).
    async fn call(&self, ctx: &TurnCtx, name: &str, arguments: Value) -> Value;
}

pub struct Tools {
    mcp: Arc<McpClient>,
    plainsong: Arc<PlainsongClient>,
    jobs: Arc<JobManager>,
    catalogue: Vec<ToolDef>,
    /// The *original* MCP schemas, kept so wavo knows which tools take the
    /// arguments it injects.
    mcp_schemas: HashMap<String, Value>,
    /// Who to ask what a YouTube link is called (§7).
    song_names: Arc<dyn SongNameLookup>,
    tool_output_chars: usize,
    job_timeout: Duration,
    work_dir: PathBuf,
    allow_delete: bool,
}

impl Tools {
    pub async fn new(
        mcp: Arc<McpClient>,
        plainsong: Arc<PlainsongClient>,
        jobs: Arc<JobManager>,
        config: &Config,
    ) -> Self {
        let mut catalogue = Vec::new();
        let mut mcp_schemas = HashMap::new();

        for tool in mcp.tools().await {
            // Re-downloading a ~300 MB model is an operator decision, not a chat one.
            if tool.name == "clean" && !config.expose_clean {
                continue;
            }
            catalogue.push(schema::to_openai_tool(
                &tool.name,
                tool.description.as_deref(),
                &tool.input_schema,
            ));
            mcp_schemas.insert(tool.name.clone(), tool.input_schema);
        }

        catalogue.push(ToolDef::new(
            "publish_track",
            "Publish a processed audio file to the music storage and get its link. \
             `path` must be one of the paths listed in a previous tool result. The \
             three title parts are joined by wavo into \"Artist — Title (modification)\"; \
             do not join them yourself.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "A path exactly as it appeared in a previous tool result's `files` list."
                    },
                    "artist": {
                        "type": "string",
                        "description": "The performer alone, as written, never translated, e.g. \"Queen\"."
                    },
                    "title": {
                        "type": "string",
                        "description": "The song title alone, without the artist and without what was done to it, e.g. \"Bohemian Rhapsody\"."
                    },
                    "modification": {
                        "type": "string",
                        "description": "What was done to this file, in the language of the user's most recent message: \
                                        \"vocals removed\" / \"bez wokalu\", \"instrumental\" / \"podkład\", \
                                        \"slowed down to 80%\" / \"zwolnione do 80%\", \"transposed to A minor\" / \
                                        \"tonacja a-moll\". If nothing was changed, say so in that language \
                                        (\"original\" / \"oryginał\")."
                    }
                },
                "required": ["path", "artist", "title", "modification"]
            }),
        ));

        catalogue.push(ToolDef::new(
            "list_tracks",
            "List up to ten tracks already in the music storage, optionally filtered by a query.",
            json!({
                "type": "object",
                "properties": {
                    "q": {"type": "string", "description": "Optional case-insensitive search over titles."}
                }
            }),
        ));

        if config.allow_delete {
            catalogue.push(ToolDef::new(
                "delete_track",
                "Delete a track from the music storage by its id.",
                json!({
                    "type": "object",
                    "properties": {"id": {"type": "string"}},
                    "required": ["id"]
                }),
            ));
        }

        Self {
            mcp,
            plainsong,
            jobs,
            catalogue,
            mcp_schemas,
            song_names: Arc::new(YtDlp::new("yt-dlp", config.proxy.clone())),
            tool_output_chars: config.tool_output_chars,
            job_timeout: config.job_timeout,
            work_dir: config.work_dir.clone(),
            allow_delete: config.allow_delete,
        }
    }

    /// Ask something other than yt-dlp what a link is called. The tests are the
    /// caller: none of them may reach YouTube (§14).
    pub fn with_song_names(mut self, lookup: Arc<dyn SongNameLookup>) -> Self {
        self.song_names = lookup;
        self
    }

    async fn call_mcp(
        &self,
        ctx: &TurnCtx,
        name: &str,
        mut arguments: Map<String, Value>,
    ) -> Value {
        let Some(input_schema) = self.mcp_schemas.get(name) else {
            return error(t_name(Msg::ErrUnknownTool, ctx.lang, name));
        };

        // A `file` argument may only name something wavo produced earlier in this
        // turn — the same rule as `publish_track`, for the same reason.
        if let Some(Value::String(requested)) = arguments.get("file").cloned() {
            match resolve_publish_path(&ctx.paths().await, &requested, self.jobs.jobs_dir()) {
                Ok(path) => {
                    arguments.insert("file".to_string(), json!(path.to_string_lossy()));
                }
                Err(e) => {
                    tracing::warn!(chat_id = ctx.chat_id, tool = name, error = %e, "refused a path");
                    return error(t(Msg::ErrPathRefused, ctx.lang));
                }
            }
        }

        // wavo decides where files land, always (§12).
        let mut job_dir = None;
        if schema::accepts(input_schema, "output_dir") {
            let dir = if name == "clean" {
                self.jobs.jobs_dir().to_path_buf()
            } else {
                match self.jobs.new_job_dir() {
                    Ok(dir) => {
                        job_dir = Some(dir.clone());
                        ctx.state.lock().await.job_dirs.push(dir.clone());
                        dir
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "could not create a job directory");
                        return error(t(Msg::ErrToolUnavailable, ctx.lang));
                    }
                }
            };
            arguments.insert("output_dir".to_string(), json!(dir.to_string_lossy()));
        }
        if schema::accepts(input_schema, "cwd") {
            arguments.insert("cwd".to_string(), json!(self.work_dir.to_string_lossy()));
        }

        // Kept before the arguments are handed over: what the run was asked to
        // fetch is what wavo asks YouTube about afterwards (§7).
        let source_url = arguments
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string);

        let long_running = name == "process_audio";
        let _permit = if long_running {
            Some(self.jobs.acquire(ctx.progress.as_deref()).await)
        } else {
            None
        };

        if ctx.cancel.is_cancelled() {
            return error(t(Msg::ErrCancelled, ctx.lang));
        }

        let stages = if long_running {
            start_progress(ctx, &arguments)
        } else {
            Vec::new()
        };

        let result = tokio::time::timeout(self.job_timeout, self.mcp.call(name, arguments)).await;

        for handle in stages {
            handle.abort();
        }

        let value = match result {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => {
                tracing::error!(tool = name, error = %e, "MCP tool call failed");
                return error(t(Msg::ErrToolUnavailable, ctx.lang));
            }
            Err(_) => {
                tracing::warn!(tool = name, timeout = ?self.job_timeout, "MCP tool call timed out");
                return error(t(Msg::ErrJobTimeout, ctx.lang));
            }
        };

        let mut reduced = self.reduce_result(ctx, name, value, job_dir).await;
        self.name_the_source(ctx, &mut reduced, source_url.as_deref())
            .await;
        reduced
    }

    /// Tell the model what the video it just processed is called (§7).
    ///
    /// Only for a YouTube link, and only when the result named nothing itself:
    /// a run demix resolved from a `search` already carries its title, and a
    /// local file has none to look up. It happens after the run, so a job that
    /// failed costs no lookup — and one that succeeded has already proved the
    /// video is reachable.
    async fn name_the_source(&self, ctx: &TurnCtx, reduced: &mut Value, url: Option<&str>) {
        let Some(url) = url.filter(|url| is_youtube_url(url)) else {
            return;
        };
        let Some(object) = reduced.as_object_mut() else {
            return;
        };
        if object.get("ok") != Some(&Value::Bool(true)) || object.contains_key("title") {
            return;
        }

        let Some(names) = self.song_names.lookup(url).await else {
            // Not an error the user hears about: the model still has whatever
            // the request itself said about the song.
            tracing::info!(
                chat_id = ctx.chat_id,
                "YouTube did not say what this video is called"
            );
            return;
        };
        tracing::info!(
            chat_id = ctx.chat_id,
            title = %names.title,
            artist = names.artist.as_deref().unwrap_or("—"),
            "named the source from YouTube"
        );

        object.insert("title".to_string(), json!(names.title));
        if let Some(artist) = names.artist {
            object.insert("artist".to_string(), json!(artist));
        }
        if let Some(track) = names.track {
            object.insert("track".to_string(), json!(track));
        }
    }

    /// Shrink a demix result to what is useful to a language model, and keep the
    /// absolute paths on wavo's side of the fence (§6.4).
    async fn reduce_result(
        &self,
        ctx: &TurnCtx,
        name: &str,
        value: Value,
        job_dir: Option<PathBuf>,
    ) -> Value {
        let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(true);
        let stdout = value.get("stdout").and_then(Value::as_str).unwrap_or("");
        let stderr = value.get("stderr").and_then(Value::as_str).unwrap_or("");

        if !ok {
            // The full stderr goes to the log whatever the user is told (§8.2).
            if !stderr.is_empty() || !stdout.is_empty() {
                tracing::warn!(tool = name, %stdout, %stderr, "demix reported a failure");
            }
            let message = match value.get("error").and_then(Value::as_str) {
                // Argument-level rejections from the MCP server are already one
                // clear sentence, and the model needs them to correct itself.
                Some(error) if stderr.is_empty() && stdout.is_empty() => error.to_string(),
                _ => classify_demix_error(stdout, stderr).message(ctx.lang),
            };
            return error(message);
        }

        let mut reduced = Map::new();
        reduced.insert("ok".to_string(), json!(true));

        if let Some(files) = value.get("files").and_then(Value::as_object) {
            let mut listed = Vec::new();
            for (relative, absolute) in files {
                let Some(absolute) = absolute.as_str() else {
                    continue;
                };
                // Video output is dropped: it cannot be published and it is the
                // biggest thing in the map.
                if !is_audio_file(relative) {
                    continue;
                }
                ctx.remember_path(relative.clone(), PathBuf::from(absolute))
                    .await;
                listed.push(relative.clone());
            }
            listed.sort();
            reduced.insert("files".to_string(), json!(listed));
        }

        for (key, source) in [("stdout", stdout), ("stderr", stderr)] {
            if !source.is_empty() {
                reduced.insert(
                    key.to_string(),
                    json!(truncate_middle(source, self.tool_output_chars)),
                );
            }
        }
        for key in ["key", "scale", "confidence", "url", "title", "video_id"] {
            if let Some(value) = value.get(key) {
                reduced.insert(key.to_string(), value.clone());
            }
        }

        if let Some(job_dir) = job_dir {
            tracing::debug!(job_dir = %job_dir.display(), "job output registered");
        }
        Value::Object(reduced)
    }

    async fn publish_track(&self, ctx: &TurnCtx, arguments: &Value) -> Value {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return error("`path` is required".to_string());
        };
        let part = |key: &str| {
            arguments
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let (artist, title, modification) = (part("artist"), part("title"), part("modification"));

        let resolved = match resolve_publish_path(&ctx.paths().await, path, self.jobs.jobs_dir()) {
            Ok(resolved) => resolved,
            Err(e) => {
                // The refusal is always visible in the log, with the path that
                // was asked for (acceptance criterion 12).
                tracing::warn!(
                    chat_id = ctx.chat_id,
                    requested = path,
                    error = %e,
                    "refused to publish a path outside this turn's output"
                );
                return match e {
                    PathError::NotAudio(_) => error(t(Msg::ErrUnsupportedType, ctx.lang)),
                    _ => error(t(Msg::ErrPathRefused, ctx.lang)),
                };
            }
        };

        // A title reaches plainsong with all three parts or not at all: wavo joins
        // them, filling in what the model left out (§7).
        if artist.trim().is_empty() || modification.trim().is_empty() {
            tracing::warn!(
                chat_id = ctx.chat_id,
                artist_missing = artist.trim().is_empty(),
                modification_missing = modification.trim().is_empty(),
                "the model left a title part out; wavo filled it in"
            );
        }
        let title = if title.trim().is_empty() {
            // The filename is wavo's own, so it names the stem at least.
            resolved
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            title
        };
        let full_title = compose_track_title(&artist, &title, &modification, ctx.lang);

        if let Some(progress) = &ctx.progress {
            progress.stage(Msg::StageUploading).await;
        }

        match self.plainsong.upload(&resolved, &full_title).await {
            Ok(track) => {
                let url = self.plainsong.track_url(&track.id);
                ctx.record_published(PublishedTrack {
                    id: track.id.clone(),
                    title: track.title.clone(),
                    url: url.clone(),
                })
                .await;
                tracing::info!(
                    chat_id = ctx.chat_id,
                    track_id = %track.id,
                    size_bytes = track.size_bytes,
                    "published a track"
                );
                json!({
                    "ok": true,
                    "id": track.id,
                    "track_url": url,
                    "all_tracks_url": self.plainsong.all_tracks_url(),
                    "size_bytes": track.size_bytes,
                })
            }
            Err(e) => {
                tracing::warn!(chat_id = ctx.chat_id, error = %e, "publishing failed");
                error(publish_error_message(&e, ctx.lang))
            }
        }
    }

    async fn list_tracks(&self, ctx: &TurnCtx, arguments: &Value) -> Value {
        let query = arguments.get("q").and_then(Value::as_str);
        match self.plainsong.list(query).await {
            Ok(tracks) => {
                let listed: Vec<Value> = tracks
                    .iter()
                    .rev()
                    .take(10)
                    .map(|track| {
                        json!({
                            "id": track.id,
                            "title": track.title,
                            "track_url": self.plainsong.track_url(&track.id),
                        })
                    })
                    .collect();
                json!({"ok": true, "tracks": listed})
            }
            Err(e) => {
                tracing::warn!(error = %e, "listing tracks failed");
                error(t(Msg::ErrPublishFailed, ctx.lang))
            }
        }
    }

    async fn delete_track(&self, ctx: &TurnCtx, arguments: &Value) -> Value {
        if !self.allow_delete {
            return error(t_name(Msg::ErrUnknownTool, ctx.lang, "delete_track"));
        }
        let Some(id) = arguments.get("id").and_then(Value::as_str) else {
            return error("`id` is required".to_string());
        };
        match self.plainsong.delete(id).await {
            Ok(()) => json!({"ok": true}),
            Err(e) => {
                tracing::warn!(track_id = id, error = %e, "deleting a track failed");
                error(publish_error_message(&e, ctx.lang))
            }
        }
    }
}

#[async_trait]
impl ToolBox for Tools {
    fn catalogue(&self) -> &[ToolDef] {
        &self.catalogue
    }

    async fn call(&self, ctx: &TurnCtx, name: &str, arguments: Value) -> Value {
        match name {
            "publish_track" => self.publish_track(ctx, &arguments).await,
            "list_tracks" => self.list_tracks(ctx, &arguments).await,
            "delete_track" => self.delete_track(ctx, &arguments).await,
            _ => {
                let Some(object) = arguments.as_object().cloned() else {
                    return error("arguments must be a JSON object".to_string());
                };
                self.call_mcp(ctx, name, object).await
            }
        }
    }
}

/// Show the stage wavo asked demix for, and hand back the tasks that keep it
/// updated. demix says nothing until it exits, so the first transition is timed:
/// a download still running after a minute is no longer the interesting part.
fn start_progress(
    ctx: &TurnCtx,
    arguments: &Map<String, Value>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let Some(progress) = ctx.progress.clone() else {
        return Vec::new();
    };

    let separating = arguments
        .get("mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode != "nosplit");
    // A model that fills in the unused source arguments as nulls is not asking for
    // a download, so the key alone does not settle it.
    let remote = ["url", "search"].iter().any(|source| {
        arguments
            .get(*source)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    });
    let processing = if separating {
        Msg::StageSeparating
    } else {
        Msg::StageEffects
    };

    let mut handles = vec![progress.spawn_ticker()];
    let starting = progress.clone();
    handles.push(tokio::spawn(async move {
        if remote {
            starting.stage(Msg::StageDownloading).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        starting.stage(processing).await;
    }));
    handles
}

fn error(message: impl Into<String>) -> Value {
    json!({"ok": false, "error": message.into()})
}

fn publish_error_message(e: &PlainsongError, lang: Lang) -> String {
    match e {
        PlainsongError::TooLarge(mb) => t_n(Msg::ErrTooLarge, lang, mb),
        PlainsongError::UnsupportedType(_) => t(Msg::ErrUnsupportedType, lang).to_string(),
        // A rejected token is an operator problem; the user gets the generic line
        // and the detail stays in the log.
        _ => t(Msg::ErrPublishFailed, lang).to_string(),
    }
}

/// What went wrong in a demix run, as far as it can be told from its output (§8.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemixFailure {
    YoutubeBlocked,
    NoResults,
    MissingBinary(String),
    ModelDownload,
    Other(String),
}

impl DemixFailure {
    pub fn message(&self, lang: Lang) -> String {
        match self {
            DemixFailure::YoutubeBlocked => t(Msg::ErrYoutubeBlocked, lang).to_string(),
            DemixFailure::NoResults => t(Msg::ErrNoResults, lang).to_string(),
            DemixFailure::ModelDownload => t(Msg::ErrModelDownload, lang).to_string(),
            // A missing binary is an operator failure the startup check should
            // have caught; the user only learns that processing failed.
            DemixFailure::MissingBinary(_) => t_detail(Msg::ErrProcessingFailed, lang, ""),
            DemixFailure::Other(detail) => t_detail(Msg::ErrProcessingFailed, lang, detail),
        }
    }
}

/// Lines a demix run prints whatever its outcome. essentia logs
/// `[   INFO   ] MusicExtractorSVM: no classifier models were configured by
/// default` the moment `demix` imports it, so it is the first line of stderr of
/// *every* run, successful or not. Left in, it becomes both the "detail" a
/// failing run reports to the user and a stray "model" in the haystack the
/// download check below matches on (§8.2).
fn is_log_noise(line: &str) -> bool {
    // essentia's own log: `[   INFO   ] …`, `[ WARNING ] …`. `[ ERROR ]` and
    // anything else bracketed stays — that is signal.
    if let Some((level, _)) = line.strip_prefix('[').and_then(|rest| rest.split_once(']')) {
        if matches!(
            level.trim().to_ascii_lowercase().as_str(),
            "info" | "warning" | "debug"
        ) {
            return true;
        }
    }
    let lower = line.to_lowercase();
    // pytubefix — the fallback downloader demix tries after yt-dlp — logs
    // through `logging`, so its warnings arrive with no level prefix at all.
    // `Unable to run botGuard. Skipping poToken generation …` is printed before
    // the download is even attempted, so it is the first line of stderr of every
    // fallback attempt; left in, it became the "detail" of failures it had
    // nothing to do with. The warning spans *two* lines, because the reason it
    // interpolates is pytubefix's own `RuntimeError`, whose message carries a
    // newline: matching only the first line left "Please install Node.js or
    // ensure it's in your PATH." as the detail — advice about a runtime this
    // image deliberately does not ship, presented to a user as the failure.
    if lower.contains("unable to run botguard")
        || lower.contains("skipping potoken generation")
        || lower.contains("node.js is required but not found")
        || lower.contains("please install node.js")
    {
        return true;
    }
    // The sequel to that warning: with no poToken generated, pytubefix warns
    // once more per attempt that the client it picked wants one. Also every
    // run, also not the failure. Only the *warning* is noise — pytubefix
    // raises `PoTokenRequired` with nearly the same sentence, and that line
    // starts with the exception's name rather than the client's.
    if lower.starts_with("the ") && lower.contains("client requires potoken") {
        return true;
    }
    // Python's `warnings` module, and TensorFlow's chatter under it.
    lower.starts_with("warning:") || (lower.contains("warning:") && lower.contains(".py:"))
}

/// The lines of demix output that say something about this particular run.
fn useful_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_log_noise(line))
        .collect()
}

/// The one line to show a user when nothing more specific was recognised.
///
/// A Python traceback puts the reason on its *last* line and scaffolding on the
/// first, so a traceback is read from the end; everything else from the front.
fn failure_detail(stdout: &str, stderr: &str) -> String {
    let lines = useful_lines(stderr);
    let line = if lines
        .first()
        .is_some_and(|line| line.starts_with("Traceback (most recent call last)"))
    {
        lines.last().copied()
    } else {
        lines.first().copied()
    };
    // demix reports some failures on stdout as one human sentence and leaves
    // stderr to essentia alone.
    let line = line.or_else(|| {
        useful_lines(stdout)
            .into_iter()
            .find(|line| line.starts_with("Error:"))
    });
    line.unwrap_or_default().chars().take(200).collect()
}

pub fn classify_demix_error(stdout: &str, stderr: &str) -> DemixFailure {
    let haystack = format!(
        "{}\n{}",
        useful_lines(stdout).join("\n"),
        useful_lines(stderr).join("\n")
    )
    .to_lowercase();

    if haystack.contains("http error 403")
        || haystack.contains("all download strategies failed")
        || haystack.contains("sign in to confirm")
        // How the same refusal reads when it lands on pytubefix instead of
        // yt-dlp — and where it lands first is `search_youtube`, which demix
        // routes through pytubefix alone. Both are exception names or exception
        // text, never the warnings above: YouTube served a bot check instead of
        // the video, which is the blocked download by another name.
        || haystack.contains("detected as a bot")
        || haystack.contains("botdetection")
        || haystack.contains("potokenrequired")
    {
        return DemixFailure::YoutubeBlocked;
    }
    if haystack.contains("no search results")
        || haystack.contains("no results found")
        || haystack.contains("no video results")
    {
        return DemixFailure::NoResults;
    }
    for binary in ["ffmpeg", "ffprobe", "yt-dlp", "demix"] {
        if haystack.contains(&format!("{binary}: not found"))
            || haystack.contains(&format!("{binary} not found"))
            || haystack.contains(&format!("no such file or directory: {binary}"))
        {
            return DemixFailure::MissingBinary(binary.to_string());
        }
    }
    if (haystack.contains("pretrained_models") || haystack.contains("model"))
        && (haystack.contains("download") || haystack.contains("urlerror"))
        && (haystack.contains("fail")
            || haystack.contains("error")
            || haystack.contains("timed out"))
    {
        return DemixFailure::ModelDownload;
    }

    DemixFailure::Other(failure_detail(stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocked_download_is_recognised() {
        let stderr = "ERROR: unable to download video data: HTTP Error 403: Forbidden\n\
                      All download strategies failed";
        assert_eq!(
            classify_demix_error("", stderr),
            DemixFailure::YoutubeBlocked
        );
        assert_eq!(
            classify_demix_error("", "ERROR: Sign in to confirm you're not a bot"),
            DemixFailure::YoutubeBlocked
        );
    }

    #[test]
    fn an_empty_search_is_recognised() {
        assert_eq!(
            classify_demix_error("", "yt-dlp: no results found for query"),
            DemixFailure::NoResults
        );
    }

    #[test]
    fn a_missing_binary_is_recognised_and_stays_operator_facing() {
        let failure = classify_demix_error("", "/bin/sh: 1: ffmpeg: not found");
        assert_eq!(failure, DemixFailure::MissingBinary("ffmpeg".to_string()));
        // Nothing about the binary leaks into the user-facing sentence.
        for lang in [Lang::En, Lang::Pl] {
            assert!(!failure.message(lang).contains("ffmpeg"));
        }
    }

    #[test]
    fn a_failed_model_download_is_recognised() {
        assert_eq!(
            classify_demix_error(
                "",
                "Downloading model 2stems… urlerror: <urlopen error timed out>"
            ),
            DemixFailure::ModelDownload
        );
    }

    #[test]
    fn anything_else_keeps_the_useful_line_of_stderr() {
        // A traceback says what went wrong on its last line, not its first.
        let failure = classify_demix_error("", "\n  Traceback (most recent call last):\nboom\n");
        assert_eq!(failure, DemixFailure::Other("boom".to_string()));
        assert!(failure.message(Lang::En).starts_with("Processing failed."));
        assert!(failure.message(Lang::Pl).starts_with("Przetwarzanie"));

        assert_eq!(
            classify_demix_error("", "\nboom\nand then some\n"),
            DemixFailure::Other("boom".to_string())
        );
    }

    /// essentia logs this on import, so it opens the stderr of every demix run.
    const ESSENTIA_BANNER: &str =
        "[   INFO   ] MusicExtractorSVM: no classifier models were configured by default";

    #[test]
    fn the_essentia_banner_is_never_reported_as_the_failure() {
        let stderr = format!(
            "{ESSENTIA_BANNER}\n\
             Traceback (most recent call last):\n  \
             File \"/opt/venv-demix/lib/python3.8/site-packages/demix/cli.py\", line 365\n    \
             subprocess.run(cmd, check=True)\n\
             subprocess.CalledProcessError: Command '['ffmpeg']' returned non-zero exit status 1."
        );
        let failure = classify_demix_error("Converting audio file to WAV...", &stderr);
        let DemixFailure::Other(detail) = &failure else {
            panic!("expected an unclassified failure, got {failure:?}");
        };
        assert!(
            detail.starts_with("subprocess.CalledProcessError:"),
            "{detail}"
        );
        for lang in [Lang::En, Lang::Pl] {
            assert!(!failure.message(lang).contains("MusicExtractorSVM"));
        }
    }

    #[test]
    fn the_essentia_banner_does_not_make_a_download_look_like_a_model_download() {
        // "no classifier models were configured" used to supply the "model" the
        // spleeter-model check looks for; yt-dlp supplies the rest.
        let stderr = format!("{ESSENTIA_BANNER}\nERROR: unable to download webpage: timed out");
        assert_eq!(
            classify_demix_error("[download] Destination: video.webm", &stderr),
            DemixFailure::Other("ERROR: unable to download webpage: timed out".to_string())
        );
    }

    /// pytubefix prints this, unprefixed, whenever it runs without `node` — and
    /// prints it over two lines, because the reason is the `str()` of a
    /// `RuntimeError` whose own message contains a newline.
    const BOTGUARD_WARNING: &str = "Unable to run botGuard. Skipping poToken generation, \
         reason: Node.js is required but not found. Tried path: node\n\
         Please install Node.js or ensure it's in your PATH.";

    #[test]
    fn the_botguard_warning_is_never_reported_as_the_failure() {
        // What a container with no JavaScript runtime actually produced: the
        // warning opened stderr, so it became the user's "Processing failed: …".
        let stderr = format!(
            "{BOTGUARD_WARNING}\n\
             ERROR: unable to download video data: HTTP Error 403: Forbidden\n\
             All download strategies failed"
        );
        assert_eq!(
            classify_demix_error("", &stderr),
            DemixFailure::YoutubeBlocked
        );
        for lang in [Lang::En, Lang::Pl] {
            assert!(!DemixFailure::YoutubeBlocked
                .message(lang)
                .contains("botGuard"));
        }

        // And with nothing else to go on it still does not reach the user.
        let failure = classify_demix_error("", BOTGUARD_WARNING);
        assert_eq!(failure, DemixFailure::Other(String::new()));
    }

    /// The warning pytubefix adds once it has given up on a poToken. Like the
    /// botGuard one it opens every attempt this image makes, successful or not.
    const POTOKEN_WARNING: &str = "The WEB client requires PoToken to obtain functional streams, \
                                   See more details at \
                                   https://github.com/JuanBindez/pytubefix/pull/209";

    #[test]
    fn a_search_refused_by_youtubes_bot_check_reads_as_a_block() {
        // Verbatim from the server, trimmed: `search_youtube` goes through
        // pytubefix, not yt-dlp, so a refused *search* carries none of the
        // strings the yt-dlp checks look for. Every line above the last is
        // either noise or scaffolding, which is how "Please install Node.js or
        // ensure it's in your PATH." became a user's explanation of it.
        let stderr = format!(
            "{ESSENTIA_BANNER}\n\
             {BOTGUARD_WARNING}\n\
             {POTOKEN_WARNING}\n\
             Traceback (most recent call last):\n  \
             File \"/opt/venv-demix/lib/python3.8/site-packages/pytubefix/__main__.py\", line 802\n    \
             if 'title' in self.vid_info['videoDetails']:\n\
             KeyError: 'videoDetails'\n\
             \n\
             During handling of the above exception, another exception occurred:\n\
             \n\
             Traceback (most recent call last):\n  \
             File \"/opt/venv-demix/lib/python3.8/site-packages/demix/cli.py\", line 244\n    \
             return video.watch_url, video.title\n\
             pytubefix.exceptions.BotDetection: A30Fx3wnfwE This request was detected as a bot. \
             Use `use_po_token=True` or switch to WEB client to view."
        );
        assert_eq!(
            classify_demix_error("✗ Searching YouTube for 'myslovitz'...", &stderr),
            DemixFailure::YoutubeBlocked
        );
        for lang in [Lang::En, Lang::Pl] {
            let message = DemixFailure::YoutubeBlocked.message(lang);
            assert!(!message.contains("Node.js"), "{message}");
            assert!(!message.contains("PoToken"), "{message}");
        }
    }

    #[test]
    fn the_potoken_warning_is_noise_but_the_exception_of_the_same_name_is_not() {
        // Only the warning: it explains nothing about this run, so the run has
        // nothing to say.
        assert_eq!(
            classify_demix_error("", POTOKEN_WARNING),
            DemixFailure::Other(String::new())
        );
        // The exception pytubefix raises says almost the same sentence, and it
        // *is* the failure — a block, reported as one.
        assert_eq!(
            classify_demix_error(
                "",
                "pytubefix.exceptions.PoTokenRequired: A30Fx3wnfwE The WEB client \
                 requires PoToken to obtain functional streams"
            ),
            DemixFailure::YoutubeBlocked
        );
    }

    #[test]
    fn a_demix_error_printed_on_stdout_is_used_when_stderr_is_only_noise() {
        assert_eq!(
            classify_demix_error(
                "Processing: /work/x.mp3\nError: File not found",
                ESSENTIA_BANNER
            ),
            DemixFailure::Other("Error: File not found".to_string())
        );
    }

    #[test]
    fn a_run_that_says_nothing_useful_falls_back_to_the_bare_sentence() {
        let failure = classify_demix_error("", ESSENTIA_BANNER);
        assert_eq!(failure, DemixFailure::Other(String::new()));
        assert_eq!(failure.message(Lang::En), "Processing failed.");
    }

    #[test]
    fn the_detail_is_capped_at_two_hundred_characters() {
        let failure = classify_demix_error("", &"x".repeat(500));
        let DemixFailure::Other(detail) = failure else {
            panic!("expected an unclassified failure");
        };
        assert_eq!(detail.len(), 200);
    }

    #[test]
    fn failure_messages_exist_in_both_languages() {
        for failure in [
            DemixFailure::YoutubeBlocked,
            DemixFailure::NoResults,
            DemixFailure::ModelDownload,
            DemixFailure::MissingBinary("ffmpeg".into()),
            DemixFailure::Other("boom".into()),
        ] {
            assert_ne!(failure.message(Lang::En), failure.message(Lang::Pl));
        }
    }
}
