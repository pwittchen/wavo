//! Job directories, the concurrency permit that keeps spleeter from eating the
//! machine, the progress message, and the cleanup that follows a turn (§3.4, §10.2).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::telegram::api::Telegram;
use crate::telegram::format::{human_duration, t_n, t_t, Lang, Msg};

/// Orphans left behind by a crash are swept at startup (§10.2).
pub const ORPHAN_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

pub struct JobManager {
    semaphore: Arc<Semaphore>,
    jobs_dir: PathBuf,
    keep_files: bool,
    /// Turns currently holding or waiting for a permit, for `/status` and for the
    /// queue position a waiting user is told.
    pending: AtomicUsize,
}

impl JobManager {
    pub fn new(jobs_dir: PathBuf, permits: usize, keep_files: bool) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(permits)),
            jobs_dir,
            keep_files,
            pending: AtomicUsize::new(0),
        }
    }

    pub fn jobs_dir(&self) -> &Path {
        &self.jobs_dir
    }

    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    /// A fresh `/work/jobs/<uuid>`. wavo, not the model, decides where output goes.
    pub fn new_job_dir(&self) -> std::io::Result<PathBuf> {
        let dir = self.jobs_dir.join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Wait for a slot. When the wait is real, the caller's progress message is
    /// told its position in the queue rather than sitting silent (§3.4).
    pub async fn acquire(&self, progress: Option<&Progress>) -> JobPermit<'_> {
        let position = self.pending.fetch_add(1, Ordering::Relaxed) + 1;
        let semaphore = self.semaphore.clone();

        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                if let Some(progress) = progress {
                    progress
                        .say(t_n(Msg::QueuePosition, progress.lang, position))
                        .await;
                }
                semaphore
                    .acquire_owned()
                    .await
                    .expect("the job semaphore is never closed")
            }
        };

        JobPermit {
            _permit: permit,
            pending: &self.pending,
        }
    }

    /// Remove the job directories a turn produced, unless the operator asked to
    /// keep them.
    pub async fn cleanup(&self, dirs: &[PathBuf]) {
        if self.keep_files {
            return;
        }
        for dir in dirs {
            if !dir.starts_with(&self.jobs_dir) {
                continue;
            }
            if let Err(e) = tokio::fs::remove_dir_all(dir).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(dir = %dir.display(), error = %e, "could not remove job directory");
                }
            }
        }
    }

    /// Drop job directories older than `max_age` — what a crash or a `SIGKILL`
    /// leaves behind.
    pub fn sweep_orphans(&self, max_age: Duration) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.jobs_dir) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let age = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok());
            if age.is_some_and(|age| age > max_age) && std::fs::remove_dir_all(entry.path()).is_ok()
            {
                removed += 1;
            }
        }
        removed
    }
}

/// Held for the duration of a demix run; releases the slot and the pending count
/// when dropped.
pub struct JobPermit<'a> {
    _permit: OwnedSemaphorePermit,
    pending: &'a AtomicUsize,
}

