//! Everything wavo says in its own voice, in both supported languages, plus the
//! HTML escaping and truncation every outgoing message goes through.
//!
//! Strings that pass through the LLM are not here — the model mirrors the user's
//! language on its own (§5.5). What is here is what wavo composes itself, which
//! therefore needs its own two-valued lookup rather than an i18n framework.

use crate::session::PublishedTrack;

/// The supported pair. It stays two-valued on purpose (§17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Pl,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Pl => "pl",
        }
    }
}

/// Every string wavo composes without asking the model. Adding a variant forces
/// a Polish and an English rendering, which is what the table test checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    Help,
    TextOnly,
    Busy,
    QueuePosition,
    Thinking,
    Done,
    StageDownloading,
    StageSeparating,
    StageEffects,
    StageUploading,
    StatusIdle,
    StatusBusy,
    StatusQueue,
    StatusUptime,
    TracksEmpty,
    TracksHeader,
    TracksUnavailable,
    ResetDone,
    CancelRequested,
    CancelNothing,
    AllSongs,
    ErrYoutubeBlocked,
    ErrNoResults,
    ErrModelDownload,
    ErrProcessingFailed,
    ErrTooLarge,
    ErrUnsupportedType,
    ErrPublishFailed,
    ErrPathRefused,
    ErrToolUnavailable,
    ErrIterationBudget,
    ErrTurnTimeout,
    ErrJobTimeout,
    ErrLlmUnavailable,
    ErrCancelled,
    ErrUnknownTool,
}

impl Msg {
    /// Used by the table test; also keeps the list in one place.
    pub const ALL: [Msg; 36] = [
        Msg::Help,
        Msg::TextOnly,
        Msg::Busy,
        Msg::QueuePosition,
        Msg::Thinking,
        Msg::Done,
        Msg::StageDownloading,
        Msg::StageSeparating,
        Msg::StageEffects,
        Msg::StageUploading,
        Msg::StatusIdle,
        Msg::StatusBusy,
        Msg::StatusQueue,
        Msg::StatusUptime,
        Msg::TracksEmpty,
        Msg::TracksHeader,
        Msg::TracksUnavailable,
        Msg::ResetDone,
        Msg::CancelRequested,
        Msg::CancelNothing,
        Msg::AllSongs,
        Msg::ErrYoutubeBlocked,
        Msg::ErrNoResults,
        Msg::ErrModelDownload,
        Msg::ErrProcessingFailed,
        Msg::ErrTooLarge,
        Msg::ErrUnsupportedType,
        Msg::ErrPublishFailed,
        Msg::ErrPathRefused,
        Msg::ErrToolUnavailable,
        Msg::ErrIterationBudget,
        Msg::ErrTurnTimeout,
        Msg::ErrJobTimeout,
        Msg::ErrLlmUnavailable,
        Msg::ErrCancelled,
        Msg::ErrUnknownTool,
    ];
}

