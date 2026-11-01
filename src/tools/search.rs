//! Matching a request against the titles in the collection (§6.3).
//!
//! People ask for a song the way they remember it: a fragment, a word too many,
//! a typo, a Polish case ending. So a search walks down three steps and stops at
//! the first that finds anything:
//!
//! 1. every query word appears in a word of the title;
//! 2. every query word appears or nearly does (a typo, an inflected form, words
//!    written together);
//! 3. only some of them do. These are suggestions and not matches: they are told
//!    to the model and never linked, because "Maryla Rodowicz Sing Sing" sharing
//!    a performer with "Maryla Rodowicz — Damą być" does not make it that song.

use super::plainsong::{fold_char, Track};

/// The most suggestions one search hands the model.
const MAX_SIMILAR: usize = 5;

/// Query words shorter than this are compared exactly: at two or three letters
/// one changed letter is a different word.
const MIN_FUZZY_CHARS: usize = 4;

#[derive(Debug, Default)]
pub struct TrackSearch {
    /// What the search found, newest first. These get links.
    pub matches: Vec<Track>,
    /// Filled only when nothing matched: the nearest titles, closest first.
    pub similar: Vec<Track>,
}

/// Search `tracks`, which plainsong lists newest first. A query with no words in
/// it finds nothing.
pub fn search(query: &str, tracks: Vec<Track>) -> TrackSearch {
    let words = search_words(query);
    if words.is_empty() {
        return TrackSearch::default();
    }

    for matches in [Title::contains, Title::resembles] {
        let found: Vec<Track> = tracks
            .iter()
            .filter(|track| {
                let title = Title::new(&track.title);
                words.iter().all(|word| matches(&title, word))
            })
            .cloned()
            .collect();
        if !found.is_empty() {
            return TrackSearch {
                matches: found,
                similar: Vec::new(),
            };
        }
    }

    TrackSearch {
        matches: Vec::new(),
        similar: similar(&words, tracks),
    }
}

/// The titles sharing at least two fifths of the query, by letters, and at least
/// one real word. Only the part before any bracket counts: "(bez wokalu)" is on
/// nearly every title, and two shared words of it say nothing about the song.
fn similar(words: &[String], tracks: Vec<Track>) -> Vec<Track> {
    let total: usize = words.iter().map(|word| word.chars().count()).sum();
    let mut scored: Vec<(usize, Track)> = tracks
        .into_iter()
        .filter_map(|track| {
            let song = track.title.split(['(', '[']).next().unwrap_or_default();
            let song = Title::new(song);
            let shared = words.iter().filter(|word| song.resembles(word));
            let lengths: Vec<usize> = shared.map(|word| word.chars().count()).collect();
            let matched: usize = lengths.iter().sum();
            let has_real_word = lengths.iter().any(|&length| length >= MIN_FUZZY_CHARS);
            (has_real_word && matched * 5 >= total * 2).then_some((matched, track))
        })
        .collect();

    // Stable, so equally near titles stay newest first.
    scored.sort_by_key(|(matched, _)| std::cmp::Reverse(*matched));
    scored
        .into_iter()
        .take(MAX_SIMILAR)
        .map(|(_, track)| track)
        .collect()
}

/// A title as a search reads it.
struct Title {
    words: Vec<String>,
    /// The words run together, for "stopklatka" against "Stop Klatka".
    joined: String,
}

impl Title {
    fn new(text: &str) -> Self {
        let words = search_words(text);
        let joined = words.concat();
        Self { words, joined }
    }

    fn contains(&self, query: &str) -> bool {
        self.words.iter().any(|word| word.contains(query))
    }

    fn resembles(&self, query: &str) -> bool {
        self.contains(query)
            || (query.chars().count() >= MIN_FUZZY_CHARS && self.joined.contains(query))
            || self.words.iter().any(|word| nearly_equal(query, word))
    }
}

/// Whether two words are one word misspelt or inflected. The first letter has to
/// agree: it is rarely the one mistyped, and without this "sama" finds "dama".
fn nearly_equal(query: &str, word: &str) -> bool {
    let query: Vec<char> = query.chars().collect();
    let word: Vec<char> = word.chars().collect();
    if query.len() < MIN_FUZZY_CHARS || word.len() < MIN_FUZZY_CHARS || query[0] != word[0] {
        return false;
    }
    let allowed_edits = if query.len() >= 8 { 2 } else { 1 };
    shares_stem(&query, &word) || edit_distance(&query, &word) <= allowed_edits
}

