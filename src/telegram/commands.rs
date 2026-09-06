//! The handful of commands wavo answers without involving the LLM (§5.2).

use std::time::Duration;

use crate::telegram::format::{escape_html, human_duration, t, t_n, t_t, Lang, Msg};
use crate::tools::plainsong::Track;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Help,
    Status,
    Tracks,
    Reset,
    Cancel,
}

/// `/help`, `/help@wavobot` and `/help something` are all the same command.
/// Anything else is a natural-language request for the model.
pub fn parse(text: &str) -> Option<Command> {
    let first = text.split_whitespace().next()?;
    let name = first.strip_prefix('/')?;
    let name = name.split('@').next().unwrap_or(name).to_lowercase();
    match name.as_str() {
        "start" | "help" => Some(Command::Help),
        "status" => Some(Command::Status),
        "tracks" => Some(Command::Tracks),
        "reset" => Some(Command::Reset),
        "cancel" => Some(Command::Cancel),
        _ => None,
    }
}

pub fn help(lang: Lang) -> String {
    t(Msg::Help, lang).to_string()
}

pub fn status(lang: Lang, busy_here: bool, pending_jobs: usize, uptime: Duration) -> String {
    let first = if busy_here {
        t(Msg::StatusBusy, lang)
    } else {
        t(Msg::StatusIdle, lang)
    };
    format!(
        "{first}\n{}\n{}",
        t_n(Msg::StatusQueue, lang, pending_jobs),
        t_t(Msg::StatusUptime, lang, human_duration(uptime.as_secs()))
    )
}

/// The ten most recent tracks. plainsong appends new tracks, so the tail is the
/// recent end.
pub fn tracks(lang: Lang, tracks: &[Track], track_url: impl Fn(&str) -> String) -> String {
    if tracks.is_empty() {
        return t(Msg::TracksEmpty, lang).to_string();
    }
    let mut out = t(Msg::TracksHeader, lang).to_string();
    for track in tracks.iter().rev().take(10) {
        out.push_str(&format!(
            "\n• {} — {}",
            escape_html(&track.title),
            escape_html(&track_url(&track.id))
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_recognised_with_and_without_the_bot_suffix() {
        assert_eq!(parse("/help"), Some(Command::Help));
        assert_eq!(parse("/start"), Some(Command::Help));
        assert_eq!(parse("  /status@wavobot  "), Some(Command::Status));
        assert_eq!(parse("/TRACKS"), Some(Command::Tracks));
        assert_eq!(parse("/cancel now"), Some(Command::Cancel));
        assert_eq!(parse("/reset"), Some(Command::Reset));
    }

    #[test]
    fn anything_else_is_a_request_for_the_model() {
        assert_eq!(parse("slow it down"), None);
        assert_eq!(parse("what/now"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("/unknown"), None);
    }

    #[test]
    fn help_follows_the_language_of_the_triggering_message() {
        assert!(help(Lang::Pl).contains("Komendy"));
        assert!(help(Lang::En).contains("Commands"));
    }

    #[test]
    fn status_reports_what_is_running_in_both_languages() {
        let busy = status(Lang::En, true, 1, Duration::from_secs(65));
        assert!(busy.contains("Working on a request"));
        assert!(busy.contains("1:05"));

        let idle = status(Lang::Pl, false, 0, Duration::from_secs(3600));
        assert!(idle.contains("Bezczynny"));
        assert!(idle.contains("1:00:00"));
    }

    #[test]
    fn the_track_listing_escapes_titles_and_caps_at_ten() {
        let listed: Vec<Track> = (0..15)
            .map(|i| Track {
                id: format!("id{i}"),
                title: format!("<b>song {i}</b>"),
                filename: String::new(),
                size_bytes: 0,
            })
            .collect();
        let rendered = tracks(Lang::En, &listed, |id| {
            format!("http://x/track.html?id={id}")
        });

        assert_eq!(rendered.lines().count(), 11, "header plus ten tracks");
        assert!(rendered.contains("&lt;b&gt;song 14&lt;/b&gt;"));
        assert!(!rendered.contains("song 4"), "older tracks are dropped");
        assert_eq!(
            tracks(Lang::Pl, &[], |_| String::new()),
            t(Msg::TracksEmpty, Lang::Pl)
        );
    }
}
