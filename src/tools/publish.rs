//! The path guard and the title wavo composes. `publish_track` is the one tool
//! that turns a string from the model into an open file, so this is where the
//! model's reach ends (§12) — and the one place a stored title is written, so
//! this is also where its three parts are joined (§7).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::PathError;
use crate::telegram::format::{t, Lang, Msg};
use crate::tools::plainsong::is_audio_file;

/// Resolve what the model asked to publish to a real file, or refuse.
///
/// The only accepted names are the ones wavo itself put in the turn's path table:
/// a relative key as shown to the model, or the absolute path that key maps to.
/// Anything else — `..`, `/etc/passwd`, a plausible-looking sibling of a real
/// output — is refused before the file is opened. The final containment check is
/// made on the *canonical* path, so a symlink pointing out of the job directory
/// is refused too.
pub fn resolve_publish_path(
    table: &HashMap<String, PathBuf>,
    requested: &str,
    jobs_root: &Path,
) -> Result<PathBuf, PathError> {
    let requested = requested.trim();

    let candidate = table
        .get(requested)
        .cloned()
        .or_else(|| {
            table
                .values()
                .find(|path| path.as_os_str() == requested)
                .cloned()
        })
        .ok_or_else(|| PathError::NotInTable(requested.to_string()))?;

    let real = candidate
        .canonicalize()
        .map_err(|_| PathError::Missing(candidate.clone()))?;

    // The root itself may be a symlink (a bind-mounted /work on macOS is), so
    // both sides are canonicalized before they are compared.
    let root = jobs_root
        .canonicalize()
        .unwrap_or_else(|_| jobs_root.to_path_buf());
    if !real.starts_with(&root) {
        return Err(PathError::Escapes(real));
    }

    let filename = real
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    if !is_audio_file(&filename) {
        return Err(PathError::NotAudio(filename));
    }

    Ok(real)
}

/// The title a track carries in plainsong: who performs it, what it is, and what
/// wavo did to it — `Artist — Title (modification)`.
///
/// The model supplies the three parts separately and wavo joins them, for the
/// same reason it composes the links block itself (§5.4): a title that reaches
/// the storage can then never be missing one of them. A part the model left out
/// is filled with a placeholder in the user's language rather than failing an
/// upload of a file that is already produced.
pub fn compose_track_title(artist: &str, title: &str, modification: &str, lang: Lang) -> String {
    let title = title.trim();
    let artist = match artist.trim() {
        "" => t(Msg::TitleUnknownArtist, lang),
        artist => artist,
    };
    let modification = match modification.trim() {
        "" => t(Msg::TitleOriginal, lang),
        modification => modification,
    };

    // Models like to answer with the whole YouTube title ("Queen - Bohemian
    // Rhapsody"), and sometimes to name the modification in it as well. Neither
    // is worth saying twice.
    let mut composed = if title.is_empty() {
        artist.to_string()
    } else if starts_with_ignoring_case(title, artist) {
        title.to_string()
    } else {
        format!("{artist} — {title}")
    };
    if !contains_ignoring_case(&composed, modification) {
        composed.push_str(&format!(" ({modification})"));
    }
    composed
}

fn starts_with_ignoring_case(haystack: &str, prefix: &str) -> bool {
    haystack.to_lowercase().starts_with(&prefix.to_lowercase())
}