/// Polish words change their ending by more than a typo does: "zakochanych" is
/// three edits from "zakochani". A long enough shared start covers that.
fn shares_stem(a: &[char], b: &[char]) -> bool {
    let shared = a.iter().zip(b).take_while(|(a, b)| a == b).count();
    shared * 10 >= a.len().max(b.len()) * 7
}

/// Edits between two words, with two swapped neighbours counting as one: "Madga"
/// is one slip away from "Magda".
fn edit_distance(a: &[char], b: &[char]) -> usize {
    let mut before_previous: Vec<usize> = vec![0; b.len() + 1];
    let mut previous: Vec<usize> = (0..=b.len()).collect();

    for (i, a_char) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, b_char) in b.iter().enumerate() {
            let cost = usize::from(a_char != b_char);
            let mut best = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
            if i > 0 && j > 0 && *a_char == b[j - 1] && a[i - 1] == *b_char {
                best = best.min(before_previous[j - 1] + 1);
            }
            current.push(best);
        }
        before_previous = std::mem::replace(&mut previous, current);
    }
    previous[b.len()]
}

/// A text as the words a search compares: diacritics folded, lowercased, split on
/// anything that is not a letter or a digit. Decomposed accents (`o` followed by
/// a combining mark) are dropped, or they would split a word in two.
fn search_words(text: &str) -> Vec<String> {
    let mut folded = String::new();
    for c in text.chars() {
        match fold_char(c) {
            Some(letters) => folded.push_str(letters),
            None if ('\u{0300}'..='\u{036f}').contains(&c) => {}
            None => folded.push(c),
        }
    }
    folded
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection() -> Vec<Track> {
        [
            "Kult — Żółw (oryginał)",
            "Queen — Bohemian Rhapsody (bez wokalu)",
            "Maryla Rodowicz - Damą być (bez wokalu)",
            "Danuta Rinn - Gdzie ci mężczyźni (bez wokalu), tempo: 0.9",
            "Madga Umer - Już szumią kasztany (bez wokalu)",
            "Stanisław Soyka - Świat (bez wokalu)",
            "Stop Klatka (bez wokalu)",
            "Wanda Warska - Oczy masz niebiesko-zielone (bez wokalu)",
            "Krystyna Konarska - Zakochani są wśród nas (bez wokalu) [tempo: 0.9]",
            "Sława Przybylska - Chryzantemy złociste (bez wokalu)",
            "Guns N' Roses — Don't Cry (bez wokalu)",
        ]
        .iter()
        .enumerate()
        .map(|(id, title)| Track {
            id: id.to_string(),
            title: title.to_string(),
            filename: String::new(),
            size_bytes: 0,
        })
        .collect()
    }

    fn matched(query: &str) -> Vec<String> {
        titles(search(query, collection()).matches)
    }

    fn suggested(query: &str) -> Vec<String> {
        let result = search(query, collection());
        assert!(result.matches.is_empty(), "{query} matched");
        titles(result.similar)
    }

    fn titles(tracks: Vec<Track>) -> Vec<String> {
        tracks.into_iter().map(|track| track.title).collect()
    }

    #[test]
    fn words_match_in_any_order_past_punctuation_and_diacritics() {
        for query in [
            "queen bohemian rhapsody",
            "Bohemian Rhapsody Queen",
            "bohemian",
            "rhaps",
            "queen bez wokalu",
        ] {
            assert_eq!(matched(query).len(), 1, "{query} missed");
        }
        assert_eq!(matched("zolw oryginal").len(), 1);
        assert_eq!(matched("ŻÓŁW").len(), 1);
    }

    #[test]
    fn a_decomposed_accent_does_not_split_a_word() {
        assert_eq!(search_words("z\u{307}o\u{301}łw"), ["zolw"]);
    }

    #[test]
    fn a_title_stored_with_decomposed_accents_is_found() {
        // Six of the 53 titles in production were stored like this.
        let tracks = vec![Track {
            title: "Halina Kunicka - S\u{301}wiat nie jest taki zły (bez wokalu)".into(),
            ..collection()[0].clone()
        }];
        assert_eq!(search("Świat nie jest taki zły", tracks).matches.len(), 1);
    }

    #[test]
    fn a_fragment_of_a_word_is_a_match() {
        assert_eq!(matched("kaszt").len(), 1);
        assert_eq!(matched("zolw").len(), 1);
    }

    #[test]
    fn a_typo_still_finds_the_song() {
        for query in [
            "Bohemian Rapsody",
            "bohemain rhapsody",
            "Magda Umer Już szumią kasztany",
            "Sojka Świat",
            "Chryzantemy zlociste Przybyslka",
        ] {
            assert_eq!(matched(query).len(), 1, "{query} missed");
        }
    }

    #[test]
    fn an_inflected_form_still_finds_the_song() {
        for query in ["Soyki Świat", "zakochanych", "kasztanach"] {
            assert_eq!(matched(query).len(), 1, "{query} missed");
        }
    }

    #[test]
    fn words_written_together_or_apart_find_the_song() {
        for query in [
            "stopklatka",
            "niebieskozielone",
            "niebiesko zielone",
            "dont cry",
        ] {
            assert_eq!(matched(query).len(), 1, "{query} missed");
        }
    }

    #[test]
    fn a_literal_match_beats_a_near_one() {
        let mut tracks = collection();
        tracks.push(Track {
            id: "near".into(),
            title: "Ktoś — Świata (bez wokalu)".into(),
            ..tracks[0].clone()
        });
        let found = titles(search("Świat Soyka", tracks).matches);
        assert_eq!(found, ["Stanisław Soyka - Świat (bez wokalu)"]);
    }

    #[test]
    fn a_nearly_right_title_is_suggested_and_not_matched() {
        assert_eq!(
            suggested("Gdzie są ci mężczyźni")[0],
            "Danuta Rinn - Gdzie ci mężczyźni (bez wokalu), tempo: 0.9"
        );
        assert_eq!(
            suggested("Chryzantemy złote")[0],
            "Sława Przybylska - Chryzantemy złociste (bez wokalu)"
        );
        assert_eq!(
            suggested("Kabaret Starszych Panów Zakochani są wśród nas")[0],
            "Krystyna Konarska - Zakochani są wśród nas (bez wokalu) [tempo: 0.9]"
        );
    }

    #[test]
    fn a_shared_performer_is_only_ever_a_suggestion() {
        assert!(matched("Maryla Rodowicz Sing Sing").is_empty());
        assert!(matched("queen radio ga ga").is_empty());
    }

    #[test]
    fn the_modification_makes_nothing_similar() {
        assert!(suggested("metallica bez wokalu").is_empty());
    }

    #[test]
    fn an_unrelated_query_finds_and_suggests_nothing() {
        assert!(suggested("metallica").is_empty());
        assert!(suggested("sama").is_empty(), "one letter off is not a word");
    }

    #[test]
    fn a_query_with_no_words_finds_nothing() {
        let result = search(" — ", collection());
        assert!(result.matches.is_empty() && result.similar.is_empty());
    }

    #[test]
    fn suggestions_are_capped() {
        let tracks: Vec<Track> = (0..20)
            .map(|n| Track {
                id: n.to_string(),
                title: format!("Queen — Song {n}"),
                ..collection()[0].clone()
            })
            .collect();
        let result = search("queen radio", tracks);
        assert_eq!(result.similar.len(), MAX_SIMILAR);
        assert_eq!(result.similar[0].id, "0", "newest first among equals");
    }

    #[test]
    fn edit_distance_counts_a_swap_as_one() {
        let chars = |word: &str| word.chars().collect::<Vec<_>>();
        assert_eq!(edit_distance(&chars("magda"), &chars("madga")), 1);
        assert_eq!(edit_distance(&chars("soyka"), &chars("sojka")), 1);
        assert_eq!(edit_distance(&chars("kot"), &chars("kot")), 0);
        assert_eq!(edit_distance(&chars("abcd"), &chars("wxyz")), 4);
    }
}
