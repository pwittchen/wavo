//! Sticking half a request together with the half that follows it (§5.6).
//!
//! People send a link and then say what to do with it, or say it first and paste
//! the link after. Taken one at a time both halves start a turn: the first runs
//! with wavo's defaults before the instruction arrives, the second asks a
//! clarifying question the user is already answering — and whichever runs first
//! makes the other one bounce off the one-turn-per-chat rule (§3.4). So a
//! message missing one of the two halves is held for
//! `WAVO_COALESCE_WINDOW_SEC`, and what accumulates is handed to the model as
//! one message.
//!
//! The word lists below only decide *when to start*, never what to do. A song
//! called "Karaoke" read as an instruction, or an instruction wavo does not know
//! the word for, costs one window of waiting and nothing else: when the window
//! closes, whatever was collected is run as it stands.

use std::collections::HashMap;

use tokio::sync::Mutex;

/// What a chat has said that wavo has not acted on yet.
#[derive(Debug, Default)]
struct Pending {
    parts: Vec<String>,
    /// Bumped by every message. Only the wait holding the newest generation may
    /// run the turn, so an earlier waiter wakes up, sees it was superseded and
    /// leaves the buffer to the message that overtook it.
    generation: u64,
}

/// The per-chat buffers of messages waiting for their other half.
#[derive(Debug, Default)]
pub struct Inbox {
    chats: Mutex<HashMap<i64, Pending>>,
}

/// What to do with the message that just arrived.
#[derive(Debug, PartialEq, Eq)]
pub enum Collected {
    /// Everything needed is here; run this text as one request.
    Ready(String),
    /// A half is missing. Wait one window, then ask for the buffer back with
    /// this generation.
    Waiting(u64),
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a message to its chat's buffer and say whether the request is whole.
    pub async fn push(&self, chat_id: i64, text: &str, song_in_context: bool) -> Collected {
        let mut chats = self.chats.lock().await;
        let pending = chats.entry(chat_id).or_default();
        pending.parts.push(text.trim().to_string());
        pending.generation += 1;

        let joined = pending.parts.join("\n");
        if is_complete(&joined, song_in_context) {
            pending.parts.clear();
            return Collected::Ready(joined);
        }
        Collected::Waiting(pending.generation)
    }

    /// The buffer, once the window has closed — unless a newer message arrived
    /// meanwhile, in which case that one's waiter owns it.
    pub async fn take_after_waiting(&self, chat_id: i64, generation: u64) -> Option<String> {
        let mut chats = self.chats.lock().await;
        let pending = chats.get_mut(&chat_id)?;
        if pending.generation != generation || pending.parts.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut pending.parts).join("\n"))
    }

    /// Drop what a chat was holding — `/reset` clears the conversation and
    /// `/cancel` stops what has not started yet, and half a request is part of
    /// both. True when there was something to drop.
    pub async fn clear(&self, chat_id: i64) -> bool {
        self.chats
            .lock()
            .await
            .remove(&chat_id)
            .is_some_and(|pending| !pending.parts.is_empty())
    }
}

/// Whether this text is a request wavo can start on. It has to name what to work
/// on — a link, a song, or the song already under discussion — *and* say what to
/// do with it.
pub fn is_complete(text: &str, song_in_context: bool) -> bool {
    let words = words_outside_urls(text);
    let names_a_song = words.iter().any(|word| is_name_like(word));
    let says_what_to_do = words.iter().any(|word| is_action_word(word));

    (contains_url(text) || song_in_context || names_a_song) && says_what_to_do
}

fn contains_url(text: &str) -> bool {
    text.split_whitespace().any(is_url)
}

fn is_url(token: &str) -> bool {
    let token = token.to_lowercase();
    token.starts_with("http://")
        || token.starts_with("https://")
        || token.starts_with("www.")
        || token.contains("youtu.be/")
        || token.contains("youtube.com/")
}

/// The words of the message with any link taken out — a URL is full of tokens
/// that would otherwise read as song names.
fn words_outside_urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|token| !is_url(token))
        .flat_map(|token| token.split(|c: char| !c.is_alphanumeric()))
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// A word wavo recognises as neither an instruction nor small change is taken
/// for part of a song or artist name. Numbers ("80", "0:30") never are.
fn is_name_like(word: &str) -> bool {
    word.chars().count() > 1
        && !word.chars().any(|c| c.is_numeric())
        && !is_action_word(word)
        && !FILLER_WORDS.contains(&word)
}

