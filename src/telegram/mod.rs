//! The long-polling loop and what happens to each message that comes out of it.

pub mod api;
pub mod commands;
pub mod format;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use crate::config::Config;
use crate::jobs::{JobManager, Progress};
use crate::llm::{compose_reply, Agent, Outcome};
use crate::session::{Sessions, TurnStart};
use crate::telegram::api::{Message, Telegram, Update};
use crate::telegram::commands::Command;
use crate::telegram::format::{t, Lang, Msg};
use crate::tools::plainsong::PlainsongClient;
use crate::tools::TurnCtx;

/// Telegram's server-side wait per poll. Long enough that an idle bot makes two
/// requests a minute, short enough that a dropped connection is noticed.
const POLL_TIMEOUT_SECS: u64 = 30;
/// How long to wait before polling again after a failed poll.
const POLL_BACKOFF: Duration = Duration::from_secs(3);
const EVICTION_INTERVAL: Duration = Duration::from_secs(300);

pub struct Bot {
    pub config: Arc<Config>,
    pub telegram: Telegram,
    pub sessions: Arc<Sessions>,
    pub agent: Arc<Agent>,
    pub jobs: Arc<JobManager>,
    pub plainsong: Arc<PlainsongClient>,
    pub started: Instant,
}

impl Bot {
    /// Poll until told to stop. In-flight turns are spawned tasks; the caller
    /// gives them the grace period of §8.3 after this returns.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        let mut offset: Option<i64> = None;
        let mut last_eviction = Instant::now();

        loop {
            if shutdown.is_cancelled() {
                return;
            }

            let updates = tokio::select! {
                updates = self.telegram.get_updates(offset, POLL_TIMEOUT_SECS) => updates,
                _ = shutdown.cancelled() => return,
            };

            match updates {
                Ok(updates) => {
                    for update in updates {
                        offset = Some(match offset {
                            Some(current) => current.max(update.update_id + 1),
                            None => update.update_id + 1,
                        });
                        self.clone().dispatch(update);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "polling failed");
                    tokio::select! {
                        _ = tokio::time::sleep(POLL_BACKOFF) => {}
                        _ = shutdown.cancelled() => return,
                    }
                }
            }

            if last_eviction.elapsed() > EVICTION_INTERVAL {
                last_eviction = Instant::now();
                let evicted = self.sessions.evict_idle().await;
                if evicted > 0 {
                    tracing::debug!(evicted, "evicted idle sessions");
                }
            }
        }
    }

    fn dispatch(self: Arc<Self>, update: Update) {
        let Some(message) = update.message else {
            return;
        };
        let chat_id = message.chat.id;

        // Strangers get silence, not an error message — but the operator gets the
        // chat ID, which is the only way to add themselves (§5.3).
        if !self.config.is_allowed(chat_id) {
            tracing::info!(chat_id, "ignored a message from a chat that is not allowed");
            return;
        }

        tokio::spawn(async move {
            self.handle(message).await;
        });
    }

    async fn handle(self: Arc<Self>, message: Message) {
        let chat_id = message.chat.id;

        let Some(text) = message.text.clone().filter(|t| !t.trim().is_empty()) else {
            let lang = self.sessions.language_for(chat_id, "").await;
            self.say(chat_id, t(Msg::TextOnly, lang)).await;
            return;
        };

        let lang = self.sessions.language_for(chat_id, &text).await;

        if let Some(command) = commands::parse(&text) {
            self.run_command(chat_id, command, lang).await;
            return;
        }

        self.run_turn(chat_id, text, lang).await;
    }

    async fn run_command(&self, chat_id: i64, command: Command, lang: Lang) {
        let reply = match command {
            Command::Help => commands::help(lang),
            Command::Status => commands::status(
                lang,
                self.sessions.busy_here(chat_id).await,
                self.jobs.pending(),
                self.started.elapsed(),
            ),
            Command::Tracks => match self.plainsong.list(None).await {
                Ok(tracks) => commands::tracks(lang, &tracks, |id| self.plainsong.track_url(id)),
                Err(e) => {
                    tracing::warn!(error = %e, "could not list tracks for /tracks");
                    t(Msg::TracksUnavailable, lang).to_string()
                }
            },
            Command::Reset => {
                self.sessions.reset(chat_id).await;
                t(Msg::ResetDone, lang).to_string()
            }
            Command::Cancel => {
                let cancelled = self.sessions.cancel(chat_id).await;
                let msg = if cancelled {
                    Msg::CancelRequested
                } else {
                    Msg::CancelNothing
                };
                t(msg, lang).to_string()
            }
        };
        self.say(chat_id, &reply).await;
    }

    async fn run_turn(self: Arc<Self>, chat_id: i64, text: String, lang: Lang) {
        let TurnStart::Ready { history, cancel } = self.sessions.begin_turn(chat_id).await else {
            // A second message from the same chat is answered and dropped, not
            // queued (§3.4).
            self.say(chat_id, t(Msg::Busy, lang)).await;
            return;
        };

        let turn_id = Uuid::new_v4();
        let span = tracing::info_span!(
            "turn",
            chat_id,
            turn_id = %turn_id,
            model = %self.config.openai_model,
        );
        self.turn_body(chat_id, text, lang, turn_id, cancel, history)
            .instrument(span)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn turn_body(
        self: Arc<Self>,
        chat_id: i64,
        text: String,
        lang: Lang,
        turn_id: Uuid,
        cancel: CancellationToken,
        history: Vec<crate::llm::openai::ChatMessage>,
    ) {
        let started = Instant::now();

        // The acknowledgement is the message every progress update edits, and the
        // one the final answer replaces.
        let ack = self
            .telegram
            .send_message(chat_id, t(Msg::Thinking, lang))
            .await;
        let progress = match ack {
            Ok(message_id) => Some(Arc::new(Progress::new(
                self.telegram.clone(),
                chat_id,
                message_id,
                lang,
                self.config.progress_interval,
            ))),
            Err(e) => {
                tracing::warn!(error = %e, "could not send the acknowledgement message");
                None
            }
        };

        let ctx = TurnCtx::new(chat_id, turn_id, lang, cancel, progress.clone());
        let result = self.agent.run_turn(&ctx, history, &text).await;

        let reply = compose_reply(
            &result.reply,
            &result.published,
            &self.plainsong.all_tracks_url(),
            lang,
        );

        // Edit the acknowledgement into the answer when we can, so a turn leaves
        // one message behind rather than two.
        let delivered = match progress.as_ref() {
            Some(progress) => self
                .telegram
                .edit_message_text(chat_id, progress.message_id(), &reply)
                .await
                .is_ok(),
            None => false,
        };
        if !delivered {
            self.say(chat_id, &reply).await;
        }

        self.sessions.end_turn(chat_id, result.messages).await;
        self.jobs.cleanup(&ctx.job_dirs().await).await;

        tracing::info!(
            iterations = result.iterations,
            tools = %result.tools_called.join(","),
            tokens_in = result.tokens_in,
            tokens_out = result.tokens_out,
            published = result.published.len(),
            escalated = result.escalated,
            outcome = ?result.outcome,
            duration_ms = started.elapsed().as_millis() as u64,
            "turn finished"
        );

        if result.outcome != Outcome::Answered {
            tracing::warn!(outcome = ?result.outcome, "turn ended without an answer from the model");
        }
    }

    async fn say(&self, chat_id: i64, text: &str) {
        if let Err(e) = self.telegram.send_message(chat_id, text).await {
            tracing::warn!(chat_id, error = %e, "could not send a message");
        }
    }
}
