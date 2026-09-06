//! Per-chat conversation state, held in memory and never persisted (§4).
//! A restart starts every conversation fresh; that is the deliberate trade.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::llm::openai::{ChatMessage, Role};
use crate::telegram::format::{detect_lang, Lang};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedTrack {
    pub id: String,
    pub title: String,
    pub url: String,
}

#[derive(Debug)]
pub struct Session {
    pub messages: Vec<ChatMessage>,
    pub last_activity: Instant,
    pub busy: bool,
    /// The last language this chat was recognised as writing in, used when the
    /// current message carries no signal of its own (§5.5).
    pub lang: Option<Lang>,
    pub cancel: Option<CancellationToken>,
}

impl Session {
    fn new(system_prompt: &str) -> Self {
        Self {
            messages: vec![ChatMessage::system(system_prompt)],
            last_activity: Instant::now(),
            busy: false,
            lang: None,
            cancel: None,
        }
    }
}

pub struct Sessions {
    inner: RwLock<HashMap<i64, Session>>,
    system_prompt: String,
    history_turns: usize,
    ttl: Duration,
}

/// What a chat may do with the message that just arrived.
pub enum TurnStart {
    /// The turn is ours: here is the history so far and the token `/cancel` flips.
    Ready {
        history: Vec<ChatMessage>,
        cancel: CancellationToken,
    },
    /// A turn from this chat is already in flight; the message is dropped, not
    /// queued (§3.4).
    Busy,
}

impl Sessions {
    pub fn new(system_prompt: impl Into<String>, history_turns: usize, ttl: Duration) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            system_prompt: system_prompt.into(),
            history_turns,
            ttl,
        }
    }

    /// The language to answer a message in: what the message itself shows, else
    /// what this chat used last, else English. Detection results are remembered.
    pub async fn language_for(&self, chat_id: i64, text: &str) -> Lang {
        let detected = detect_lang(text);
        let mut sessions = self.inner.write().await;
        let session = sessions
            .entry(chat_id)
            .or_insert_with(|| Session::new(&self.system_prompt));
        match detected {
            Some(lang) => {
                session.lang = Some(lang);
                lang
            }
            None => session.lang.unwrap_or(Lang::En),
        }
    }

    pub async fn begin_turn(&self, chat_id: i64) -> TurnStart {
        let mut sessions = self.inner.write().await;
        let session = sessions
            .entry(chat_id)
            .or_insert_with(|| Session::new(&self.system_prompt));
        if session.busy {
            return TurnStart::Busy;
        }
        let cancel = CancellationToken::new();
        session.busy = true;
        session.cancel = Some(cancel.clone());
        session.last_activity = Instant::now();
        TurnStart::Ready {
            history: session.messages.clone(),
            cancel,
        }
    }

    /// Store the messages the turn produced and release the chat.
    pub async fn end_turn(&self, chat_id: i64, messages: Vec<ChatMessage>) {
        let mut sessions = self.inner.write().await;
        let Some(session) = sessions.get_mut(&chat_id) else {
            return;
        };
        session.messages = messages;
        trim_history(&mut session.messages, self.history_turns);
        session.busy = false;
        session.cancel = None;
        session.last_activity = Instant::now();
    }

    pub async fn reset(&self, chat_id: i64) {
        let mut sessions = self.inner.write().await;
        if let Some(session) = sessions.get_mut(&chat_id) {
            let lang = session.lang;
            session.messages = vec![ChatMessage::system(&self.system_prompt)];
            session.lang = lang;
            session.last_activity = Instant::now();
        }
    }

    /// True when there was something to cancel.
    pub async fn cancel(&self, chat_id: i64) -> bool {
        let sessions = self.inner.read().await;
        match sessions.get(&chat_id) {
            Some(session) if session.busy => {
                if let Some(token) = &session.cancel {
                    token.cancel();
                }
                true
            }
            _ => false,
        }
    }

    /// Whether this chat has already discussed a song, which is what lets a bare
    /// follow-up ("zwolnij do 80%") count as a whole request (§5.6).
    pub async fn has_song_context(&self, chat_id: i64) -> bool {
        self.inner
            .read()
            .await
            .get(&chat_id)
            .is_some_and(|session| {
                session
                    .messages
                    .iter()
                    .any(|message| message.role == Role::User)
            })
    }

    pub async fn busy_here(&self, chat_id: i64) -> bool {
        self.inner
            .read()
            .await
            .get(&chat_id)
            .is_some_and(|session| session.busy)
    }

    pub async fn busy_count(&self) -> usize {
        self.inner
            .read()
            .await
            .values()
            .filter(|session| session.busy)
            .count()
    }

    /// Drop sessions nobody has touched for `WAVO_SESSION_TTL_MIN` minutes.
    pub async fn evict_idle(&self) -> usize {
        let mut sessions = self.inner.write().await;
        let before = sessions.len();
        sessions.retain(|_, session| session.busy || session.last_activity.elapsed() < self.ttl);
        before - sessions.len()
    }
}