fn is_action_word(word: &str) -> bool {
    ACTION_WORDS.contains(&word)
}

/// What to do with a song, in both languages, plus the short "go ahead" answers
/// to a clarifying question. Stems count: naming one is asking for it.
const ACTION_WORDS: &[&str] = &[
    // Polish
    "usuń",
    "usun",
    "usunąć",
    "wytnij",
    "wycinek",
    "oddziel",
    "rozdziel",
    "zwolnij",
    "spowolnij",
    "wolniej",
    "przyspiesz",
    "przyśpiesz",
    "szybciej",
    "przetransponuj",
    "transponuj",
    "obniż",
    "podnieś",
    "zmień",
    "wokal",
    "wokale",
    "wokalu",
    "wokalem",
    "podkład",
    "podkładu",
    "karaoke",
    "tonacja",
    "tonację",
    "tonacji",
    "tempo",
    "półton",
    "półtony",
    "półtonu",
    "moll",
    "dur",
    "ścieżki",
    "ścieżkę",
    "perkusja",
    "perkusję",
    "bas",
    "basu",
    "gitara",
    "gitarę",
    "pianino",
    "fortepian",
    "tak",
    "dawaj",
    "jasne",
    "okej",
    // English
    "remove",
    "strip",
    "cut",
    "trim",
    "separate",
    "split",
    "isolate",
    "slow",
    "slower",
    "speed",
    "faster",
    "transpose",
    "shift",
    "lower",
    "raise",
    "vocals",
    "vocal",
    "acapella",
    "instrumental",
    "backing",
    "key",
    "bpm",
    "semitone",
    "semitones",
    "stems",
    "stem",
    "drums",
    "bass",
    "guitar",
    "piano",
    "minor",
    "major",
    "down",
    "up",
    "yes",
    "yeah",
    "sure",
    "ok",
    "go",
    "proceed",
];

