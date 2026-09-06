//! The tool-calling loop: one user message in, one reply out, with the budgets
//! and the escalation policy of §6.5 and §8.1 in between.

pub mod openai;
pub mod prompt;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::Instrument;

use crate::llm::openai::{ChatMessage, OpenAi, ToolCall};
use crate::session::PublishedTrack;
use crate::telegram::format::{t, Lang, Msg};
use crate::tools::{ToolBox, TurnCtx};

/// How a turn ended. Everything but `Answered` produces a wavo-composed reply,
/// because the model is either broken, gone or out of budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Answered,
    IterationBudget,
    Timeout,
    Cancelled,
    LlmFailure,
    /// Two invalid tool calls after escalating to the fallback model.
    Abandoned,
}

#[derive(Debug)]
pub struct TurnResult {
    pub reply: String,
    pub messages: Vec<ChatMessage>,
    pub published: Vec<PublishedTrack>,
    pub outcome: Outcome,
    pub iterations: usize,
    pub tools_called: Vec<String>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub escalated: bool,
}

pub struct Agent {
    llm: Arc<OpenAi>,
    tools: Arc<dyn ToolBox>,
    model: String,
    fallback_model: String,
    max_iterations: usize,
    turn_timeout: Duration,
}

impl Agent {
    pub fn new(
        llm: Arc<OpenAi>,
        tools: Arc<dyn ToolBox>,
        model: String,
        fallback_model: String,
        max_iterations: usize,
        turn_timeout: Duration,
    ) -> Self {
        Self {
            llm,
            tools,
            model,
            fallback_model,
            max_iterations,
            turn_timeout,
        }
    }

    pub async fn run_turn(
        &self,
        ctx: &TurnCtx,
        history: Vec<ChatMessage>,
        user_message: &str,
    ) -> TurnResult {
        let started = Instant::now();
        let mut messages = history;
        messages.push(ChatMessage::user(user_message));

        let mut model = self.model.clone();
        let mut invalid_calls = 0usize;
        let mut escalated = false;
        let mut tools_called = Vec::new();
        let mut tokens_in = 0;
        let mut tokens_out = 0;
        let user_tag = ctx.chat_id.to_string();

        for iteration in 1..=self.max_iterations {
            if ctx.cancel.is_cancelled() {
                return self
                    .finish(
                        ctx,
                        messages,
                        Outcome::Cancelled,
                        iteration - 1,
                        tools_called,
                        tokens_in,
                        tokens_out,
                        escalated,
                    )
                    .await;
            }
            if started.elapsed() > self.turn_timeout {
                tracing::warn!(
                    chat_id = ctx.chat_id,
                    iteration,
                    last_tool = tools_called.last().map(String::as_str).unwrap_or("none"),
                    "turn exceeded its wall-clock budget"
                );
                return self
                    .finish(
                        ctx,
                        messages,
                        Outcome::Timeout,
                        iteration - 1,
                        tools_called,
                        tokens_in,
                        tokens_out,
                        escalated,
                    )
                    .await;
            }

            let completion = match self
                .llm
                .chat(&model, &messages, self.tools.catalogue(), &user_tag)
                .await
            {
                Ok(completion) => completion,
                Err(e) => {
                    tracing::error!(chat_id = ctx.chat_id, model, error = %e, "LLM call failed");
                    return self
                        .finish(
                            ctx,
                            messages,
                            Outcome::LlmFailure,
                            iteration - 1,
                            tools_called,
                            tokens_in,
                            tokens_out,
                            escalated,
                        )
                        .await;
                }
            };

            tokens_in += completion.usage.prompt_tokens;
            tokens_out += completion.usage.completion_tokens;

            let assistant = completion.message;
            let calls: Vec<ToolCall> = assistant.tool_calls().to_vec();
            messages.push(assistant.clone());

            if calls.is_empty() {
                let reply = assistant.content.unwrap_or_default();
                return self
                    .answer(
                        ctx,
                        reply,
                        messages,
                        iteration,
                        tools_called,
                        tokens_in,
                        tokens_out,
                        escalated,
                    )
                    .await;
            }

            for call in calls {
                let started_call = Instant::now();
                let span = tracing::info_span!("tool", name = %call.function.name);

                let result = match self.prepare(&call) {
                    Ok(arguments) => {
                        invalid_calls = 0;
                        tools_called.push(call.function.name.clone());
                        let value = self
                            .tools
                            .call(ctx, &call.function.name, arguments)
                            .instrument(span.clone())
                            .await;
                        let ok = value
                            .get("ok")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(true);
                        span.in_scope(|| {
                            tracing::info!(
                                duration_ms = started_call.elapsed().as_millis() as u64,
                                ok,
                                "tool call finished"
                            )
                        });
                        value
                    }
                    Err(message) => {
                        // Not an abort: the model is told what it got wrong so it
                        // can try again (§8.1).
                        invalid_calls += 1;
                        span.in_scope(
                            || tracing::warn!(invalid_calls, %message, "invalid tool call"),
                        );
                        serde_json::json!({"ok": false, "error": message})
                    }
                };

                messages.push(ChatMessage::tool_result(
                    call.id,
                    serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string()),
                ));
            }

            if invalid_calls >= 2 {
                if escalated {
                    tracing::error!(
                        chat_id = ctx.chat_id,
                        "abandoning the turn after repeated invalid tool calls"
                    );
                    return self
                        .finish(
                            ctx,
                            messages,
                            Outcome::Abandoned,
                            iteration,
                            tools_called,
                            tokens_in,
                            tokens_out,
                            escalated,
                        )
                        .await;
                }
                tracing::warn!(
                    chat_id = ctx.chat_id,
                    from = %model,
                    to = %self.fallback_model,
                    "escalating to the fallback model"
                );
                model = self.fallback_model.clone();
                escalated = true;
                invalid_calls = 0;
            }
        }