fn contains_ignoring_case(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _root: tempfile::TempDir,
        jobs_root: PathBuf,
        job_dir: PathBuf,
        table: HashMap<String, PathBuf>,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let jobs_root = root.path().join("jobs");
        let job_dir = jobs_root.join("11111111-1111-1111-1111-111111111111");
        std::fs::create_dir_all(job_dir.join("music/mp3")).unwrap();

        let vocals = job_dir.join("music/mp3/song_vocals.mp3");
        std::fs::write(&vocals, b"fake audio").unwrap();
        std::fs::write(job_dir.join("music/mp3/notes.txt"), b"text").unwrap();

        let mut table = HashMap::new();
        table.insert("music/mp3/song_vocals.mp3".to_string(), vocals);
        table.insert(
            "music/mp3/notes.txt".to_string(),
            job_dir.join("music/mp3/notes.txt"),
        );

        Fixture {
            _root: root,
            jobs_root,
            job_dir,
            table,
        }
    }

    #[test]
    fn a_key_from_the_path_table_resolves() {
        let f = fixture();
        let resolved =
            resolve_publish_path(&f.table, "music/mp3/song_vocals.mp3", &f.jobs_root).unwrap();
        assert!(resolved.ends_with("song_vocals.mp3"));
    }

    #[test]
    fn the_absolute_path_of_a_known_key_resolves_too() {
        let f = fixture();
        let absolute = f.table["music/mp3/song_vocals.mp3"]
            .to_string_lossy()
            .to_string();
        assert!(resolve_publish_path(&f.table, &absolute, &f.jobs_root).is_ok());
    }

    #[test]
    fn a_path_wavo_never_produced_is_refused() {
        let f = fixture();
        for requested in [
            "/etc/passwd",
            "../../etc/passwd",
            "music/mp3/../../../../etc/passwd",
            "music/mp3/song_other.mp3",
            "",
        ] {
            assert!(
                matches!(
                    resolve_publish_path(&f.table, requested, &f.jobs_root),
                    Err(PathError::NotInTable(_))
                ),
                "{requested} was not refused"
            );
        }
    }

    #[test]
    fn a_symlink_out_of_the_job_directory_is_refused() {
        let f = fixture();
        let outside = f._root.path().join("secret.mp3");
        std::fs::write(&outside, b"not yours").unwrap();
        let link = f.job_dir.join("music/mp3/link.mp3");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        #[cfg(not(unix))]
        return;

        let mut table = f.table.clone();
        table.insert("music/mp3/link.mp3".to_string(), link);

        assert!(matches!(
            resolve_publish_path(&table, "music/mp3/link.mp3", &f.jobs_root),
            Err(PathError::Escapes(_))
        ));
    }

    #[test]
    fn a_non_audio_file_is_refused_even_from_the_table() {
        let f = fixture();
        assert!(matches!(
            resolve_publish_path(&f.table, "music/mp3/notes.txt", &f.jobs_root),
            Err(PathError::NotAudio(_))
        ));
    }

    #[test]
    fn a_published_title_names_the_artist_the_song_and_what_was_done() {
        assert_eq!(
            compose_track_title("Queen", "Bohemian Rhapsody", "bez wokalu", Lang::Pl),
            "Queen — Bohemian Rhapsody (bez wokalu)"
        );
        assert_eq!(
            compose_track_title("Queen", "Bohemian Rhapsody", "vocals removed", Lang::En),
            "Queen — Bohemian Rhapsody (vocals removed)"
        );
    }

    #[test]
    fn a_missing_part_is_filled_in_in_the_users_language() {
        assert_eq!(
            compose_track_title("", "Bohemian Rhapsody", "bez wokalu", Lang::Pl),
            "Nieznany wykonawca — Bohemian Rhapsody (bez wokalu)"
        );
        assert_eq!(
            compose_track_title("Queen", "Bohemian Rhapsody", "  ", Lang::Pl),
            "Queen — Bohemian Rhapsody (oryginał)"
        );
        assert_eq!(
            compose_track_title("Queen", "Bohemian Rhapsody", "", Lang::En),
            "Queen — Bohemian Rhapsody (original)"
        );
        // Even with nothing to work with the title still has all three parts.
        assert_eq!(
            compose_track_title("", "", "", Lang::En),
            "Unknown artist (original)"
        );
    }

    #[test]
    fn a_part_the_model_repeated_is_not_said_twice() {
        assert_eq!(
            compose_track_title("Queen", "Queen - Bohemian Rhapsody", "bez wokalu", Lang::Pl),
            "Queen - Bohemian Rhapsody (bez wokalu)"
        );
        assert_eq!(
            compose_track_title(
                "Queen",
                "Bohemian Rhapsody (bez wokalu)",
                "Bez wokalu",
                Lang::Pl
            ),
            "Queen — Bohemian Rhapsody (bez wokalu)"
        );
    }

    #[test]
    fn a_file_that_has_been_swept_away_is_refused() {
        let f = fixture();
        std::fs::remove_file(&f.table["music/mp3/song_vocals.mp3"]).unwrap();
        assert!(matches!(
            resolve_publish_path(&f.table, "music/mp3/song_vocals.mp3", &f.jobs_root),
            Err(PathError::Missing(_))
        ));
    }
}