/// Words that carry no request of their own: politeness, articles, prepositions
/// and the words for "song". They must not be mistaken for a song's name.
const FILLER_WORDS: &[&str] = &[
    // Polish
    "proszę",
    "prosze",
    "poproszę",
    "poprosze",
    "daj",
    "zrób",
    "zrob",
    "chcę",
    "chce",
    "mi",
    "mnie",
    "dla",
    "to",
    "tego",
    "ten",
    "ta",
    "te",
    "tej",
    "tym",
    "ze",
    "do",
    "na",
    "od",
    "bez",
    "jest",
    "jak",
    "tylko",
    "teraz",
    "potem",
    "jeszcze",
    "raz",
    "też",
    "tez",
    "także",
    "takze",
    "ale",
    "oraz",
    "utwór",
    "utworu",
    "piosenka",
    "piosenkę",
    "piosenki",
    "kawałek",
    "nagranie",
    "plik",
    "link",
    "tutaj",
    "tu",
    // English
    "please",
    "give",
    "make",
    "get",
    "me",
    "my",
    "the",
    "an",
    "this",
    "that",
    "it",
    "its",
    "from",
    "of",
    "to",
    "in",
    "into",
    "on",
    "at",
    "and",
    "or",
    "with",
    "without",
    "for",
    "song",
    "track",
    "file",
    "version",
    "now",
    "again",
    "also",
    "then",
    "just",
    "some",
    "one",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_with_an_instruction_is_a_whole_request() {
        for text in [
            "https://youtu.be/fJ9rUzIMcZQ usuń wokal",
            "https://www.youtube.com/watch?v=fJ9rUzIMcZQ — poproszę podkład",
            "slow https://youtu.be/fJ9rUzIMcZQ down to 80%",
            "oddziel wokal od Queen - Bohemian Rhapsody",
            "separate the vocals from Queen - Bohemian Rhapsody",
        ] {
            assert!(is_complete(text, false), "{text} was held back");
        }
    }

    #[test]
    fn half_a_request_waits_for_the_other_half() {
        for text in [
            // A source with nothing to do to it…
            "https://youtu.be/fJ9rUzIMcZQ",
            "  https://music.youtube.com/watch?v=x&list=y  ",
            "Queen - Bohemian Rhapsody",
            // …and something to do with no source to do it to.
            "usuń wokal",
            "zwolnij do 80%",
            "separate the vocals",
        ] {
            assert!(!is_complete(text, false), "{text} started on its own");
        }
    }

    #[test]
    fn a_song_already_under_discussion_counts_as_the_source() {
        // The same follow-up is whole in a conversation and half a request in a
        // fresh one, which is what keeps follow-ups instant.
        assert!(is_complete("zwolnij do 80%", true));
        assert!(is_complete("teraz podkład", true));
        assert!(!is_complete("zwolnij do 80%", false));
        // "teraz" is a word wavo knows, so it is not read as a song's name.
        assert!(!is_complete("teraz podkład", false));
        // A new link is still a new song, and still says nothing about what to do.
        assert!(!is_complete("https://youtu.be/fJ9rUzIMcZQ", true));
    }

    #[tokio::test]
    async fn two_messages_are_joined_into_one_request() {
        let inbox = Inbox::new();
        let first = inbox.push(7, "https://youtu.be/fJ9rUzIMcZQ", false).await;
        assert_eq!(first, Collected::Waiting(1));

        let second = inbox.push(7, "usuń wokal", false).await;
        assert_eq!(
            second,
            Collected::Ready("https://youtu.be/fJ9rUzIMcZQ\nusuń wokal".to_string())
        );

        // The first waiter wakes up to find its message already spoken for.
        assert_eq!(inbox.take_after_waiting(7, 1).await, None);
    }

    #[tokio::test]
    async fn the_other_order_is_joined_the_same_way() {
        let inbox = Inbox::new();
        assert_eq!(
            inbox.push(7, "usuń wokal", false).await,
            Collected::Waiting(1)
        );
        assert_eq!(
            inbox.push(7, "https://youtu.be/fJ9rUzIMcZQ", false).await,
            Collected::Ready("usuń wokal\nhttps://youtu.be/fJ9rUzIMcZQ".to_string())
        );
    }

    #[tokio::test]
    async fn a_half_request_nobody_completes_is_run_as_it_stands() {
        let inbox = Inbox::new();
        assert_eq!(
            inbox.push(7, "https://youtu.be/fJ9rUzIMcZQ", false).await,
            Collected::Waiting(1)
        );
        assert_eq!(
            inbox.take_after_waiting(7, 1).await.as_deref(),
            Some("https://youtu.be/fJ9rUzIMcZQ")
        );
        // And it is handed out exactly once.
        assert_eq!(inbox.take_after_waiting(7, 1).await, None);
    }

    #[tokio::test]
    async fn a_newer_message_takes_over_the_buffer() {
        let inbox = Inbox::new();
        inbox.push(7, "https://youtu.be/fJ9rUzIMcZQ", false).await;
        let second = inbox.push(7, "and the other one too", false).await;
        assert_eq!(second, Collected::Waiting(2));

        assert_eq!(inbox.take_after_waiting(7, 1).await, None);
        assert_eq!(
            inbox.take_after_waiting(7, 2).await.as_deref(),
            Some("https://youtu.be/fJ9rUzIMcZQ\nand the other one too")
        );
    }

    #[tokio::test]
    async fn chats_do_not_collect_into_each_other() {
        let inbox = Inbox::new();
        inbox.push(7, "https://youtu.be/aaa", false).await;
        inbox.push(8, "https://youtu.be/bbb", false).await;
        assert_eq!(
            inbox.take_after_waiting(7, 1).await.as_deref(),
            Some("https://youtu.be/aaa")
        );
        assert_eq!(
            inbox.take_after_waiting(8, 1).await.as_deref(),
            Some("https://youtu.be/bbb")
        );
    }

    #[tokio::test]
    async fn a_reset_drops_what_was_waiting() {
        let inbox = Inbox::new();
        inbox.push(7, "https://youtu.be/fJ9rUzIMcZQ", false).await;
        assert!(inbox.clear(7).await, "there was a half-request to drop");
        assert_eq!(inbox.take_after_waiting(7, 1).await, None);
        // Nothing waiting, nothing dropped — what /cancel reports on.
        assert!(!inbox.clear(7).await);
    }
}
