use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use super::{
    layout::validate_request,
    model::{RecentGangUpJob, RecentGangUpJobInput},
    persistence::{read_json_or_default, valid_identifier, write_json_atomic},
};
use crate::error::{lock_mutex, AppError, AppResult};

const RECENT_JOBS_FILE: &str = "gang-up-recent-jobs.json";
const MAX_RECENT_JOBS: usize = 12;

#[derive(Clone)]
pub(crate) struct RecentGangUpStore {
    path: Arc<PathBuf>,
    write_lock: Arc<Mutex<()>>,
}

impl RecentGangUpStore {
    pub(crate) fn new(data_dir: PathBuf) -> Self {
        Self {
            path: Arc::new(data_dir.join(RECENT_JOBS_FILE)),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn list(&self) -> AppResult<Vec<RecentGangUpJob>> {
        let _guard = lock_mutex(&self.write_lock, "recent job store")?;
        self.read_saved_unlocked()
    }

    pub(crate) fn create(&self, mut input: RecentGangUpJobInput) -> AppResult<RecentGangUpJob> {
        input.request.source_id = None;
        validate_request(&input.request)?;
        let _guard = lock_mutex(&self.write_lock, "recent job store")?;
        let mut jobs = self.read_saved_unlocked()?;
        let now = unix_timestamp();
        let id = format!("job-{now}");
        let name = input
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| fallback_name(input.source_filename.as_deref()));
        let job = RecentGangUpJob {
            id: unique_id(&id, &jobs)?,
            name,
            source_filename: input.source_filename.filter(|name| !name.trim().is_empty()),
            saved_at: now.to_string(),
            request: input.request,
            layout_summary: input.layout_summary,
        };

        jobs.retain(|existing| {
            existing.source_filename != job.source_filename || existing.request != job.request
        });
        jobs.insert(0, job.clone());
        jobs.truncate(MAX_RECENT_JOBS);
        self.write_saved_unlocked(&jobs)?;
        Ok(job)
    }

    pub(crate) fn delete(&self, id: &str) -> AppResult<()> {
        if !valid_identifier(id) {
            return Err(AppError::bad_request("invalid recent job id"));
        }
        let _guard = lock_mutex(&self.write_lock, "recent job store")?;
        let mut jobs = self.read_saved_unlocked()?;
        let before = jobs.len();
        jobs.retain(|job| job.id != id);
        if jobs.len() == before {
            return Err(AppError::bad_request("recent job not found"));
        }
        self.write_saved_unlocked(&jobs)
    }

    fn read_saved_unlocked(&self) -> AppResult<Vec<RecentGangUpJob>> {
        let jobs: Vec<RecentGangUpJob> =
            read_json_or_default(self.path.as_path(), "recent gang-up jobs")?;
        let mut ids = HashSet::with_capacity(jobs.len());
        if jobs.len() > MAX_RECENT_JOBS
            || jobs.iter().any(|job| {
                !valid_identifier(&job.id)
                    || !ids.insert(job.id.as_str())
                    || validate_request(&job.request).is_err()
            })
        {
            return Err(AppError::Internal(
                "recent gang-up job metadata is invalid".to_string(),
            ));
        }
        Ok(jobs)
    }

    fn write_saved_unlocked(&self, jobs: &[RecentGangUpJob]) -> AppResult<()> {
        write_json_atomic(self.path.as_path(), jobs, "recent gang-up jobs")
    }
}

fn fallback_name(filename: Option<&str>) -> String {
    filename
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| format!("Repeat {name}"))
        .unwrap_or_else(|| "Recent gang-up job".to_string())
}

