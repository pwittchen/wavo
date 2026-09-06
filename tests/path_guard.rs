//! The path guard, against the path table a real `process_audio` result produces.
//!
//! This is the boundary the model cannot cross: it names a key, wavo decides
//! what file that is, and anything that resolves outside the job directory is
//! refused before the file is opened (§12, acceptance criterion 12).

use std::collections::HashMap;
use std::path::PathBuf;

use wavo::error::PathError;
use wavo::tools::publish::resolve_publish_path;

struct Fixture {
    root: tempfile::TempDir,
    jobs_root: PathBuf,
    job_dir: PathBuf,
    table: HashMap<String, PathBuf>,
}

/// A job directory shaped like demix's output, with the path table wavo builds
/// from the `files` map it returns.
fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let jobs_root = root.path().join("jobs");
    let job_dir = jobs_root.join("6f3c2f4e-0000-4000-8000-000000000000");
    std::fs::create_dir_all(job_dir.join("music/mp3")).unwrap();
    std::fs::create_dir_all(job_dir.join("music/wav")).unwrap();
    std::fs::create_dir_all(job_dir.join("video")).unwrap();

    let mut table = HashMap::new();
    for relative in [
        "music/mp3/bohemian_rhapsody_vocals.mp3",
        "music/mp3/bohemian_rhapsody_accompaniment.mp3",
        "music/wav/bohemian_rhapsody_vocals.wav",
    ] {
        let path = job_dir.join(relative);
        std::fs::write(&path, b"fake audio").unwrap();
        table.insert(relative.to_string(), path);
    }

    // Secrets that exist next to the job directory, to be refused.
    std::fs::write(root.path().join("passwd"), b"root:x:0:0").unwrap();

    Fixture {
        root,
        jobs_root,
        job_dir,
        table,
    }
}

#[test]
fn the_stems_the_run_produced_can_be_published() {
    let f = fixture();
    for key in [
        "music/mp3/bohemian_rhapsody_vocals.mp3",
        "music/wav/bohemian_rhapsody_vocals.wav",
    ] {
        let resolved = resolve_publish_path(&f.table, key, &f.jobs_root).unwrap();
        assert!(resolved.starts_with(f.jobs_root.canonicalize().unwrap()));
        assert!(resolved.is_file());
    }
}

#[test]
fn a_request_to_publish_a_system_file_is_refused() {
    let f = fixture();
    for requested in [
        "/etc/passwd",
        "/etc/shadow",
        "../../../../etc/passwd",
        "music/mp3/../../../passwd",
        "music/mp3/../../../../../../etc/passwd",
        "~/.ssh/id_rsa",
        "music/mp3/bohemian_rhapsody_vocals.mp3 ; cat /etc/passwd",
    ] {
        let refused = resolve_publish_path(&f.table, requested, &f.jobs_root);
        assert!(
            matches!(refused, Err(PathError::NotInTable(_))),
            "{requested} was not refused: {refused:?}"
        );
    }
}

#[test]
fn a_neighbouring_job_directory_is_out_of_reach() {
    let f = fixture();
    // Another turn's output really exists, but it is not in this turn's table.
    let other = f
        .jobs_root
        .join("aaaaaaaa-0000-4000-8000-000000000000/music/mp3");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("private.mp3"), b"audio").unwrap();

    let requested = other.join("private.mp3").to_string_lossy().to_string();
    assert!(matches!(
        resolve_publish_path(&f.table, &requested, &f.jobs_root),
        Err(PathError::NotInTable(_))
    ));
}

#[cfg(unix)]
#[test]
fn a_symlink_planted_inside_the_job_directory_does_not_help() {
    let f = fixture();
    // Even if something inside the job dir points out of it — a crafted archive,
    // a hostile filename — the containment check is made after canonicalization.
    let link = f.job_dir.join("music/mp3/escape.mp3");
    std::os::unix::fs::symlink(f.root.path().join("passwd"), &link).unwrap();

    let mut table = f.table.clone();
    table.insert("music/mp3/escape.mp3".to_string(), link);

    assert!(matches!(
        resolve_publish_path(&table, "music/mp3/escape.mp3", &f.jobs_root),
        Err(PathError::Escapes(_))
    ));
}

#[test]
fn video_output_cannot_be_published_even_when_it_is_in_the_table() {
    let f = fixture();
    let video = f.job_dir.join("video/bohemian_rhapsody.mkv");
    std::fs::write(&video, b"fake video").unwrap();

    let mut table = f.table.clone();
    table.insert("video/bohemian_rhapsody.mkv".to_string(), video);

    assert!(matches!(
        resolve_publish_path(&table, "video/bohemian_rhapsody.mkv", &f.jobs_root),
        Err(PathError::NotAudio(_))
    ));
}
