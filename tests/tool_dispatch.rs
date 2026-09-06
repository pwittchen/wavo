//! The tool layer end to end: a real MCP client against the stub server, a real
//! plainsong client against a stub HTTP server, and the two containment rules of
//! §6.3 and §6.4 observed from the outside.

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use support::{test_config, StubHttp};
use wavo::config::Config;
use wavo::jobs::JobManager;
use wavo::mcp::McpClient;
use wavo::telegram::format::Lang;
use wavo::tools::plainsong::PlainsongClient;
use wavo::tools::{ToolBox, Tools, TurnCtx};

fn stub_server() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stub_mcp_server.py")
}

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

struct Harness {
    tools: Tools,
    ctx: TurnCtx,
    jobs: Arc<JobManager>,
    _work: tempfile::TempDir,
    _plainsong_stub: StubHttp,
}

async fn harness(
    config: impl FnOnce(&mut Config),
    plainsong_responses: Vec<(u16, &str)>,
) -> Harness {
    let work = tempfile::tempdir().unwrap();
    let mut cfg = test_config(work.path().to_path_buf());
    config(&mut cfg);

    std::fs::create_dir_all(cfg.jobs_dir()).unwrap();

    let stub = StubHttp::start(plainsong_responses).await;
    let plainsong = Arc::new(PlainsongClient::new(
        stub.base(),
        cfg.plainsong_public_url.clone(),
        cfg.plainsong_token.clone(),
        cfg.max_upload_bytes(),
    ));
    let mcp = Arc::new(
        McpClient::connect(stub_server().to_str().unwrap(), work.path())
            .await
            .unwrap(),
    );
    let jobs = Arc::new(JobManager::new(
        cfg.jobs_dir(),
        cfg.max_concurrent_jobs,
        cfg.keep_job_files,
    ));
    let tools = Tools::new(mcp, plainsong, jobs.clone(), &cfg).await;

    Harness {
        tools,
        ctx: TurnCtx::new(42, Uuid::new_v4(), Lang::En, CancellationToken::new(), None),
        jobs,
        _work: work,
        _plainsong_stub: stub,
    }
}

/// The single job directory a `process_audio` call created.
fn only_job_dir(jobs: &JobManager) -> PathBuf {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(jobs.jobs_dir())
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(dirs.len(), 1, "expected exactly one job directory");
    dirs.pop().unwrap()
}

#[tokio::test]
async fn the_catalogue_hides_the_operator_only_tools_by_default() {
    if !python3_available() {
        return;
    }
    let h = harness(|_| {}, vec![(200, "[]")]).await;
    let names: Vec<&str> = h.tools.catalogue().iter().map(|tool| tool.name()).collect();

    assert!(names.contains(&"process_audio"));
    assert!(names.contains(&"publish_track"));
    assert!(names.contains(&"list_tracks"));
    assert!(
        !names.contains(&"delete_track"),
        "delete_track is off by default"
    );
    assert!(!names.contains(&"clean"), "clean is off by default");

    // And the schema the model sees never mentions where files go.
    let process_audio = h
        .tools
        .catalogue()
        .iter()
        .find(|tool| tool.name() == "process_audio")
        .unwrap();
    let properties = process_audio.function.parameters["properties"]
        .as_object()
        .unwrap();
    assert!(!properties.contains_key("output_dir"));
    assert!(!properties.contains_key("cwd"));
}

#[tokio::test]
async fn opting_in_exposes_the_operator_only_tools() {
    if !python3_available() {
        return;
    }
    let h = harness(
        |cfg| {
            cfg.allow_delete = true;
            cfg.expose_clean = true;
        },
        vec![(200, "[]")],
    )
    .await;
    let names: Vec<&str> = h.tools.catalogue().iter().map(|tool| tool.name()).collect();
    assert!(names.contains(&"delete_track"));
}

#[tokio::test]
async fn a_run_lands_in_a_job_directory_wavo_chose_and_comes_back_trimmed() {
    if !python3_available() {
        return;
    }
    let h = harness(|cfg| cfg.tool_output_chars = 200, vec![(200, "[]")]).await;

    let result = h
        .tools
        .call(
            &h.ctx,
            "process_audio",
            json!({"search": "Queen - Bohemian Rhapsody", "mode": "2stems"}),
        )
        .await;

    assert_eq!(result["ok"], true);

    // The model sees relative paths only, and no video.
    let files: Vec<&str> = result["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(
        files,
        vec![
            "music/mp3/song_accompaniment.mp3",
            "music/mp3/song_vocals.mp3"
        ]
    );
    assert!(
        !result.to_string().contains(".mkv"),
        "video output reached the model"
    );

    // The output really went to a job directory under /work/jobs.
    let job_dir = only_job_dir(&h.jobs);
    assert!(job_dir.starts_with(h.jobs.jobs_dir()));

    // demix's 5 KB of stdout is cut down, head and tail kept.
    let stdout = result["stdout"].as_str().unwrap();
    assert!(stdout.contains("chars omitted"));
    assert!(stdout.starts_with("Detected key: C major"));
    assert!(stdout.chars().count() < 400);
}

