//! The path guard. `publish_track` is the one tool that turns a string from the
//! model into an open file, so this is where the model's reach ends (§12).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::PathError;
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
    fn a_file_that_has_been_swept_away_is_refused() {
        let f = fixture();
        std::fs::remove_file(&f.table["music/mp3/song_vocals.mp3"]).unwrap();
        assert!(matches!(
            resolve_publish_path(&f.table, "music/mp3/song_vocals.mp3", &f.jobs_root),
            Err(PathError::Missing(_))
        ));
    }
}
