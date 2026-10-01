use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use super::persistence::valid_identifier;
use crate::{
    adapters::UploadFile,
    error::{lock_mutex, AppError, AppResult},
};

const SOURCE_SESSION_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_SOURCE_SESSIONS: usize = 32;

#[derive(Clone)]
pub(crate) struct SourceSessionStore {
    root: Arc<PathBuf>,
    records: Arc<Mutex<HashMap<String, SourceSessionRecord>>>,
}

pub(crate) struct SourceSessionFile {
    pub(crate) filename: String,
    pub(crate) path: PathBuf,
    _lease: SourceSessionLease,
}

struct SourceSessionRecord {
    filename: String,
    path: PathBuf,
    last_used: Instant,
    state: SourceSessionState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceSessionState {
    Creating,
    Available { active_leases: usize },
    RemovalPending { active_leases: NonZeroUsize },
    Removing,
}

struct SourceSessionLease {
    id: String,
    path: PathBuf,
    records: Arc<Mutex<HashMap<String, SourceSessionRecord>>>,
}

impl Drop for SourceSessionLease {
    fn drop(&mut self) {
        let path = match lock_mutex(&self.records, "source session store") {
            Ok(mut records) => release_lease(&mut records, &self.id, &self.path),
            Err(error) => {
                tracing::error!(source_id = self.id, %error, "failed to release source session lease");
                None
            }
        };
        if let Some(path) = path {
            match remove_file_if_present(&path) {
                Ok(()) => remove_cleaned_record(&self.records, &self.id, &path),
                Err(error) => {
                    tracing::warn!(source_id = self.id, %error, "deferred source session could not be removed");
                }
            }
        }
    }
}

impl SourceSessionStore {
    pub(crate) fn new(data_dir: PathBuf) -> Self {
        let root = data_dir.join("source-sessions");
        if let Err(error) = remove_orphaned_sessions(&root) {
            tracing::warn!(%error, "orphaned source sessions could not be cleaned");
        }
        Self {
            root: Arc::new(root),
            records: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn create(&self, file: &UploadFile) -> AppResult<String> {
        let now = Instant::now();
        let mut records = lock_mutex(&self.records, "source session store")?;
        let expired = take_expired(&mut records, now);
        if let Err(error) = make_capacity(&mut records) {
            drop(records);
            cleanup_expired(expired);
            return Err(error);
        }
        let (id, path, temp_path) = match allocate_source_paths(&records, &self.root) {
            Ok(paths) => paths,
            Err(error) => {
                drop(records);
                cleanup_expired(expired);
                return Err(error);
            }
        };
        records.insert(
            id.clone(),
            SourceSessionRecord {
                filename: file.filename.clone(),
                path: path.clone(),
                last_used: now,
                state: SourceSessionState::Creating,
            },
        );
        drop(records);
        cleanup_expired(expired);

        if let Err(error) = self.write_source_file(&temp_path, &path, &file.bytes) {
            self.remove_reservation(&id);
            return Err(error);
        }

        let mut records = match lock_mutex(&self.records, "source session store") {
            Ok(records) => records,
            Err(error) => {
                let _ = remove_file_if_present(&path);
                return Err(error);
            }
        };
        let Some(record) = records.get_mut(&id) else {
            drop(records);
            let _ = remove_file_if_present(&path);
            return Err(AppError::Internal(
                "source session reservation disappeared".to_string(),
            ));
        };
        record.state = SourceSessionState::Available { active_leases: 0 };
        Ok(id)
    }

    pub(crate) fn create_from_path(&self, filename: &str, source: &Path) -> AppResult<String> {
        let now = Instant::now();
        let mut records = lock_mutex(&self.records, "source session store")?;
        let expired = take_expired(&mut records, now);
        if let Err(error) = make_capacity(&mut records) {
            drop(records);
            cleanup_expired(expired);
            return Err(error);
        }
        let (id, path, temp_path) = allocate_source_paths(&records, &self.root)?;
        records.insert(
            id.clone(),
            SourceSessionRecord {
                filename: filename.to_string(),
                path: path.clone(),
                last_used: now,
                state: SourceSessionState::Creating,
            },
        );
        drop(records);
        cleanup_expired(expired);
        if let Err(error) = self.copy_source_file(source, &temp_path, &path) {
            self.remove_reservation(&id);
            return Err(error);
        }
        let mut records = lock_mutex(&self.records, "source session store")?;
        let Some(record) = records.get_mut(&id) else {
            drop(records);
            let _ = remove_file_if_present(&path);
            return Err(AppError::Internal(
                "source session reservation disappeared".to_string(),
            ));
        };
        record.state = SourceSessionState::Available { active_leases: 0 };
        Ok(id)
    }

    fn copy_source_file(&self, source: &Path, temp_path: &Path, path: &Path) -> AppResult<()> {
        fs::create_dir_all(self.root.as_ref()).map_err(|error| {
            AppError::internal_cause("could not create prepared source directory", error)
        })?;
        if let Err(error) = fs::copy(source, temp_path) {
            let _ = remove_file_if_present(temp_path);
            return Err(AppError::internal_cause(
                "could not copy prepared source",
                error,
            ));
        }
        let temp = OpenOptions::new()
            .write(true)
            .open(temp_path)
            .map_err(|error| {
                AppError::internal_cause("could not open temporary prepared source", error)
            })?;
        if let Err(error) = temp.sync_all() {
            drop(temp);
            let _ = remove_file_if_present(temp_path);
            return Err(AppError::internal_cause(
                "could not flush temporary prepared source",
                error,
            ));
        }
        drop(temp);
        if let Err(error) = fs::rename(temp_path, path) {
            let _ = remove_file_if_present(temp_path);
            return Err(AppError::internal_cause(
                "could not commit prepared source",
                error,
            ));
        }
        Ok(())
    }

    fn write_source_file(&self, temp_path: &Path, path: &Path, bytes: &[u8]) -> AppResult<()> {
        fs::create_dir_all(self.root.as_ref()).map_err(|error| {
            AppError::internal_cause(
                format!(
                    "could not create prepared source directory `{}`",
                    self.root.display()
                ),
                error,
            )
        })?;
        let mut temp_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp_path)
            .map_err(|error| {
                AppError::internal_cause(
                    format!(
                        "could not create temporary prepared source `{}`",
                        temp_path.display()
                    ),
                    error,
                )
            })?;
        if let Err(error) = temp_file.write_all(bytes) {
            drop(temp_file);
            let _ = fs::remove_file(temp_path);
            return Err(AppError::internal_cause(
                format!(
                    "could not write temporary prepared source `{}`",
                    temp_path.display()
                ),
                error,
            ));
        }
        if let Err(error) = temp_file.sync_all() {
            drop(temp_file);
            let _ = fs::remove_file(temp_path);
            return Err(AppError::internal_cause(
                format!(
                    "could not flush temporary prepared source `{}`",
                    temp_path.display()
                ),
                error,
            ));
        }
        drop(temp_file);
        if let Err(error) = fs::rename(temp_path, path) {
            let _ = fs::remove_file(temp_path);
            return Err(AppError::internal_cause(
                format!(
                    "could not commit prepared source from `{}` to `{}`",
                    temp_path.display(),
                    path.display()
                ),
                error,
            ));
        }

        Ok(())
    }

    fn remove_reservation(&self, id: &str) {
        match lock_mutex(&self.records, "source session store") {
            Ok(mut records) => {
                records.remove(id);
            }
            Err(error) => {
                tracing::error!(source_id = id, %error, "failed to release source session reservation");
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn load(&self, id: &str) -> AppResult<UploadFile> {
        let file = self.load_path(id)?;
        let bytes = match fs::read(&file.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err(AppError::internal_cause(
                    format!("could not read prepared source `{}`", file.path.display()),
                    error,
                ));
            }
        };
        Ok(UploadFile {
            filename: file.filename.clone(),
            bytes: bytes.into(),
        })
    }

    pub(crate) fn load_path(&self, id: &str) -> AppResult<SourceSessionFile> {
        validate_source_id(id)?;
        let now = Instant::now();
        let mut records = lock_mutex(&self.records, "source session store")?;
        let expired = take_expired(&mut records, now);
        let result = match records.get_mut(id) {
            Some(record) => match &mut record.state {
                SourceSessionState::Available { active_leases } => {
                    *active_leases = active_leases.checked_add(1).ok_or_else(|| {
                        AppError::Internal("source session lease count overflowed".to_string())
                    })?;
                    record.last_used = now;
                    Ok(SourceSessionFile {
                        filename: record.filename.clone(),
                        path: record.path.clone(),
                        _lease: SourceSessionLease {
                            id: id.to_string(),
                            path: record.path.clone(),
                            records: self.records.clone(),
                        },
                    })
                }
                SourceSessionState::Creating
                | SourceSessionState::RemovalPending { .. }
                | SourceSessionState::Removing => Err(AppError::bad_request(
                    "prepared source PDF was not found or expired",
                )),
            },
            None => Err(AppError::bad_request(
                "prepared source PDF was not found or expired",
            )),
        };
        drop(records);
        cleanup_expired(expired);
        let file = result?;
        if !file.path.is_file() {
            mark_removal_pending(&self.records, id, &file.path)?;
            return Err(AppError::bad_request(
                "prepared source PDF was not found or expired",
            ));
        }
        Ok(file)
    }

    pub(crate) fn renew(&self, id: &str) -> AppResult<()> {
        validate_source_id(id)?;
        let now = Instant::now();
        let mut records = lock_mutex(&self.records, "source session store")?;
        let expired = take_expired(&mut records, now);
        let result = records
            .get_mut(id)
            .filter(|record| matches!(record.state, SourceSessionState::Available { .. }))
            .ok_or_else(|| AppError::bad_request("prepared source PDF was not found or expired"))
            .map(|record| {
                record.last_used = now;
            });
        drop(records);
        cleanup_expired(expired);
        result
    }

    pub(crate) fn delete(&self, id: &str) -> AppResult<()> {
        validate_source_id(id)?;
        let mut records = lock_mutex(&self.records, "source session store")?;
        let Some(record) = records.get_mut(id) else {
            return Ok(());
        };
        let (active_leases, restore_on_failure) = match record.state {
            SourceSessionState::Available { active_leases } => (active_leases, true),
            SourceSessionState::Removing => (0, false),
            SourceSessionState::Creating | SourceSessionState::RemovalPending { .. } => {
                return Ok(())
            }
        };
        if let Some(active_leases) = NonZeroUsize::new(active_leases) {
            record.state = SourceSessionState::RemovalPending { active_leases };
            return Ok(());
        }
        record.state = SourceSessionState::Removing;
        let path = record.path.clone();
        drop(records);

        if let Err(error) = remove_file_if_present(&path) {
            let mut records = lock_mutex(&self.records, "source session store")?;
            if restore_on_failure {
                if let Some(record) = records.get_mut(id).filter(|record| record.path == path) {
                    record.state = SourceSessionState::Available { active_leases: 0 };
                }
            }
            return Err(error);
        }
        let mut records = lock_mutex(&self.records, "source session store")?;
        records.remove(id);
        Ok(())
    }
}

fn validate_source_id(id: &str) -> AppResult<()> {
    if valid_identifier(id) {
        Ok(())
    } else {
        Err(AppError::bad_request("sourceId is invalid"))
    }
}

fn random_source_id() -> AppResult<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| AppError::internal_cause("could not allocate source session", error))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn allocate_source_paths(
    records: &HashMap<String, SourceSessionRecord>,
    root: &Path,
) -> AppResult<(String, PathBuf, PathBuf)> {
    const MAX_ID_ATTEMPTS: usize = 4;

    for _ in 0..MAX_ID_ATTEMPTS {
        let id = random_source_id()?;
        let path = root.join(format!("source-{id}.pdf"));
        let temp_path = root.join(format!("source-{id}.tmp"));
        if !records.contains_key(&id) && !path.exists() && !temp_path.exists() {
            return Ok((id, path, temp_path));
        }
    }
    Err(AppError::Internal(
        "could not allocate a unique prepared source id".to_string(),
    ))
}

fn make_capacity(records: &mut HashMap<String, SourceSessionRecord>) -> AppResult<()> {
    if records.len() >= MAX_SOURCE_SESSIONS {
        Err(AppError::unavailable(
            "prepared source storage is full; try again later",
        ))
    } else {
        Ok(())
    }
}

fn take_expired(
    records: &mut HashMap<String, SourceSessionRecord>,
    now: Instant,
) -> Vec<(String, SourceSessionRecord)> {
    let expired = records
        .iter()
        .filter(|(_, record)| {
            matches!(record.state, SourceSessionState::Available { .. })
                && now.duration_since(record.last_used) >= SOURCE_SESSION_TTL
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let mut cleanup = Vec::new();
    for id in expired {
        let Some(record) = records.get_mut(&id) else {
            continue;
        };
        let SourceSessionState::Available { active_leases } = record.state else {
            continue;
        };
        if let Some(active_leases) = NonZeroUsize::new(active_leases) {
            record.state = SourceSessionState::RemovalPending { active_leases };
        } else if let Some(record) = records.remove(&id) {
            cleanup.push((id, record));
        }
    }
    cleanup
}

fn mark_removal_pending(
    records: &Arc<Mutex<HashMap<String, SourceSessionRecord>>>,
    id: &str,
    path: &Path,
) -> AppResult<()> {
    let mut records = lock_mutex(records, "source session store")?;
    let Some(record) = records.get_mut(id).filter(|record| record.path == path) else {
        return Ok(());
    };
    if let SourceSessionState::Available { active_leases } = record.state {
        if let Some(active_leases) = NonZeroUsize::new(active_leases) {
            record.state = SourceSessionState::RemovalPending { active_leases };
        }
    }
    Ok(())
}

fn release_lease(
    records: &mut HashMap<String, SourceSessionRecord>,
    id: &str,
    path: &Path,
) -> Option<PathBuf> {
    let record = records.get_mut(id).filter(|record| record.path == path)?;
    match record.state {
        SourceSessionState::Available {
            ref mut active_leases,
        } => {
            let Some(remaining) = active_leases.checked_sub(1) else {
                tracing::error!(
                    source_id = id,
                    "source session lease count was already zero"
                );
                return None;
            };
            *active_leases = remaining;
            None
        }
        SourceSessionState::RemovalPending { active_leases } => {
            if let Some(remaining) = NonZeroUsize::new(active_leases.get() - 1) {
                record.state = SourceSessionState::RemovalPending {
                    active_leases: remaining,
                };
                None
            } else {
                record.state = SourceSessionState::Removing;
                Some(record.path.clone())
            }
        }
        SourceSessionState::Creating | SourceSessionState::Removing => None,
    }
}

fn remove_cleaned_record(
    records: &Arc<Mutex<HashMap<String, SourceSessionRecord>>>,
    id: &str,
    path: &Path,
) {
    match lock_mutex(records, "source session store") {
        Ok(mut records) => {
            if records.get(id).is_some_and(|record| {
                record.path == path && matches!(record.state, SourceSessionState::Removing)
            }) {
                records.remove(id);
            }
        }
        Err(error) => {
            tracing::error!(source_id = id, %error, "failed to finalize source session cleanup");
        }
    }
}

fn cleanup_expired(expired: Vec<(String, SourceSessionRecord)>) {
    for (id, record) in expired {
        if let Err(error) = remove_file_if_present(&record.path) {
            tracing::warn!(source_id = id, %error, "expired source session could not be removed");
        }
    }
}

fn remove_file_if_present(path: &Path) -> AppResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_orphaned_sessions(root: &Path) -> AppResult<()> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("source-") && (name.ends_with(".pdf") || name.ends_with(".tmp")) {
            remove_file_if_present(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SourceSessionStore {
        let unique = random_source_id().unwrap();
        SourceSessionStore::new(
            std::env::temp_dir().join(format!("pdf-tools-source-session-test-{unique}")),
        )
    }

    #[test]
    fn source_session_roundtrips_and_deletes_prepared_pdf() {
        let store = store();
        let file = UploadFile {
            filename: "source.pdf".to_string(),
            bytes: b"%PDF-session".to_vec().into(),
        };

        let id = store.create(&file).unwrap();
        let loaded = store.load(&id).unwrap();
        assert_eq!(loaded.filename, "source.pdf");
        assert_eq!(loaded.bytes.as_ref(), b"%PDF-session");

        store.delete(&id).unwrap();
        assert!(store.load(&id).is_err());
    }

    #[test]
    fn source_session_rejects_unsafe_identifiers() {
        let store = store();
        assert!(store.load("../source").is_err());
        assert!(store.delete("bad/source").is_err());
    }

    #[test]
    fn source_session_capacity_never_evicts_a_live_session() {
        let store = store();
        let file = UploadFile {
            filename: "source.pdf".to_string(),
            bytes: b"%PDF-session".to_vec().into(),
        };
        let mut ids = Vec::new();
        for _ in 0..MAX_SOURCE_SESSIONS {
            ids.push(store.create(&file).unwrap());
        }

        assert!(store.create(&file).is_err());
        assert_eq!(store.load(&ids[0]).unwrap().bytes.as_ref(), b"%PDF-session");

        for id in ids {
            store.delete(&id).unwrap();
        }
        assert!(store.create(&file).is_ok());
    }

    #[test]
    fn source_session_renewal_extends_a_live_lease_but_not_an_expired_one() {
        let store = store();
        let file = UploadFile {
            filename: "source.pdf".to_string(),
            bytes: b"%PDF-session".to_vec().into(),
        };
        let id = store.create(&file).unwrap();
        let path = {
            let mut records = store.records.lock().unwrap();
            let record = records.get_mut(&id).unwrap();
            record.last_used = Instant::now() - SOURCE_SESSION_TTL + Duration::from_secs(1);
            record.path.clone()
        };

        store.renew(&id).unwrap();
        {
            let records = store.records.lock().unwrap();
            assert!(records[&id].last_used.elapsed() < Duration::from_secs(1));
        }

        {
            let mut records = store.records.lock().unwrap();
            records.get_mut(&id).unwrap().last_used = Instant::now() - SOURCE_SESSION_TTL;
        }
        assert!(store.renew(&id).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn failed_artifact_delete_keeps_the_session_retryable() {
        let store = store();
        let file = UploadFile {
            filename: "source.pdf".to_string(),
            bytes: b"%PDF-session".to_vec().into(),
        };
        let id = store.create(&file).unwrap();
        let path = {
            let records = store.records.lock().unwrap();
            records.get(&id).unwrap().path.clone()
        };
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();

        assert!(store.delete(&id).is_err());
        assert!(store.records.lock().unwrap().contains_key(&id));

        fs::remove_dir(&path).unwrap();
        store.delete(&id).unwrap();
        assert!(!store.records.lock().unwrap().contains_key(&id));
    }

    #[test]
    fn missing_artifacts_release_session_capacity() {
        let store = store();
        let file = UploadFile {
            filename: "source.pdf".to_string(),
            bytes: b"%PDF-session".to_vec().into(),
        };
        let id = store.create(&file).unwrap();
        let path = {
            let records = store.records.lock().unwrap();
            records.get(&id).unwrap().path.clone()
        };
        fs::remove_file(path).unwrap();

        assert!(store.load(&id).is_err());
        assert!(!store.records.lock().unwrap().contains_key(&id));
    }

    #[test]
    fn delete_during_active_preview_lease_defers_cleanup_and_rejects_new_loads() {
        let store = store();
        let id = store.create(&test_file()).unwrap();
        let preview_lease = store.load_path(&id).unwrap();
        let path = preview_lease.path.clone();

        store.delete(&id).unwrap();

        assert!(path.is_file());
        assert!(store.load_path(&id).is_err());
        drop(preview_lease);
        assert!(!path.exists());
        assert!(!store.records.lock().unwrap().contains_key(&id));
    }

    #[test]
    fn delete_during_active_export_lease_waits_for_all_lease_copies_to_finish() {
        let store = store();
        let id = store.create(&test_file()).unwrap();
        let first_export_lease = store.load_path(&id).unwrap();
        let final_export_lease = store.load_path(&id).unwrap();
        let path = first_export_lease.path.clone();

        store.delete(&id).unwrap();
        drop(first_export_lease);
        assert!(path.is_file());
        drop(final_export_lease);

        assert!(!path.exists());
    }

    #[test]
    fn expiry_during_a_lease_defers_cleanup_and_cannot_be_renewed_or_loaded() {
        let store = store();
        let id = store.create(&test_file()).unwrap();
        let lease = store.load_path(&id).unwrap();
        let path = lease.path.clone();
        {
            let mut records = store.records.lock().unwrap();
            records.get_mut(&id).unwrap().last_used = Instant::now() - SOURCE_SESSION_TTL;
        }

        assert!(store.renew(&id).is_err());
        assert!(store.load_path(&id).is_err());
        assert!(path.is_file());
        drop(lease);

        assert!(!path.exists());
        assert!(!store.records.lock().unwrap().contains_key(&id));
    }

    fn test_file() -> UploadFile {
        UploadFile {
            filename: "source.pdf".to_string(),
            bytes: b"%PDF-session".to_vec().into(),
        }
    }
}