/// A YouTube link is a source like any other: it reaches demix character for
/// character, and wavo's own arguments are added around it, not over it.
#[tokio::test]
async fn a_youtube_link_reaches_demix_untouched() {
    if !python3_available() {
        return;
    }
    let h = harness(|_| {}, vec![(200, "[]")]).await;
    let link = "https://www.youtube.com/watch?v=fJ9rUzIMcZQ&list=PL1234&t=42s";

    let result = h
        .tools
        .call(
            &h.ctx,
            "process_audio",
            json!({"url": link, "mode": "2stems"}),
        )
        .await;

    assert_eq!(result["ok"], true);
    assert_eq!(result["url"], link, "the link was rewritten on the way");

    let job_dir = only_job_dir(&h.jobs);
    assert!(job_dir.starts_with(h.jobs.jobs_dir()));
}

#[tokio::test]
async fn publishing_walks_from_a_relative_key_to_a_real_upload() {
    if !python3_available() {
        return;
    }
    let h = harness(
        |_| {},
        vec![(
            201,
            r#"{"id":"7f1c","title":"Bohemian Rhapsody — vocals","filename":"song_vocals.mp3","size_bytes":9}"#,
        )],
    )
    .await;

    h.tools
        .call(
            &h.ctx,
            "process_audio",
            json!({"search": "Queen", "mode": "2stems"}),
        )
        .await;

    // demix would have written these; the stub only reported them.
    let job_dir = only_job_dir(&h.jobs);
    std::fs::create_dir_all(job_dir.join("music/mp3")).unwrap();
    std::fs::write(job_dir.join("music/mp3/song_vocals.mp3"), b"fake audio").unwrap();

    let result = h
        .tools
        .call(
            &h.ctx,
            "publish_track",
            json!({
                "path": "music/mp3/song_vocals.mp3",
                "artist": "Queen",
                "title": "Bohemian Rhapsody",
                "modification": "bez wokalu"
            }),
        )
        .await;

    assert_eq!(result["ok"], true);
    assert_eq!(result["id"], "7f1c");
    assert_eq!(
        result["track_url"],
        "https://music.example.com/track.html?id=7f1c"
    );
    assert_eq!(result["all_tracks_url"], "https://music.example.com/");

    // The three parts reached plainsong joined into one title, in the multipart body.
    let uploads = h._plainsong_stub.requests().await;
    let body = &uploads.last().unwrap().body;
    assert!(
        body.contains("Queen — Bohemian Rhapsody (bez wokalu)"),
        "the title plainsong received was: {body}"
    );

    // The turn now knows about the track, which is what the links block is built from.
    let published = h.ctx.published().await;
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].id, "7f1c");
}

#[tokio::test]
async fn a_published_title_keeps_its_three_parts_even_when_the_model_skimps() {
    if !python3_available() {
        return;
    }
    let h = harness(
        |_| {},
        vec![(
            201,
            r#"{"id":"7f1c","title":"x","filename":"song_vocals.mp3","size_bytes":9}"#,
        )],
    )
    .await;
    // The user asked in Polish, so the parts wavo fills in are Polish.
    let ctx = TurnCtx::new(42, Uuid::new_v4(), Lang::Pl, CancellationToken::new(), None);

    h.tools
        .call(&ctx, "process_audio", json!({"search": "Queen"}))
        .await;
    let job_dir = only_job_dir(&h.jobs);
    std::fs::create_dir_all(job_dir.join("music/mp3")).unwrap();
    std::fs::write(job_dir.join("music/mp3/song_vocals.mp3"), b"fake audio").unwrap();

    let result = h
        .tools
        .call(
            &ctx,
            "publish_track",
            json!({"path": "music/mp3/song_vocals.mp3", "title": "Bohemian Rhapsody"}),
        )
        .await;

    assert_eq!(result["ok"], true);
    let uploads = h._plainsong_stub.requests().await;
    let body = &uploads.last().unwrap().body;
    assert!(
        body.contains("Nieznany wykonawca — Bohemian Rhapsody (oryginał)"),
        "the title plainsong received was: {body}"
    );
}

#[tokio::test]
async fn publishing_a_path_wavo_never_produced_is_refused_before_any_upload() {
    if !python3_available() {
        return;
    }
    let h = harness(|_| {}, vec![(201, "{}")]).await;

    for requested in [
        "/etc/passwd",
        "../../../../etc/passwd",
        "music/mp3/anything.mp3",
    ] {
        let result = h
            .tools
            .call(
                &h.ctx,
                "publish_track",
                json!({"path": requested, "title": "not yours"}),
            )
            .await;
        assert_eq!(result["ok"], false, "{requested} was accepted");
        assert!(result["error"]
            .as_str()
            .unwrap()
            .contains("produced during this conversation"));
    }

    assert_eq!(
        h._plainsong_stub.request_count().await,
        0,
        "a refused path still reached plainsong"
    );
    assert!(h.ctx.published().await.is_empty());
}

#[tokio::test]
async fn an_unknown_tool_name_comes_back_as_a_result_not_an_error() {
    if !python3_available() {
        return;
    }
    let h = harness(|_| {}, vec![(200, "[]")]).await;
    let result = h.tools.call(&h.ctx, "rm_rf", json!({})).await;
    assert_eq!(result["ok"], false);
    assert!(result["error"].as_str().unwrap().contains("rm_rf"));
}

#[tokio::test]
async fn a_cancelled_turn_does_not_start_a_new_run() {
    if !python3_available() {
        return;
    }
    let h = harness(|_| {}, vec![(200, "[]")]).await;
    h.ctx.cancel.cancel();

    let result = h
        .tools
        .call(&h.ctx, "process_audio", json!({"search": "Queen"}))
        .await;
    assert_eq!(result["ok"], false);
}
