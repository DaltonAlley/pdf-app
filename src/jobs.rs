use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use crate::{adapters::FileDownload, error::lock_mutex, AppError, AppResult};

const TERMINAL_JOB_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_TERMINAL_JOBS: usize = 100;
const MAX_TERMINAL_JOB_BYTES: usize = 512 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct JobStore {
    records: Arc<Mutex<HashMap<String, JobRecord>>>,
    max_active_jobs: Option<usize>,
}

impl Default for JobStore {
    fn default() -> Self {
        Self {
            records: Arc::new(Mutex::new(HashMap::new())),
            max_active_jobs: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct JobSnapshot {
    state: JobSnapshotState,
}

#[derive(Clone)]
enum JobSnapshotState {
    Queued,
    Running { percent: u8, stage: String },
    Done { download: Option<DownloadMetadata> },
    Failed { stage: String, error: String },
}

#[derive(Clone)]
struct DownloadMetadata {
    filename: String,
    content_type: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobStatus {
    Queued,
    Running,
    Done,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobMutationOutcome {
    Applied,
    Unchanged,
    Missing,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Error => "error",
        })
    }
}

impl JobSnapshot {
    pub(crate) fn status(&self) -> JobStatus {
        match self.state {
            JobSnapshotState::Queued => JobStatus::Queued,
            JobSnapshotState::Running { .. } => JobStatus::Running,
            JobSnapshotState::Done { .. } => JobStatus::Done,
            JobSnapshotState::Failed { .. } => JobStatus::Error,
        }
    }

    pub(crate) fn percent(&self) -> u8 {
        match &self.state {
            JobSnapshotState::Queued => 0,
            JobSnapshotState::Running { percent, .. } => *percent,
            JobSnapshotState::Done { .. } | JobSnapshotState::Failed { .. } => 100,
        }
    }

    pub(crate) fn stage(&self) -> &str {
        match &self.state {
            JobSnapshotState::Queued => "Waiting to start",
            JobSnapshotState::Running { stage, .. } | JobSnapshotState::Failed { stage, .. } => {
                stage
            }
            JobSnapshotState::Done { download: Some(_) } => "Ready to download",
            JobSnapshotState::Done { download: None } => "Download released",
        }
    }

    pub(crate) fn filename(&self) -> Option<&str> {
        match &self.state {
            JobSnapshotState::Done {
                download: Some(download),
            } => Some(&download.filename),
            _ => None,
        }
    }

    pub(crate) fn content_type(&self) -> Option<&str> {
        match &self.state {
            JobSnapshotState::Done {
                download: Some(download),
            } => Some(&download.content_type),
            _ => None,
        }
    }

    pub(crate) fn error(&self) -> Option<&str> {
        match &self.state {
            JobSnapshotState::Failed { error, .. } => Some(error),
            _ => None,
        }
    }
}

pub(crate) struct CreatedJob {
    pub(crate) id: String,
    pub(crate) cancellation: JobCancellation,
}

struct JobRecord {
    state: JobState,
    cancellation: JobCancellation,
}

enum JobState {
    Queued,
    Running {
        percent: u8,
        stage: String,
    },
    Done {
        output: JobOutput,
        terminal_at: Instant,
    },
    Failed {
        stage: String,
        error: String,
        terminal_at: Instant,
    },
}

enum JobOutput {
    Available(FileDownload),
    Released,
}

#[derive(Clone)]
pub(crate) struct JobCancellation(Arc<AtomicBool>);

impl JobCancellation {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl JobState {
    fn terminal_at(&self) -> Option<Instant> {
        match self {
            Self::Queued | Self::Running { .. } => None,
            Self::Done { terminal_at, .. } | Self::Failed { terminal_at, .. } => Some(*terminal_at),
        }
    }

    fn retained_bytes(&self) -> usize {
        match self {
            Self::Done {
                output: JobOutput::Available(file),
                ..
            } => file.bytes.len(),
            Self::Queued
            | Self::Running { .. }
            | Self::Done {
                output: JobOutput::Released,
                ..
            }
            | Self::Failed { .. } => 0,
        }
    }

    fn update(&mut self, percent: u8, stage: String) -> JobMutationOutcome {
        match self {
            Self::Running {
                percent: current, ..
            } if percent < *current => JobMutationOutcome::Applied,
            Self::Queued | Self::Running { .. } => {
                *self = Self::Running {
                    percent: percent.min(99),
                    stage,
                };
                JobMutationOutcome::Applied
            }
            Self::Done { .. } | Self::Failed { .. } => JobMutationOutcome::Unchanged,
        }
    }

    fn complete(&mut self, file: FileDownload, now: Instant) -> JobMutationOutcome {
        match self {
            Self::Queued | Self::Running { .. } => {
                *self = Self::Done {
                    output: JobOutput::Available(file),
                    terminal_at: now,
                };
                JobMutationOutcome::Applied
            }
            Self::Done { .. } | Self::Failed { .. } => JobMutationOutcome::Unchanged,
        }
    }

    fn fail(&mut self, message: String, now: Instant) -> JobMutationOutcome {
        match self {
            Self::Queued | Self::Running { .. } => {
                *self = Self::Failed {
                    stage: "Could not finish".to_string(),
                    error: message,
                    terminal_at: now,
                };
                JobMutationOutcome::Applied
            }
            Self::Done { .. } | Self::Failed { .. } => JobMutationOutcome::Unchanged,
        }
    }
}

impl JobStore {
    pub(crate) fn new(max_active_jobs: Option<usize>) -> Self {
        Self {
            max_active_jobs,
            ..Self::default()
        }
    }

    pub(crate) fn create(&self) -> AppResult<String> {
        Ok(self.create_cancellable()?.id)
    }

    pub(crate) fn create_cancellable(&self) -> AppResult<CreatedJob> {
        let now = Instant::now();
        let mut records = lock_mutex(&self.records, "job store")?;
        prune_records(&mut records, now);
        let active_jobs = records
            .values()
            .filter(|job| job.state.terminal_at().is_none())
            .count();
        if self
            .max_active_jobs
            .is_some_and(|max_active_jobs| active_jobs >= max_active_jobs)
        {
            return Err(AppError::unavailable("server is busy; try again later"));
        }
        let id = allocate_job_id(&records)?;
        let cancellation = JobCancellation(Arc::new(AtomicBool::new(false)));
        records.insert(
            id.clone(),
            JobRecord {
                state: JobState::Queued,
                cancellation: cancellation.clone(),
            },
        );
        Ok(CreatedJob { id, cancellation })
    }

    #[cfg(test)]
    fn remove(&self, id: &str) -> AppResult<()> {
        lock_mutex(&self.records, "job store")?.remove(id);
        Ok(())
    }

    #[cfg(test)]
    fn cancellation(&self, id: &str) -> AppResult<Option<JobCancellation>> {
        Ok(lock_mutex(&self.records, "job store")?
            .get(id)
            .map(|job| job.cancellation.clone()))
    }

    pub(crate) fn cancel(&self, id: &str) -> AppResult<bool> {
        let now = Instant::now();
        let mut records = lock_mutex(&self.records, "job store")?;
        let Some(job) = records.get_mut(id) else {
            return Ok(false);
        };
        if let JobState::Done { output, .. } = &mut job.state {
            *output = JobOutput::Released;
            return Ok(true);
        }
        if matches!(job.state, JobState::Failed { .. }) {
            return Ok(true);
        }
        job.cancellation.0.store(true, Ordering::Release);
        job.state = JobState::Failed {
            stage: "Cancelled".to_string(),
            error: "operation was cancelled".to_string(),
            terminal_at: now,
        };
        Ok(true)
    }

    pub(crate) fn update(
        &self,
        id: &str,
        percent: u8,
        stage: impl Into<String>,
    ) -> AppResult<JobMutationOutcome> {
        let mut records = lock_mutex(&self.records, "job store")?;
        let Some(job) = records.get_mut(id) else {
            return Ok(JobMutationOutcome::Missing);
        };
        if job.cancellation.is_cancelled() {
            return Ok(JobMutationOutcome::Unchanged);
        }
        Ok(job.state.update(percent, stage.into()))
    }

    pub(crate) fn complete(&self, id: &str, file: FileDownload) -> AppResult<JobMutationOutcome> {
        let now = Instant::now();
        let mut records = lock_mutex(&self.records, "job store")?;
        let outcome = match records.get_mut(id) {
            Some(job) if job.cancellation.is_cancelled() => JobMutationOutcome::Unchanged,
            Some(job) => job.state.complete(file, now),
            None => JobMutationOutcome::Missing,
        };
        prune_records(&mut records, now);
        Ok(outcome)
    }

    pub(crate) fn fail(
        &self,
        id: &str,
        message: impl Into<String>,
    ) -> AppResult<JobMutationOutcome> {
        let now = Instant::now();
        let mut records = lock_mutex(&self.records, "job store")?;
        let outcome = match records.get_mut(id) {
            Some(job) if job.cancellation.is_cancelled() => JobMutationOutcome::Unchanged,
            Some(job) => job.state.fail(message.into(), now),
            None => JobMutationOutcome::Missing,
        };
        prune_records(&mut records, now);
        Ok(outcome)
    }

    pub(crate) fn snapshot(&self, id: &str) -> AppResult<Option<JobSnapshot>> {
        let mut records = lock_mutex(&self.records, "job store")?;
        prune_records(&mut records, Instant::now());
        let Some(job) = records.get(id) else {
            return Ok(None);
        };
        let state = match &job.state {
            JobState::Queued => JobSnapshotState::Queued,
            JobState::Running { percent, stage } => JobSnapshotState::Running {
                percent: *percent,
                stage: stage.clone(),
            },
            JobState::Done { output, .. } => {
                let download = match output {
                    JobOutput::Available(file) => Some(DownloadMetadata {
                        filename: file.filename.clone(),
                        content_type: file.content_type.clone(),
                    }),
                    JobOutput::Released => None,
                };
                JobSnapshotState::Done { download }
            }
            JobState::Failed { stage, error, .. } => JobSnapshotState::Failed {
                stage: stage.clone(),
                error: error.clone(),
            },
        };
        Ok(Some(JobSnapshot { state }))
    }

    pub(crate) fn download(&self, id: &str) -> AppResult<Option<FileDownload>> {
        let mut records = lock_mutex(&self.records, "job store")?;
        prune_records(&mut records, Instant::now());
        let Some(job) = records.get(id) else {
            return Ok(None);
        };
        match &job.state {
            JobState::Done {
                output: JobOutput::Available(file),
                ..
            } => Ok(Some(FileDownload::new(
                file.content_type.clone(),
                file.filename.clone(),
                file.bytes.clone(),
            ))),
            JobState::Queued
            | JobState::Running { .. }
            | JobState::Done {
                output: JobOutput::Released,
                ..
            }
            | JobState::Failed { .. } => Ok(None),
        }
    }
}

fn random_job_id() -> AppResult<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|err| AppError::internal_cause("could not generate job id", err))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn allocate_job_id(records: &HashMap<String, JobRecord>) -> AppResult<String> {
    const MAX_ID_ATTEMPTS: usize = 4;

    for _ in 0..MAX_ID_ATTEMPTS {
        let id = random_job_id()?;
        if !records.contains_key(&id) {
            return Ok(id);
        }
    }
    Err(AppError::Internal(
        "could not allocate a unique job id".to_string(),
    ))
}

fn prune_records(records: &mut HashMap<String, JobRecord>, now: Instant) {
    records.retain(|_, job| {
        job.state
            .terminal_at()
            .map(|terminal_at| now.duration_since(terminal_at) < TERMINAL_JOB_TTL)
            .unwrap_or(true)
    });

    let terminal_count = records
        .values()
        .filter(|job| job.state.terminal_at().is_some())
        .count();
    let mut terminal_jobs = records
        .iter()
        .filter_map(|(id, job)| {
            job.state
                .terminal_at()
                .map(|terminal_at| (id.clone(), terminal_at))
        })
        .collect::<Vec<_>>();
    terminal_jobs.sort_by_key(|(_, terminal_at)| *terminal_at);

    let mut retained_bytes = records
        .values()
        .map(|job| job.state.retained_bytes())
        .sum::<usize>();
    let overflow = terminal_count.saturating_sub(MAX_TERMINAL_JOBS);
    let removable_jobs = terminal_jobs.len().saturating_sub(1);
    for (index, (id, _)) in terminal_jobs.into_iter().enumerate() {
        if index >= overflow && retained_bytes <= MAX_TERMINAL_JOB_BYTES {
            break;
        }
        if index >= removable_jobs {
            break;
        }
        retained_bytes = retained_bytes.saturating_sub(
            records
                .get(&id)
                .map(|job| job.state.retained_bytes())
                .unwrap_or(0),
        );
        records.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download(bytes: &[u8]) -> FileDownload {
        FileDownload::new("application/pdf", "result.pdf", bytes.to_vec())
    }

    fn snapshot(store: &JobStore, id: &str) -> JobSnapshot {
        store.snapshot(id).unwrap().unwrap()
    }

    fn stored_download(store: &JobStore, id: &str) -> FileDownload {
        store.download(id).unwrap().unwrap()
    }

    #[test]
    fn snapshots_preserve_serialized_status_values() {
        let store = JobStore::default();

        let queued_id = store.create().unwrap();
        assert_eq!(snapshot(&store, &queued_id).status(), JobStatus::Queued);

        store.update(&queued_id, 50, "Working").unwrap();
        assert_eq!(snapshot(&store, &queued_id).status(), JobStatus::Running);

        store.complete(&queued_id, download(b"pdf")).unwrap();
        assert_eq!(snapshot(&store, &queued_id).status(), JobStatus::Done);

        let failed_id = store.create().unwrap();
        store.fail(&failed_id, "failed").unwrap();
        assert_eq!(snapshot(&store, &failed_id).status(), JobStatus::Error);
    }

    #[test]
    fn update_clamps_running_percent_to_99() {
        let store = JobStore::default();
        let id = store.create().unwrap();

        store.update(&id, 250, "Almost done").unwrap();

        let snapshot = snapshot(&store, &id);
        assert_eq!(snapshot.status(), JobStatus::Running);
        assert_eq!(snapshot.percent(), 99);
        assert_eq!(snapshot.stage(), "Almost done");
    }

    #[test]
    fn download_can_be_retried_until_the_job_expires() {
        let store = JobStore::default();
        let id = store.create().unwrap();
        store.complete(&id, download(b"pdf")).unwrap();

        assert_eq!(stored_download(&store, &id).bytes.as_ref(), b"pdf");
        assert_eq!(snapshot(&store, &id).status(), JobStatus::Done);
        assert_eq!(stored_download(&store, &id).bytes.as_ref(), b"pdf");
    }

    #[test]
    fn cancelling_a_completed_job_releases_its_download() {
        let store = JobStore::default();
        let id = store.create().unwrap();
        store.complete(&id, download(b"pdf")).unwrap();

        assert!(store.cancel(&id).unwrap());
        assert!(store.download(&id).unwrap().is_none());
        let snapshot = snapshot(&store, &id);
        assert_eq!(snapshot.status(), JobStatus::Done);
        assert_eq!(snapshot.stage(), "Download released");
        assert!(snapshot.filename().is_none());
    }

    #[test]
    fn download_returns_none_for_missing_pending_and_failed_jobs() {
        let store = JobStore::default();

        assert!(store.download("missing").unwrap().is_none());

        let pending_id = store.create().unwrap();
        assert!(store.download(&pending_id).unwrap().is_none());
        assert_eq!(snapshot(&store, &pending_id).status(), JobStatus::Queued);

        let failed_id = store.create().unwrap();
        store.fail(&failed_id, "failed").unwrap();
        assert!(store.download(&failed_id).unwrap().is_none());
        assert_eq!(snapshot(&store, &failed_id).status(), JobStatus::Error);
    }

    #[test]
    fn missing_job_mutations_are_explicit_and_harmless() {
        let store = JobStore::default();

        assert_eq!(
            store.update("missing", 10, "late").unwrap(),
            JobMutationOutcome::Missing
        );
        assert_eq!(
            store.complete("missing", download(b"pdf")).unwrap(),
            JobMutationOutcome::Missing
        );
        assert_eq!(
            store.fail("missing", "failed").unwrap(),
            JobMutationOutcome::Missing
        );

        assert!(store.snapshot("missing").unwrap().is_none());
    }

    #[test]
    fn late_concurrent_progress_cannot_replace_newer_progress() {
        let store = Arc::new(JobStore::default());
        let id = store.create().unwrap();
        let (newer_stored, wait_for_newer) = std::sync::mpsc::channel();

        let newer_store = store.clone();
        let newer_id = id.clone();
        let newer = std::thread::spawn(move || {
            let outcome = newer_store
                .update(&newer_id, 70, "Converted image 70 of 100: `newer.png`")
                .unwrap();
            newer_stored.send(()).unwrap();
            outcome
        });
        let stale_store = store.clone();
        let stale_id = id.clone();
        let stale = std::thread::spawn(move || {
            wait_for_newer.recv().unwrap();
            stale_store
                .update(&stale_id, 40, "Converted image 40 of 100: `stale.png`")
                .unwrap()
        });

        assert_eq!(newer.join().unwrap(), JobMutationOutcome::Applied);
        assert_eq!(stale.join().unwrap(), JobMutationOutcome::Applied);
        let snapshot = snapshot(&store, &id);
        assert_eq!(snapshot.percent(), 70);
        assert_eq!(snapshot.stage(), "Converted image 70 of 100: `newer.png`");
    }

    #[test]
    fn terminal_job_count_is_bounded() {
        let store = JobStore::default();
        let first_id = store.create().unwrap();
        store.complete(&first_id, download(b"first")).unwrap();

        for _ in 0..MAX_TERMINAL_JOBS {
            let id = store.create().unwrap();
            store.complete(&id, download(b"next")).unwrap();
        }

        assert!(store.snapshot(&first_id).unwrap().is_none());
    }

    #[test]
    fn active_job_capacity_recovers_after_a_job_finishes_or_is_removed() {
        let store = JobStore::new(Some(1));
        let first = store.create().unwrap();
        assert_eq!(
            store.create().unwrap_err().to_string(),
            "server is busy; try again later"
        );

        store.fail(&first, "failed").unwrap();
        let second = store.create().unwrap();
        store.remove(&second).unwrap();
        assert!(store.create().is_ok());
    }

    #[test]
    fn active_jobs_are_unlimited_when_no_capacity_is_configured() {
        let store = JobStore::default();

        for _ in 0..32 {
            assert!(store.create().is_ok());
        }
    }

    #[test]
    fn job_ids_are_unguessable_lowercase_hex_capabilities() {
        let store = JobStore::default();
        let first = store.create().unwrap();
        let second = store.create().unwrap();

        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(first, first.to_ascii_lowercase());
        assert_ne!(first, second);
    }

    #[test]
    fn cancelling_a_job_marks_its_token_and_releases_capacity() {
        let store = JobStore::new(Some(1));
        let id = store.create().unwrap();
        let cancellation = store.cancellation(&id).unwrap().unwrap();

        assert!(store.cancel(&id).unwrap());
        assert!(cancellation.is_cancelled());
        assert_eq!(snapshot(&store, &id).status(), JobStatus::Error);
        assert_eq!(
            snapshot(&store, &id).error(),
            Some("operation was cancelled")
        );
        assert!(store.create().is_ok());
    }

    #[test]
    fn queued_and_running_jobs_can_reach_each_terminal_state() {
        let store = JobStore::default();

        let queued_done = store.create().unwrap();
        assert_eq!(
            store.complete(&queued_done, download(b"queued")).unwrap(),
            JobMutationOutcome::Applied
        );

        let running_done = store.create().unwrap();
        assert_eq!(
            store.update(&running_done, 20, "working").unwrap(),
            JobMutationOutcome::Applied
        );
        assert_eq!(
            store.complete(&running_done, download(b"running")).unwrap(),
            JobMutationOutcome::Applied
        );

        let queued_failed = store.create().unwrap();
        assert_eq!(
            store.fail(&queued_failed, "queued failure").unwrap(),
            JobMutationOutcome::Applied
        );

        let running_failed = store.create().unwrap();
        store.update(&running_failed, 20, "working").unwrap();
        assert_eq!(
            store.fail(&running_failed, "running failure").unwrap(),
            JobMutationOutcome::Applied
        );
    }

    #[test]
    fn late_and_repeated_terminal_events_do_not_change_completed_jobs() {
        let store = JobStore::default();
        let id = store.create().unwrap();
        store.complete(&id, download(b"original")).unwrap();

        assert_eq!(
            store.update(&id, 50, "late progress").unwrap(),
            JobMutationOutcome::Unchanged
        );
        assert_eq!(
            store.fail(&id, "late failure").unwrap(),
            JobMutationOutcome::Unchanged
        );
        assert_eq!(
            store.complete(&id, download(b"replacement")).unwrap(),
            JobMutationOutcome::Unchanged
        );
        assert_eq!(stored_download(&store, &id).bytes.as_ref(), b"original");
    }

    #[test]
    fn late_and_repeated_terminal_events_do_not_change_failed_jobs() {
        let store = JobStore::default();
        let id = store.create().unwrap();
        store.fail(&id, "original failure").unwrap();

        assert_eq!(
            store.update(&id, 50, "late progress").unwrap(),
            JobMutationOutcome::Unchanged
        );
        assert_eq!(
            store.complete(&id, download(b"late output")).unwrap(),
            JobMutationOutcome::Unchanged
        );
        assert_eq!(
            store.fail(&id, "replacement failure").unwrap(),
            JobMutationOutcome::Unchanged
        );
        assert_eq!(snapshot(&store, &id).error(), Some("original failure"));
    }

    #[test]
    fn racing_terminal_events_apply_exactly_one_consistent_result() {
        use std::{sync::Barrier, thread};

        let store = JobStore::default();
        let id = store.create().unwrap();
        let barrier = Arc::new(Barrier::new(3));

        let complete_store = store.clone();
        let complete_id = id.clone();
        let complete_barrier = barrier.clone();
        let complete = thread::spawn(move || {
            complete_barrier.wait();
            complete_store
                .complete(&complete_id, download(b"winner"))
                .unwrap()
        });

        let fail_store = store.clone();
        let fail_id = id.clone();
        let fail_barrier = barrier.clone();
        let fail = thread::spawn(move || {
            fail_barrier.wait();
            fail_store.fail(&fail_id, "racing failure").unwrap()
        });

        barrier.wait();
        let outcomes = [complete.join().unwrap(), fail.join().unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == JobMutationOutcome::Applied)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == JobMutationOutcome::Unchanged)
                .count(),
            1
        );

        let snapshot = snapshot(&store, &id);
        match snapshot.status() {
            JobStatus::Done => {
                assert_eq!(snapshot.filename(), Some("result.pdf"));
                assert!(snapshot.error().is_none());
                assert_eq!(stored_download(&store, &id).bytes.as_ref(), b"winner");
            }
            JobStatus::Error => {
                assert_eq!(snapshot.error(), Some("racing failure"));
                assert!(snapshot.filename().is_none());
                assert!(store.download(&id).unwrap().is_none());
            }
            JobStatus::Queued | JobStatus::Running => panic!("job must be terminal"),
        }
    }
}