/// The lookup. `{}` placeholders are filled by the helpers below.
pub fn t(msg: Msg, lang: Lang) -> &'static str {
    match (msg, lang) {
        (Msg::Help, Lang::En) => concat!(
            "🎧 <b>wavo</b> — send me a song, by name or as a YouTube link, ",
            "and what to do with it.\n\n",
            "Examples:\n",
            "• separate vocals from Queen - Bohemian Rhapsody\n",
            "• https://youtu.be/fJ9rUzIMcZQ — instrumental, please\n",
            "• slow down that song to 80%\n",
            "• transpose it to A minor\n\n",
            "Commands: /status /tracks /reset /cancel"
        ),
        (Msg::Help, Lang::Pl) => concat!(
            "🎧 <b>wavo</b> — napisz, jaki utwór (tytuł albo link do YouTube) ",
            "i co z nim zrobić.\n\n",
            "Przykłady:\n",
            "• oddziel wokal od Queen - Bohemian Rhapsody\n",
            "• https://youtu.be/fJ9rUzIMcZQ — poproszę podkład\n",
            "• zwolnij ten utwór do 80%\n",
            "• przetransponuj go do a-moll\n\n",
            "Komendy: /status /tracks /reset /cancel"
        ),

        (Msg::TextOnly, Lang::En) => "I only understand text for now.",
        (Msg::TextOnly, Lang::Pl) => "Na razie rozumiem tylko tekst.",

        (Msg::Busy, Lang::En) => "Still working on the previous request — send it again once I'm done.",
        (Msg::Busy, Lang::Pl) => "Wciąż pracuję nad poprzednią prośbą — napisz jeszcze raz, gdy skończę.",

        (Msg::QueuePosition, Lang::En) => "⏳ Waiting for a free slot — position {n} in the queue.",
        (Msg::QueuePosition, Lang::Pl) => "⏳ Czekam na wolne miejsce — pozycja {n} w kolejce.",

        (Msg::Thinking, Lang::En) => "⏳ Thinking…",
        (Msg::Thinking, Lang::Pl) => "⏳ Myślę…",

        (Msg::Done, Lang::En) => "Done.",
        (Msg::Done, Lang::Pl) => "Gotowe.",

        (Msg::StageDownloading, Lang::En) => "⏳ downloading… ({t})",
        (Msg::StageDownloading, Lang::Pl) => "⏳ pobieram… ({t})",

        (Msg::StageSeparating, Lang::En) => "⏳ separating stems… ({t})",
        (Msg::StageSeparating, Lang::Pl) => "⏳ rozdzielam ścieżki… ({t})",

        (Msg::StageEffects, Lang::En) => "⏳ applying effects… ({t})",
        (Msg::StageEffects, Lang::Pl) => "⏳ nakładam efekty… ({t})",

        (Msg::StageUploading, Lang::En) => "⏳ uploading… ({t})",
        (Msg::StageUploading, Lang::Pl) => "⏳ wysyłam… ({t})",

        (Msg::StatusIdle, Lang::En) => "Idle — nothing is running.",
        (Msg::StatusIdle, Lang::Pl) => "Bezczynny — nic nie jest uruchomione.",

        (Msg::StatusBusy, Lang::En) => "Working on a request from this chat.",
        (Msg::StatusBusy, Lang::Pl) => "Pracuję nad prośbą z tego czatu.",

        (Msg::StatusQueue, Lang::En) => "Jobs running or queued: {n}",
        (Msg::StatusQueue, Lang::Pl) => "Zadania w toku lub w kolejce: {n}",

        (Msg::StatusUptime, Lang::En) => "Uptime: {t}",
        (Msg::StatusUptime, Lang::Pl) => "Czas działania: {t}",

        (Msg::TracksEmpty, Lang::En) => "No tracks published yet.",
        (Msg::TracksEmpty, Lang::Pl) => "Nie opublikowano jeszcze żadnych utworów.",

        (Msg::TracksHeader, Lang::En) => "Ten most recent tracks:",
        (Msg::TracksHeader, Lang::Pl) => "Dziesięć ostatnich utworów:",

        (Msg::TracksUnavailable, Lang::En) => "I can't reach the music storage right now.",
        (Msg::TracksUnavailable, Lang::Pl) => "Nie mogę teraz połączyć się z magazynem muzyki.",

        (Msg::ResetDone, Lang::En) => "Conversation cleared.",
        (Msg::ResetDone, Lang::Pl) => "Historia rozmowy wyczyszczona.",

        (Msg::CancelRequested, Lang::En) => "Stopping after the current step.",
        (Msg::CancelRequested, Lang::Pl) => "Zatrzymam się po bieżącym kroku.",

        (Msg::CancelNothing, Lang::En) => "Nothing is running.",
        (Msg::CancelNothing, Lang::Pl) => "Nic nie jest uruchomione.",

        (Msg::AllSongs, Lang::En) => "all songs",
        (Msg::AllSongs, Lang::Pl) => "wszystkie utwory",

        (Msg::ErrYoutubeBlocked, Lang::En) => {
            "YouTube blocked the download. Try again later, or send a direct link."
        }
        (Msg::ErrYoutubeBlocked, Lang::Pl) => {
            "YouTube zablokował pobieranie. Spróbuj później albo podaj bezpośredni link."
        }

        (Msg::ErrNoResults, Lang::En) => {
            "I couldn't find that song on YouTube — try adding the artist."
        }
        (Msg::ErrNoResults, Lang::Pl) => {
            "Nie znalazłem tego utworu na YouTube — spróbuj dodać wykonawcę."
        }

        (Msg::ErrModelDownload, Lang::En) => {
            "The separation model couldn't be downloaded; retrying later usually works."
        }
        (Msg::ErrModelDownload, Lang::Pl) => {
            "Nie udało się pobrać modelu do rozdzielania ścieżek; zwykle pomaga spróbowanie później."
        }

        (Msg::ErrProcessingFailed, Lang::En) => "Processing failed. {detail}",
        (Msg::ErrProcessingFailed, Lang::Pl) => "Przetwarzanie nie powiodło się. {detail}",

        (Msg::ErrTooLarge, Lang::En) => {
            "That file is too big for the music storage ({n} MB limit) — try a shorter cut."
        }
        (Msg::ErrTooLarge, Lang::Pl) => {
            "Ten plik jest za duży dla magazynu muzyki (limit {n} MB) — spróbuj krótszego fragmentu."
        }

        (Msg::ErrUnsupportedType, Lang::En) => "That file type isn't accepted.",
        (Msg::ErrUnsupportedType, Lang::Pl) => "Ten typ pliku nie jest obsługiwany.",

        (Msg::ErrPublishFailed, Lang::En) => "I couldn't publish the track to the music storage.",
        (Msg::ErrPublishFailed, Lang::Pl) => {
            "Nie udało mi się opublikować utworu w magazynie muzyki."
        }

        (Msg::ErrPathRefused, Lang::En) => {
            "I can only publish files produced during this conversation."
        }
        (Msg::ErrPathRefused, Lang::Pl) => {
            "Mogę publikować tylko pliki powstałe w tej rozmowie."
        }

        (Msg::ErrToolUnavailable, Lang::En) => {
            "The audio processing service is unavailable right now."
        }
        (Msg::ErrToolUnavailable, Lang::Pl) => {
            "Usługa przetwarzania dźwięku jest teraz niedostępna."
        }

        (Msg::ErrIterationBudget, Lang::En) => {
            "Sorry — I went in circles on that one and stopped. Try rephrasing it."
        }
        (Msg::ErrIterationBudget, Lang::Pl) => {
            "Przepraszam — zapętliłem się i przerwałem. Spróbuj sformułować to inaczej."
        }

        (Msg::ErrTurnTimeout, Lang::En) => "Sorry — that took too long and I had to stop.",
        (Msg::ErrTurnTimeout, Lang::Pl) => "Przepraszam — trwało to zbyt długo i musiałem przerwać.",

        (Msg::ErrJobTimeout, Lang::En) => "Processing took too long and was stopped.",
        (Msg::ErrJobTimeout, Lang::Pl) => "Przetwarzanie trwało zbyt długo i zostało przerwane.",

        (Msg::ErrLlmUnavailable, Lang::En) => "I can't think straight right now — try again in a moment.",
        (Msg::ErrLlmUnavailable, Lang::Pl) => "Nie mogę teraz myśleć — spróbuj za chwilę.",

        (Msg::ErrCancelled, Lang::En) => "Stopped. Nothing was published.",
        (Msg::ErrCancelled, Lang::Pl) => "Zatrzymane. Nic nie zostało opublikowane.",

        (Msg::ErrUnknownTool, Lang::En) => "Unknown tool: {name}",
        (Msg::ErrUnknownTool, Lang::Pl) => "Nieznane narzędzie: {name}",
    }
}