/// Keep the system prompt, the last `turns` user/assistant pairs, and full tool
/// results only for the most recent two turns (§4). Trimming always cuts at a
/// user message so an assistant's `tool_calls` never loses its `tool` replies —
/// the API rejects that shape.
pub fn trim_history(messages: &mut Vec<ChatMessage>, turns: usize) {
    let user_positions: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .map(|(i, _)| i)
        .collect();

    if user_positions.len() > turns {
        let cut_from = user_positions[user_positions.len() - turns];
        let keep_system = matches!(messages.first(), Some(m) if m.role == Role::System);
        let tail: Vec<ChatMessage> = messages.split_off(cut_from);
        messages.truncate(if keep_system { 1 } else { 0 });
        messages.extend(tail);
    }

    // demix output is the bulkiest thing in a history and the least useful once
    // the model has acted on it, so older tool blocks keep only their verdict.
    let user_positions: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .map(|(i, _)| i)
        .collect();
    let recent_from = user_positions
        .len()
        .checked_sub(2)
        .map(|i| user_positions[i])
        .unwrap_or(0);

    for message in messages.iter_mut().take(recent_from) {
        if message.role != Role::Tool {
            continue;
        }
        let summary = summarize_tool_result(message.content.as_deref().unwrap_or(""));
        if message.content.as_deref().map(str::len).unwrap_or(0) > summary.len() {
            message.content = Some(summary);
        }
    }
}

fn summarize_tool_result(content: &str) -> String {
    let ok = serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|value| value.get("ok").and_then(Value::as_bool));
    match ok {
        Some(true) => "[earlier tool result omitted — it succeeded]".to_string(),
        Some(false) => "[earlier tool result omitted — it failed]".to_string(),
        None => "[earlier tool result omitted]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::openai::{FunctionCall, ToolCall};

    fn tool_call_message(id: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: id.to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "process_audio".to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
            tool_call_id: None,
        }
    }

    fn conversation(turns: usize) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::system("system")];
        for i in 0..turns {
            messages.push(ChatMessage::user(format!("request {i}")));
            messages.push(tool_call_message(&format!("call_{i}")));
            messages.push(ChatMessage::tool_result(
                format!("call_{i}"),
                format!("{{\"ok\":true,\"stdout\":\"{}\"}}", "x".repeat(200)),
            ));
            messages.push(ChatMessage::assistant(format!("answer {i}")));
        }
        messages
    }

    #[test]
    fn history_keeps_the_system_prompt_and_the_last_n_turns() {
        let mut messages = conversation(5);
        trim_history(&mut messages, 2);

        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[1].content.as_deref(), Some("request 3"));
        let users = messages.iter().filter(|m| m.role == Role::User).count();
        assert_eq!(users, 2);
    }

    #[test]
    fn trimming_never_orphans_a_tool_result() {
        let mut messages = conversation(5);
        trim_history(&mut messages, 3);

        // Every `tool` message must be preceded (eventually) by the assistant
        // message carrying its call id, or the API rejects the request.
        let mut announced: Vec<String> = Vec::new();
        for message in &messages {
            for call in message.tool_calls() {
                announced.push(call.id.clone());
            }
            if let Some(id) = &message.tool_call_id {
                assert!(announced.contains(id), "orphaned tool result {id}");
            }
        }
    }

    #[test]
    fn older_tool_results_shrink_to_one_line() {
        let mut messages = conversation(4);
        trim_history(&mut messages, 4);

        let tool_bodies: Vec<&str> = messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.as_deref().unwrap_or(""))
            .collect();

        assert_eq!(tool_bodies.len(), 4);
        assert!(tool_bodies[0].contains("omitted"));
        assert!(tool_bodies[1].contains("omitted"));
        // The two most recent turns keep their full output.
        assert!(tool_bodies[3].contains("xxxx"));
    }

    #[test]
    fn a_short_history_is_left_alone() {
        let mut messages = conversation(1);
        let before = messages.clone();
        trim_history(&mut messages, 12);
        assert_eq!(messages.len(), before.len());
        assert_eq!(messages[3].content, before[3].content);
    }

    #[tokio::test]
    async fn a_chat_runs_one_turn_at_a_time() {
        let sessions = Sessions::new("system", 12, Duration::from_secs(60));
        assert!(matches!(
            sessions.begin_turn(7).await,
            TurnStart::Ready { .. }
        ));
        assert!(matches!(sessions.begin_turn(7).await, TurnStart::Busy));
        // A different chat is unaffected.
        assert!(matches!(
            sessions.begin_turn(8).await,
            TurnStart::Ready { .. }
        ));

        sessions
            .end_turn(7, vec![ChatMessage::system("system")])
            .await;
        assert!(matches!(
            sessions.begin_turn(7).await,
            TurnStart::Ready { .. }
        ));
    }

    #[tokio::test]
    async fn language_falls_back_to_the_chats_last_known_one() {
        let sessions = Sessions::new("system", 12, Duration::from_secs(60));
        assert_eq!(sessions.language_for(1, "oddziel wokal").await, Lang::Pl);
        // A bare link carries no signal, so the chat's Polish carries over.
        assert_eq!(
            sessions.language_for(1, "https://youtu.be/x").await,
            Lang::Pl
        );
        assert_eq!(
            sessions.language_for(1, "slow that song down please").await,
            Lang::En
        );
        // An unknown chat with no history answers in English.
        assert_eq!(sessions.language_for(2, "🎵").await, Lang::En);
    }

    #[tokio::test]
    async fn cancel_reports_whether_anything_was_running() {
        let sessions = Sessions::new("system", 12, Duration::from_secs(60));
        assert!(!sessions.cancel(1).await);

        let TurnStart::Ready { cancel, .. } = sessions.begin_turn(1).await else {
            panic!("expected a free chat");
        };
        assert!(sessions.cancel(1).await);
        assert!(cancel.is_cancelled());
    }
}