        tracing::warn!(
            chat_id = ctx.chat_id,
            iterations = self.max_iterations,
            last_tool = tools_called.last().map(String::as_str).unwrap_or("none"),
            "turn exceeded its tool-call budget"
        );
        self.finish(
            ctx,
            messages,
            Outcome::IterationBudget,
            self.max_iterations,
            tools_called,
            tokens_in,
            tokens_out,
            escalated,
        )
        .await
    }

    /// Validate one tool call before it is dispatched: the name has to be one we
    /// published, and the arguments have to be a JSON object.
    fn prepare(&self, call: &ToolCall) -> Result<Value, String> {
        if !self
            .tools
            .catalogue()
            .iter()
            .any(|tool| tool.name() == call.function.name)
        {
            return Err(format!("unknown tool `{}`", call.function.name));
        }

        let raw = call.function.arguments.trim();
        let arguments: Value = if raw.is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(raw).map_err(|e| format!("arguments were not valid JSON: {e}"))?
        };
        if !arguments.is_object() {
            return Err("arguments must be a JSON object".to_string());
        }
        Ok(arguments)
    }

    #[allow(clippy::too_many_arguments)]
    async fn answer(
        &self,
        ctx: &TurnCtx,
        reply: String,
        messages: Vec<ChatMessage>,
        iterations: usize,
        tools_called: Vec<String>,
        tokens_in: u64,
        tokens_out: u64,
        escalated: bool,
    ) -> TurnResult {
        let published = ctx.published().await;
        let reply = if reply.trim().is_empty() {
            t(Msg::Done, ctx.lang).to_string()
        } else {
            reply
        };
        TurnResult {
            reply,
            messages,
            published,
            outcome: Outcome::Answered,
            iterations,
            tools_called,
            tokens_in,
            tokens_out,
            escalated,
        }
    }

    /// Every non-`Answered` ending, with the apology wavo composes itself so a
    /// broken model cannot decide what the user is told.
    #[allow(clippy::too_many_arguments)]
    async fn finish(
        &self,
        ctx: &TurnCtx,
        messages: Vec<ChatMessage>,
        outcome: Outcome,
        iterations: usize,
        tools_called: Vec<String>,
        tokens_in: u64,
        tokens_out: u64,
        escalated: bool,
    ) -> TurnResult {
        let reply = t(message_for(outcome), ctx.lang).to_string();
        TurnResult {
            reply,
            messages,
            // A cancelled turn publishes nothing, even if a tool already ran (§8.3).
            published: if outcome == Outcome::Cancelled {
                Vec::new()
            } else {
                ctx.published().await
            },
            outcome,
            iterations,
            tools_called,
            tokens_in,
            tokens_out,
            escalated,
        }
    }
}

fn message_for(outcome: Outcome) -> Msg {
    match outcome {
        Outcome::Cancelled => Msg::ErrCancelled,
        Outcome::Timeout => Msg::ErrTurnTimeout,
        Outcome::LlmFailure => Msg::ErrLlmUnavailable,
        Outcome::IterationBudget | Outcome::Abandoned | Outcome::Answered => {
            Msg::ErrIterationBudget
        }
    }
}

/// The reply as it goes to Telegram: the model's words, escaped, with the links
/// block wavo composes appended (§5.4).
pub fn compose_reply(
    reply: &str,
    published: &[PublishedTrack],
    all_tracks_url: &str,
    lang: Lang,
) -> String {
    let mut out = crate::telegram::format::escape_html(reply.trim());
    let links = crate::telegram::format::links_block(published, all_tracks_url, lang);
    if !links.is_empty() {
        out.push('\n');
        out.push_str(&links);
    }
    crate::telegram::format::truncate_message(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_links_block_is_appended_by_wavo_not_by_the_model() {
        let published = vec![PublishedTrack {
            id: "abc".into(),
            title: "Vocals".into(),
            url: "http://x/track.html?id=abc".into(),
        }];
        let composed = compose_reply("Here you go & enjoy", &published, "http://x/", Lang::En);
        assert!(composed.contains("Here you go &amp; enjoy"));
        assert!(composed.contains("track.html?id=abc"));
        assert!(composed.contains("all songs:"));
    }

    #[test]
    fn a_reply_without_tracks_carries_no_links() {
        let composed = compose_reply("Which song?", &[], "http://x/", Lang::Pl);
        assert_eq!(composed, "Which song?");
    }
}