/// `t` with `{n}` filled in.
pub fn t_n(msg: Msg, lang: Lang, n: impl std::fmt::Display) -> String {
    t(msg, lang).replace("{n}", &n.to_string())
}

/// `t` with `{t}` filled in.
pub fn t_t(msg: Msg, lang: Lang, value: impl std::fmt::Display) -> String {
    t(msg, lang).replace("{t}", &value.to_string())
}

/// `t` with `{detail}` filled in, and the placeholder dropped when there is none.
pub fn t_detail(msg: Msg, lang: Lang, detail: &str) -> String {
    t(msg, lang)
        .replace("{detail}", detail)
        .trim_end()
        .to_string()
}

/// `t` with `{name}` filled in.
pub fn t_name(msg: Msg, lang: Lang, name: &str) -> String {
    t(msg, lang).replace("{name}", name)
}

// --- language detection -----------------------------------------------------

const PL_MARKERS: [&str; 24] = [
    "nie",
    "tak",
    "jest",
    "proszę",
    "utwór",
    "utworu",
    "piosenkę",
    "piosenka",
    "wokal",
    "wokale",
    "podkład",
    "zwolnij",
    "przyspiesz",
    "oddziel",
    "przetransponuj",
    "tonację",
    "tonacji",
    "ten",
    "tego",
    "mi",
    "daj",
    "wytnij",
    "od",
    "żeby",
];