impl Drop for JobPermit<'_> {
    fn drop(&mut self) {
        self.pending.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The acknowledgement message wavo edits in place while a turn runs (§5.4).
///
/// Edits are rate limited to one every `WAVO_PROGRESS_INTERVAL_SEC`: Telegram
/// answers a burst of edits with a 429 and then ignores the bot for a while.
pub struct Progress {
    telegram: Telegram,
    chat_id: i64,
    message_id: i64,
    pub lang: Lang,
    interval: Duration,
    started: Instant,
    state: Mutex<ProgressState>,
}

#[derive(Debug)]
struct ProgressState {
    stage: Option<Msg>,
    last_edit: Instant,
    last_text: String,
}

impl Progress {
    pub fn new(
        telegram: Telegram,
        chat_id: i64,
        message_id: i64,
        lang: Lang,
        interval: Duration,
    ) -> Self {
        Self {
            telegram,
            chat_id,
            message_id,
            lang,
            interval,
            started: Instant::now(),
            state: Mutex::new(ProgressState {
                stage: None,
                // Backdated so the first stage change is shown immediately.
                last_edit: Instant::now() - interval,
                last_text: String::new(),
            }),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The acknowledgement message this progress reporter edits — the same one
    /// the final answer replaces.
    pub fn message_id(&self) -> i64 {
        self.message_id
    }

    /// Move to a new stage. The stage is what wavo *asked demix to do*: demix
    /// reports nothing until it exits, so the honest signal is the requested work
    /// plus how long it has been running.
    pub async fn stage(&self, stage: Msg) {
        {
            let mut state = self.state.lock().await;
            state.stage = Some(stage);
        }
        self.refresh(true).await;
    }

    /// Re-render the current stage with an updated elapsed time.
    pub async fn tick(&self) {
        self.refresh(false).await;
    }

    /// A one-off line that replaces the progress text (queue position).
    pub async fn say(&self, text: String) {
        let mut state = self.state.lock().await;
        state.stage = None;
        self.edit(&mut state, text, true).await;
    }

    async fn refresh(&self, force: bool) {
        let mut state = self.state.lock().await;
        let Some(stage) = state.stage else {
            return;
        };
        let text = t_t(
            stage,
            self.lang,
            human_duration(self.started.elapsed().as_secs()),
        );
        self.edit(&mut state, text, force).await;
    }

    async fn edit(&self, state: &mut ProgressState, text: String, force: bool) {
        if text == state.last_text {
            return;
        }
        if !force && state.last_edit.elapsed() < self.interval {
            return;
        }
        state.last_edit = Instant::now();
        state.last_text = text.clone();

        if let Err(e) = self
            .telegram
            .edit_message_text(self.chat_id, self.message_id, &text)
            .await
        {
            // A failed progress edit must never fail the turn.
            tracing::debug!(chat_id = self.chat_id, error = %e, "progress edit failed");
        }
    }

    /// Keep the elapsed time moving while a long tool call runs. The handle is
    /// aborted by the caller when the call returns.
    pub fn spawn_ticker(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let progress = self.clone();
        let interval = self.interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                progress.tick().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_directories_are_created_under_the_jobs_root() {
        let root = tempfile::tempdir().unwrap();
        let manager = JobManager::new(root.path().join("jobs"), 1, false);
        std::fs::create_dir_all(manager.jobs_dir()).unwrap();

        let first = manager.new_job_dir().unwrap();
        let second = manager.new_job_dir().unwrap();
        assert!(first.is_dir());
        assert_ne!(first, second);
        assert!(first.starts_with(manager.jobs_dir()));
    }

    #[tokio::test]
    async fn cleanup_removes_job_directories_but_refuses_paths_outside_the_root() {
        let root = tempfile::tempdir().unwrap();
        let manager = JobManager::new(root.path().join("jobs"), 1, false);
        std::fs::create_dir_all(manager.jobs_dir()).unwrap();

        let job = manager.new_job_dir().unwrap();
        let outside = root.path().join("keep-me");
        std::fs::create_dir_all(&outside).unwrap();

        manager.cleanup(&[job.clone(), outside.clone()]).await;
        assert!(!job.exists());
        assert!(outside.exists(), "cleanup escaped the jobs root");
    }

    #[tokio::test]
    async fn cleanup_keeps_everything_when_asked_to() {
        let root = tempfile::tempdir().unwrap();
        let manager = JobManager::new(root.path().join("jobs"), 1, true);
        std::fs::create_dir_all(manager.jobs_dir()).unwrap();

        let job = manager.new_job_dir().unwrap();
        manager.cleanup(std::slice::from_ref(&job)).await;
        assert!(job.exists());
    }

    #[test]
    fn the_sweep_only_takes_old_directories() {
        let root = tempfile::tempdir().unwrap();
        let manager = JobManager::new(root.path().join("jobs"), 1, false);
        std::fs::create_dir_all(manager.jobs_dir()).unwrap();

        let fresh = manager.new_job_dir().unwrap();
        assert_eq!(manager.sweep_orphans(ORPHAN_MAX_AGE), 0);
        assert!(fresh.exists());
        // Everything is "old" with a zero threshold.
        assert_eq!(manager.sweep_orphans(Duration::ZERO), 1);
        assert!(!fresh.exists());
    }

    #[tokio::test]
    async fn one_permit_serializes_jobs_and_tracks_how_many_are_pending() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(JobManager::new(root.path().join("jobs"), 1, false));
        assert_eq!(manager.pending(), 0);

        let first = manager.acquire(None).await;
        assert_eq!(manager.pending(), 1);

        let waiting = {
            let manager = manager.clone();
            tokio::spawn(async move {
                let _permit = manager.acquire(None).await;
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(manager.pending(), 2, "the waiting job should be counted");

        drop(first);
        waiting.await.unwrap();
        assert_eq!(manager.pending(), 0);
    }
}
