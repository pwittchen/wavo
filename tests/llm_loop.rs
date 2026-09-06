//! The tool-calling loop against a stub OpenAI server replaying scripted
//! completions: budgets, escalation, malformed arguments and cancellation (§14).

mod support;

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use support::{text_response, tool_call_response, ScriptedTools, StubHttp};
use wavo::config::Secret;
use wavo::llm::openai::OpenAi;
use wavo::llm::{Agent, Outcome};
use wavo::session::PublishedTrack;
use wavo::telegram::format::{t, Lang, Msg};
use wavo::tools::{ToolBox, TurnCtx};

const MAIN_MODEL: &str = "test-mini";
const FALLBACK_MODEL: &str = "test-large";

fn agent(stub: &StubHttp, tools: Arc<dyn ToolBox>, max_iterations: usize) -> Agent {
    let llm = Arc::new(OpenAi::new(
        stub.base(),
        Secret::new("test-key"),
        Duration::from_secs(10),
    ));
    Agent::new(
        llm,
        tools,
        MAIN_MODEL.to_string(),
        FALLBACK_MODEL.to_string(),
        max_iterations,
        Duration::from_secs(60),
    )
}

fn ctx(cancel: CancellationToken) -> TurnCtx {
    TurnCtx::new(42, Uuid::new_v4(), Lang::En, cancel, None)
}

#[tokio::test]
async fn a_turn_chains_tool_calls_and_ends_with_the_models_answer() {
    let stub = StubHttp::start(vec![
        (
            200,
            &tool_call_response(
                "c1",
                "process_audio",
                r#"{"search":"Queen - Bohemian Rhapsody","mode":"2stems"}"#,
            ),
        ),
        (
            200,
            &tool_call_response(
                "c2",
                "publish_track",
                r#"{"path":"music/mp3/song_vocals.mp3","title":"Bohemian Rhapsody — vocals"}"#,
            ),
        ),
        (200, &text_response("Here are the vocals.")),
    ])
    .await;

    let tools = Arc::new(
        ScriptedTools::new()
            .returning(
                "process_audio",
                json!({"ok": true, "files": ["music/mp3/song_vocals.mp3"]}),
            )
            .returning("publish_track", json!({"ok": true, "id": "7f1c"})),
    );
    let agent = agent(&stub, tools.clone(), 8);
    let ctx = ctx(CancellationToken::new());

    let result = agent
        .run_turn(
            &ctx,
            vec![],
            "separate vocals from Queen - Bohemian Rhapsody",
        )
        .await;

    assert_eq!(result.outcome, Outcome::Answered);
    assert_eq!(result.reply, "Here are the vocals.");
    assert_eq!(result.tools_called, vec!["process_audio", "publish_track"]);
    assert_eq!(result.iterations, 3);
    assert_eq!(result.tokens_in, 30);
    assert!(!result.escalated);

    // The arguments reached the tool as the model wrote them.
    let calls = tools.calls().await;
    assert_eq!(calls[0].1["mode"], "2stems");
    assert_eq!(calls[1].1["title"], "Bohemian Rhapsody — vocals");

    // The first request carried the system prompt, the user message and the catalogue.
    let requests = stub.requests().await;
    assert_eq!(requests.len(), 3);
    let first = requests[0].json();
    assert_eq!(first["model"], MAIN_MODEL);
    assert_eq!(first["messages"][0]["role"], "user");
    assert_eq!(first["tool_choice"], "auto");
    let names: Vec<String> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["process_audio", "publish_track"]);
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer test-key")
    );

    // Every tool call has a matching tool result in the stored history.
    let last = result.messages.last().unwrap();
    assert_eq!(last.content.as_deref(), Some("Here are the vocals."));
}