const EN_MARKERS: [&str; 22] = [
    "the",
    "please",
    "song",
    "track",
    "vocals",
    "instrumental",
    "slow",
    "down",
    "speed",
    "up",
    "separate",
    "transpose",
    "key",
    "give",
    "me",
    "from",
    "that",
    "this",
    "and",
    "with",
    "cut",
    "make",
];

/// Which of the two languages a message is written in, or `None` when the text
/// carries no signal at all (a bare URL, a bare title, an emoji). Callers fall
/// back to the chat's last known language and then to English (§5.5).
pub fn detect_lang(text: &str) -> Option<Lang> {
    let lower = text.to_lowercase();

    // Polish diacritics settle it on their own; no English word carries them.
    if lower.contains(['ą', 'ć', 'ę', 'ł', 'ń', 'ó', 'ś', 'ź', 'ż']) {
        return Some(Lang::Pl);
    }

    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .collect();

    let pl = words.iter().filter(|w| PL_MARKERS.contains(w)).count();
    let en = words.iter().filter(|w| EN_MARKERS.contains(w)).count();

    match pl.cmp(&en) {
        // A mixed-language sentence is decided by its dominant language, so English
        // song titles and stem names inside a Polish sentence keep it Polish (§5.5).
        std::cmp::Ordering::Greater => Some(Lang::Pl),
        std::cmp::Ordering::Less => Some(Lang::En),
        std::cmp::Ordering::Equal => None,
    }
}

// --- outgoing text ----------------------------------------------------------

/// Telegram's hard limit on a message body.
pub const TELEGRAM_MAX_CHARS: usize = 4096;