fn unique_id(base: &str, jobs: &[RecentGangUpJob]) -> AppResult<String> {
    if jobs.iter().all(|job| job.id != base) {
        return Ok(base.to_string());
    }

    for suffix in 2..=MAX_RECENT_JOBS + 1 {
        let candidate = format!("{base}-{suffix}");
        if jobs.iter().all(|job| job.id != candidate) {
            return Ok(candidate);
        }
    }
    Err(AppError::Internal(
        "could not allocate a unique recent job id".to_string(),
    ))
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn store() -> RecentGangUpStore {
        static NEXT_DIR: AtomicU64 = AtomicU64::new(1);
        RecentGangUpStore::new(std::env::temp_dir().join(format!(
            "pdf-tools-recent-store-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn input(quantity: usize) -> RecentGangUpJobInput {
        serde_json::from_value(serde_json::json!({
            "name": null,
            "sourceFilename": "cards.pdf",
            "request": {
                "sourcePdfSize": { "width": 3.5, "height": 2.0 },
                "sourcePageCount": 1,
                "finishedCutSize": { "width": 3.5, "height": 2.0 },
                "parentSheetSize": { "width": 12.0, "height": 18.0 },
                "quantityRequested": quantity,
                "impositionMode": "repeat",
                "sides": "single",
                "duplex": null,
                "layoutMode": "auto",
                "bleedOption": "useAsIs",
                "gutter": { "horizontal": 0.0, "vertical": 0.0 },
                "manual": null
            },
            "layoutSummary": null
        }))
        .unwrap()
    }

    fn cleanup(store: &RecentGangUpStore) {
        if let Some(parent) = store.path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn create_deduplicates_equivalent_recent_jobs() {
        let store = store();
        store.create(input(10)).unwrap();
        let replacement = store.create(input(10)).unwrap();

        let jobs = store.list().unwrap();
        assert_eq!(jobs, vec![replacement]);
        cleanup(&store);
    }

    #[test]
    fn stretch_recent_jobs_reload_without_changing_legacy_or_fit_settings() {
        use super::super::model::{ArtworkFit, ArtworkFitMode, CropPosition, PageOverride};
        let store = store();
        let legacy = store.create(input(10)).unwrap();
        let mut stretch = input(10);
        stretch.request.artwork_fit = Some(ArtworkFit {
            mode: ArtworkFitMode::Stretch,
            position: CropPosition::default(),
        });
        stretch.request.page_overrides.push(PageOverride {
            page_number: 1,
            finished_cut_size: None,
            artwork_fit: stretch.request.artwork_fit,
        });
        let saved = store.create(stretch.clone()).unwrap();
        stretch.request.artwork_fit.as_mut().unwrap().mode = ArtworkFitMode::Contain;
        stretch.request.page_overrides.clear();
        let fit = store.create(stretch).unwrap();
        let reloaded = RecentGangUpStore::new(store.path.parent().unwrap().to_path_buf());
        assert_eq!(reloaded.list().unwrap(), vec![fit, saved, legacy]);
        cleanup(&store);
    }

    #[test]
    fn timestamp_id_collisions_receive_deterministic_suffixes() {
        let base = "job-100";
        let store = store();
        let mut existing = store.create(input(1)).unwrap();
        existing.id = base.to_string();

        assert_eq!(unique_id(base, &[existing]).unwrap(), "job-100-2");
        cleanup(&store);
    }

    #[test]
    fn recent_jobs_are_truncated_to_capacity() {
        let store = store();
        for quantity in 1..=MAX_RECENT_JOBS + 2 {
            store.create(input(quantity)).unwrap();
        }

        let jobs = store.list().unwrap();
        assert_eq!(jobs.len(), MAX_RECENT_JOBS);
        assert_eq!(
            jobs.first().map(|job| job.request.quantity_requested),
            Some(MAX_RECENT_JOBS + 2)
        );
        cleanup(&store);
    }

    #[test]
    fn delete_reports_missing_records() {
        let store = store();
        assert!(store.delete("job-missing").is_err());
        cleanup(&store);
    }

    #[test]
    fn corrupt_or_duplicate_persisted_metadata_is_rejected() {
        let corrupt_store = store();
        let mut job = corrupt_store.create(input(1)).unwrap();
        job.request.quantity_requested = 0;
        write_json_atomic(corrupt_store.path.as_path(), &[job], "test recent jobs").unwrap();
        assert!(corrupt_store.list().is_err());
        cleanup(&corrupt_store);

        let duplicate_store = store();
        let job = duplicate_store.create(input(2)).unwrap();
        write_json_atomic(
            duplicate_store.path.as_path(),
            &[job.clone(), job],
            "test recent jobs",
        )
        .unwrap();
        assert!(duplicate_store.list().is_err());
        cleanup(&duplicate_store);
    }
}