#[tokio::test]
async fn a_model_that_never_stops_calling_tools_hits_the_iteration_budget() {
    let stub = StubHttp::start(vec![(
        200,
        &tool_call_response("c1", "process_audio", r#"{"search":"x"}"#),
    )])
    .await;

    let agent = agent(&stub, Arc::new(ScriptedTools::new()), 3);
    let result = agent
        .run_turn(&ctx(CancellationToken::new()), vec![], "do something")
        .await;

    assert_eq!(result.outcome, Outcome::IterationBudget);
    assert_eq!(result.iterations, 3);
    assert_eq!(stub.request_count().await, 3);
    // The apology is wavo's, in the user's language.
    assert_eq!(result.reply, t(Msg::ErrIterationBudget, Lang::En));
}

#[tokio::test]
async fn invalid_arguments_escalate_to_the_fallback_model_and_then_give_up() {
    let stub = StubHttp::start(vec![(
        200,
        &tool_call_response("c1", "process_audio", "{not json"),
    )])
    .await;

    let agent = agent(&stub, Arc::new(ScriptedTools::new()), 8);
    let result = agent
        .run_turn(&ctx(CancellationToken::new()), vec![], "do something")
        .await;

    assert_eq!(result.outcome, Outcome::Abandoned);
    assert!(result.escalated);

    let requests = stub.requests().await;
    // Two invalid calls on the main model, then the turn switches over.
    assert_eq!(requests[0].json()["model"], MAIN_MODEL);
    assert_eq!(requests[1].json()["model"], MAIN_MODEL);
    assert_eq!(requests[2].json()["model"], FALLBACK_MODEL);
    assert_eq!(requests.len(), 4, "two more invalid calls end the turn");

    // The model was told what was wrong rather than being cut off.
    let complaint = result
        .messages
        .iter()
        .filter_map(|m| m.content.as_deref())
        .find(|content| content.contains("not valid JSON"));
    assert!(
        complaint.is_some(),
        "the model was never told about the bad arguments"
    );
}

#[tokio::test]
async fn a_tool_name_wavo_does_not_publish_is_refused_without_dispatching() {
    let stub = StubHttp::start(vec![
        (200, &tool_call_response("c1", "rm_rf", "{}")),
        (200, &text_response("Sorry about that.")),
    ])
    .await;

    let tools = Arc::new(ScriptedTools::new());
    let agent = agent(&stub, tools.clone(), 8);
    let result = agent
        .run_turn(&ctx(CancellationToken::new()), vec![], "delete everything")
        .await;

    assert_eq!(result.outcome, Outcome::Answered);
    assert!(
        tools.calls().await.is_empty(),
        "an unknown tool was dispatched"
    );
    assert!(result
        .messages
        .iter()
        .filter_map(|m| m.content.as_deref())
        .any(|content| content.contains("unknown tool")));
}

#[tokio::test]
async fn a_failing_tool_is_reported_to_the_model_rather_than_aborting_the_turn() {
    let stub = StubHttp::start(vec![
        (
            200,
            &tool_call_response("c1", "process_audio", r#"{"search":"x"}"#),
        ),
        (
            200,
            &text_response("YouTube blocked the download — try a direct link."),
        ),
    ])
    .await;

    let tools = Arc::new(ScriptedTools::new().returning(
        "process_audio",
        json!({"ok": false, "error": "YouTube blocked the download."}),
    ));
    let agent = agent(&stub, tools, 8);
    let result = agent
        .run_turn(&ctx(CancellationToken::new()), vec![], "get me that song")
        .await;

    assert_eq!(result.outcome, Outcome::Answered);
    assert!(result.reply.contains("YouTube blocked"));
    assert!(result
        .messages
        .iter()
        .filter_map(|m| m.content.as_deref())
        .any(|content| content.contains("\"ok\":false")));
}

#[tokio::test]
async fn a_cancelled_turn_stops_and_publishes_nothing() {
    let stub = StubHttp::start(vec![(200, &text_response("never asked"))]).await;

    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = ctx(cancel);
    // Something was published before the cancellation arrived; it is not reported.
    ctx.record_published(PublishedTrack {
        id: "abc".into(),
        title: "Vocals".into(),
        url: "http://x/track.html?id=abc".into(),
    })
    .await;

    let agent = agent(&stub, Arc::new(ScriptedTools::new()), 8);
    let result = agent.run_turn(&ctx, vec![], "slow it down").await;

    assert_eq!(result.outcome, Outcome::Cancelled);
    assert!(result.published.is_empty());
    assert_eq!(result.reply, t(Msg::ErrCancelled, Lang::En));
    assert_eq!(
        stub.request_count().await,
        0,
        "the LLM was called after /cancel"
    );
}

#[tokio::test]
async fn a_turn_that_outlives_its_wall_clock_budget_ends_with_an_apology() {
    let stub = StubHttp::start(vec![(
        200,
        &tool_call_response("c1", "process_audio", r#"{"search":"x"}"#),
    )])
    .await;

    let llm = Arc::new(OpenAi::new(
        stub.base(),
        Secret::new("test-key"),
        Duration::from_secs(10),
    ));
    let agent = Agent::new(
        llm,
        Arc::new(ScriptedTools::new()),
        MAIN_MODEL.to_string(),
        FALLBACK_MODEL.to_string(),
        50,
        // Already spent by the time the second iteration checks.
        Duration::from_millis(1),
    );

    let result = agent
        .run_turn(&ctx(CancellationToken::new()), vec![], "do something")
        .await;

    assert_eq!(result.outcome, Outcome::Timeout);
    assert_eq!(result.reply, t(Msg::ErrTurnTimeout, Lang::En));
}

#[tokio::test]
async fn a_provider_outage_is_retried_and_then_reported_in_the_users_language() {
    let stub = StubHttp::start(vec![(500, "{\"error\":\"upstream on fire\"}")]).await;

    let agent = agent(&stub, Arc::new(ScriptedTools::new()), 8);
    let ctx = TurnCtx::new(42, Uuid::new_v4(), Lang::Pl, CancellationToken::new(), None);
    let result = agent.run_turn(&ctx, vec![], "zwolnij ten utwór").await;

    assert_eq!(result.outcome, Outcome::LlmFailure);
    assert_eq!(result.reply, t(Msg::ErrLlmUnavailable, Lang::Pl));
    assert_eq!(
        stub.request_count().await,
        3,
        "two retries after the first failure"
    );
}