/// Escape the five characters that can break `parse_mode=HTML`. Every value
/// interpolated into a message goes through this — titles and song names come
/// from YouTube and from the model, and neither is trusted markup.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Cut a message to Telegram's limit on a character boundary, marking the cut.
pub fn truncate_message(text: &str) -> String {
    if text.chars().count() <= TELEGRAM_MAX_CHARS {
        return text.to_string();
    }
    let keep = TELEGRAM_MAX_CHARS - 1;
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

/// Head and tail with a marker in the middle — used on demix stdout/stderr before
/// it goes back into the model's context (§6.4).
pub fn truncate_middle(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let omitted = chars.len() - limit;
    let head = limit / 2;
    let tail = limit - head;
    let mut out: String = chars[..head].iter().collect();
    out.push_str(&format!("\n…[{omitted} chars omitted]…\n"));
    out.extend(chars[chars.len() - tail..].iter());
    out
}

/// The links block wavo appends itself so the model can never invent a URL (§5.4).
pub fn links_block(published: &[PublishedTrack], all_tracks_url: &str, lang: Lang) -> String {
    if published.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for track in published {
        out.push_str(&format!("\n✅ {}", escape_html(&track.title)));
        out.push_str(&format!("\n🎵 {}", escape_html(&track.url)));
    }
    out.push_str(&format!(
        "\n📚 {}: {}",
        t(Msg::AllSongs, lang),
        escape_html(all_tracks_url)
    ));
    out
}

/// `1:05` / `1:02:03`, for progress lines and `/status`.
pub fn human_duration(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_composed_string_exists_in_both_languages() {
        for msg in Msg::ALL {
            let en = t(msg, Lang::En);
            let pl = t(msg, Lang::Pl);
            assert!(!en.trim().is_empty(), "{msg:?} has no English text");
            assert!(!pl.trim().is_empty(), "{msg:?} has no Polish text");
            assert_ne!(en, pl, "{msg:?} is the same in both languages");
        }
    }

    #[test]
    fn placeholders_survive_into_both_variants() {
        for (msg, placeholder) in [
            (Msg::QueuePosition, "{n}"),
            (Msg::StatusQueue, "{n}"),
            (Msg::ErrTooLarge, "{n}"),
            (Msg::StageDownloading, "{t}"),
            (Msg::StatusUptime, "{t}"),
            (Msg::ErrProcessingFailed, "{detail}"),
            (Msg::ErrUnknownTool, "{name}"),
        ] {
            for lang in [Lang::En, Lang::Pl] {
                assert!(
                    t(msg, lang).contains(placeholder),
                    "{msg:?}/{lang:?} lost {placeholder}"
                );
            }
        }
    }

    #[test]
    fn polish_is_detected_by_diacritics_and_by_vocabulary() {
        assert_eq!(detect_lang("oddziel wokal od tego utworu"), Some(Lang::Pl));
        assert_eq!(detect_lang("zwolnij ten utwor do 80%"), Some(Lang::Pl));
        assert_eq!(
            detect_lang("separate the vocals from that song"),
            Some(Lang::En)
        );
        assert_eq!(detect_lang("slow that song down to 80%"), Some(Lang::En));
    }

    #[test]
    fn a_polish_sentence_with_english_terms_stays_polish() {
        assert_eq!(
            detect_lang("oddziel vocals od Queen - Bohemian Rhapsody"),
            Some(Lang::Pl)
        );
    }

    #[test]
    fn a_signal_free_message_has_no_language() {
        assert_eq!(detect_lang("https://youtu.be/abcdef"), None);
        assert_eq!(detect_lang("🎵"), None);
        assert_eq!(detect_lang("Bohemian Rhapsody"), None);
    }

    #[test]
    fn html_special_characters_are_escaped() {
        assert_eq!(
            escape_html("AC/DC <b>\"rock\"</b> & 'roll'"),
            "AC/DC &lt;b&gt;&quot;rock&quot;&lt;/b&gt; &amp; &#39;roll&#39;"
        );
    }

    #[test]
    fn messages_are_cut_to_the_telegram_limit() {
        let long = "ą".repeat(5000);
        let cut = truncate_message(&long);
        assert_eq!(cut.chars().count(), TELEGRAM_MAX_CHARS);
        assert!(cut.ends_with('…'));
        assert_eq!(truncate_message("short"), "short");
    }

    #[test]
    fn tool_output_keeps_its_head_and_tail() {
        let text = format!("{}{}", "A".repeat(100), "B".repeat(100));
        let cut = truncate_middle(&text, 20);
        assert!(cut.starts_with("AAAAAAAAAA"));
        assert!(cut.ends_with("BBBBBBBBBB"));
        assert!(cut.contains("[180 chars omitted]"));
        assert_eq!(truncate_middle("tiny", 20), "tiny");
    }

    #[test]
    fn the_links_block_labels_itself_in_the_users_language() {
        let published = vec![PublishedTrack {
            id: "7f1c".to_string(),
            title: "Bohemian Rhapsody — vocals".to_string(),
            url: "http://music.example.com/track.html?id=7f1c".to_string(),
        }];
        let pl = links_block(&published, "http://music.example.com/", Lang::Pl);
        assert!(pl.contains("wszystkie utwory:"));
        assert!(pl.contains("track.html?id=7f1c"));
        let en = links_block(&published, "http://music.example.com/", Lang::En);
        assert!(en.contains("all songs:"));
        assert!(links_block(&[], "http://music.example.com/", Lang::En).is_empty());
    }

    #[test]
    fn durations_read_as_clock_time() {
        assert_eq!(human_duration(5), "0:05");
        assert_eq!(human_duration(65), "1:05");
        assert_eq!(human_duration(3723), "1:02:03");
    }
}
