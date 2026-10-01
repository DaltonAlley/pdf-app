use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use super::{
    layout::validate_request,
    model::{GangUpExportRecord, GangUpExportRecordInput, GangUpExportType},
    persistence::{read_json_or_default, valid_identifier, write_json_atomic},
};
use crate::{
    adapters::{validate_download_size, FileDownload},
    error::{lock_mutex, AppError, AppResult},
};

const EXPORT_HISTORY_FILE: &str = "gang-up-export-history.json";
const EXPORT_HISTORY_FILES_DIR: &str = "gang-up-export-files";
const MAX_EXPORT_HISTORY: usize = 30;
const MAX_STORED_EXPORT_BYTES: usize = 256 * 1024 * 1024;
const MAX_TOTAL_STORED_EXPORT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct GangUpExportHistoryStore {
    path: Arc<PathBuf>,
    write_lock: Arc<Mutex<()>>,
}

impl GangUpExportHistoryStore {
    pub(crate) fn new(data_dir: PathBuf) -> Self {
        Self {
            path: Arc::new(data_dir.join(EXPORT_HISTORY_FILE)),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn list(&self) -> AppResult<Vec<GangUpExportRecord>> {
        let _guard = lock_mutex(&self.write_lock, "gang-up export history store")?;
        self.read_saved_unlocked()
    }

    pub(crate) fn create(&self, input: GangUpExportRecordInput) -> AppResult<GangUpExportRecord> {
        reject_legacy_export_type(input.output_type)?;
        validate_request(&input.request)?;
        if input.stored_file.is_some() {
            return Err(AppError::bad_request(
                "client-provided export files are not supported; use a generated export",
            ));
        }
        self.create_with_bytes(input, None)
    }

    pub(crate) fn create_generated(
        &self,
        input: GangUpExportRecordInput,
        bytes: &[u8],
    ) -> AppResult<GangUpExportRecord> {
        reject_legacy_export_type(input.output_type)?;
        validate_request(&input.request)?;
        validate_stored_bytes(bytes)?;
        self.create_with_bytes(input, Some(bytes))
    }

    fn create_with_bytes(
        &self,
        mut input: GangUpExportRecordInput,
        stored_bytes: Option<&[u8]>,
    ) -> AppResult<GangUpExportRecord> {
        input.request.source_id = None;
        let _guard = lock_mutex(&self.write_lock, "gang-up export history store")?;
        let mut records = self.read_saved_unlocked()?;
        self.reconcile_orphans_unlocked(&records)?;
        let now = unix_timestamp();
        let id = format!("export-{now}");
        let output_filename = input.output_filename.trim().to_string();
        let name = input
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| fallback_name(input.source_filename.as_deref(), input.output_type));
        let record = GangUpExportRecord {
            id: unique_id(&id, &records)?,
            name,
            source_filename: input.source_filename.filter(|name| !name.trim().is_empty()),
            exported_at: now.to_string(),
            output_type: input.output_type,
            output_filename,
            has_stored_file: stored_bytes.is_some(),
            request: input.request,
            layout_summary: input.layout_summary,
        };
        if let Some(bytes) = stored_bytes {
            self.write_file_unlocked(&record.id, bytes)?;
        }

        let commit = (|| {
            records.insert(0, record.clone());
            let mut removed = records.split_off(records.len().min(MAX_EXPORT_HISTORY));
            while self.stored_bytes_unlocked(&records)? > MAX_TOTAL_STORED_EXPORT_BYTES {
                let Some(old_record) = records.pop() else {
                    break;
                };
                removed.push(old_record);
            }
            self.write_saved_unlocked(&records)?;
            Ok::<_, AppError>(removed)
        })();
        let removed = match commit {
            Ok(removed) => removed,
            Err(error) => {
                if record.has_stored_file {
                    if let Err(cleanup_error) = self.delete_file_unlocked(&record.id) {
                        tracing::warn!(
                            export_id = record.id,
                            error = %cleanup_error,
                            "failed to roll back export artifact after metadata commit failure"
                        );
                    }
                }
                return Err(error);
            }
        };
        if record.has_stored_file && !records.iter().any(|saved| saved.id == record.id) {
            if let Err(error) = self.delete_file_unlocked(&record.id) {
                tracing::warn!(
                    export_id = record.id,
                    %error,
                    "failed to remove export artifact rejected by the total storage limit"
                );
            }
            return Err(AppError::payload_too_large(format!(
                "stored exports are limited to {} MB in total",
                MAX_TOTAL_STORED_EXPORT_BYTES / 1024 / 1024
            )));
        }
        for old_record in removed {
            // Orphans are reconciled before the next write if this cleanup is interrupted.
            if let Err(error) = self.delete_file_unlocked(&old_record.id) {
                tracing::warn!(
                    export_id = old_record.id,
                    %error,
                    "failed to remove expired export artifact"
                );
            }
        }
        Ok(record)
    }

    fn reconcile_orphans_unlocked(&self, records: &[GangUpExportRecord]) -> AppResult<()> {
        let dir = self.files_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(AppError::internal_cause(
                    format!(
                        "could not read export artifact directory `{}`",
                        dir.display()
                    ),
                    error,
                ));
            }
        };
        for entry in entries {
            let entry = entry.map_err(|error| {
                AppError::internal_cause(
                    format!(
                        "could not inspect an entry in export artifact directory `{}`",
                        dir.display()
                    ),
                    error,
                )
            })?;
            let path = entry.path();
            if path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.ends_with(".bin.tmp"))
            {
                remove_artifact_path(&path, "temporary export artifact")?;
                continue;
            }
            if path.extension().and_then(|value| value.to_str()) != Some("bin") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if !valid_identifier(id) || !records.iter().any(|record| record.id == id) {
                remove_artifact_path(&path, "orphaned export artifact")?;
            }
        }
        Ok(())
    }

    pub(crate) fn download(
        &self,
        id: &str,
        max_download_bytes: Option<usize>,
    ) -> AppResult<FileDownload> {
        if !valid_identifier(id) {
            return Err(AppError::bad_request("invalid export history id"));
        }
        let _guard = lock_mutex(&self.write_lock, "gang-up export history store")?;
        let records = self.read_saved_unlocked()?;
        let record = records
            .iter()
            .find(|record| record.id == id)
            .ok_or_else(|| AppError::bad_request("export history record not found"))?;
        if !record.has_stored_file {
            return Err(AppError::bad_request(
                "export history record does not have a stored file",
            ));
        }
        let path = self.file_path(id);
        let metadata = fs::metadata(&path).map_err(|error| {
            AppError::internal_cause(
                format!(
                    "could not inspect stored export artifact `{}`",
                    path.display()
                ),
                error,
            )
        })?;
        if metadata.len() == 0 || metadata.len() > MAX_STORED_EXPORT_BYTES as u64 {
            return Err(AppError::Internal(
                "stored export artifact has an invalid size".to_string(),
            ));
        }
        let stored_size = usize::try_from(metadata.len()).map_err(|_| {
            AppError::Internal("stored export artifact size is not supported".to_string())
        })?;
        validate_download_size(stored_size, max_download_bytes)?;
        let bytes = fs::read(&path).map_err(|error| {
            AppError::internal_cause(
                format!("could not read stored export artifact `{}`", path.display()),
                error,
            )
        })?;
        Ok(FileDownload::new(
            content_type(record.output_type),
            record.output_filename.clone(),
            bytes,
        ))
    }

    pub(crate) fn delete(&self, id: &str) -> AppResult<()> {
        if !valid_identifier(id) {
            return Err(AppError::bad_request("invalid export history id"));
        }
        let _guard = lock_mutex(&self.write_lock, "gang-up export history store")?;
        let mut records = self.read_saved_unlocked()?;
        let before = records.len();
        records.retain(|record| record.id != id);
        if records.len() == before {
            return Err(AppError::bad_request("export history record not found"));
        }
        self.write_saved_unlocked(&records)?;
        // Metadata is already committed; artifact cleanup is best effort.
        if let Err(error) = self.delete_file_unlocked(id) {
            tracing::warn!(export_id = id, %error, "failed to remove deleted export artifact");
        }
        Ok(())
    }

    fn read_saved_unlocked(&self) -> AppResult<Vec<GangUpExportRecord>> {
        let records: Vec<GangUpExportRecord> =
            read_json_or_default(self.path.as_path(), "gang-up export history")?;
        if records.len() > MAX_EXPORT_HISTORY
            || records.iter().any(|record| {
                !valid_identifier(&record.id) || validate_request(&record.request).is_err()
            })
        {
            return Err(AppError::Internal(
                "gang-up export history metadata is invalid".to_string(),
            ));
        }
        Ok(records)
    }

    fn write_saved_unlocked(&self, records: &[GangUpExportRecord]) -> AppResult<()> {
        write_json_atomic(self.path.as_path(), records, "gang-up export history")
    }

    fn write_file_unlocked(&self, id: &str, bytes: &[u8]) -> AppResult<()> {
        if !valid_identifier(id) {
            return Err(AppError::Internal("invalid export artifact id".to_string()));
        }
        validate_stored_bytes(bytes)?;
        let dir = self.files_dir();
        fs::create_dir_all(&dir).map_err(|error| {
            AppError::internal_cause(
                format!(
                    "could not create export artifact directory `{}`",
                    dir.display()
                ),
                error,
            )
        })?;
        let final_path = self.file_path(id);
        let temp_path = final_path.with_extension("bin.tmp");
        let mut temp_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| {
                AppError::internal_cause(
                    format!(
                        "could not create temporary export artifact `{}`",
                        temp_path.display()
                    ),
                    error,
                )
            })?;
        if let Err(error) = temp_file.write_all(bytes) {
            drop(temp_file);
            let _ = fs::remove_file(&temp_path);
            return Err(AppError::internal_cause(
                format!(
                    "could not write temporary export artifact `{}`",
                    temp_path.display()
                ),
                error,
            ));
        }
        if let Err(error) = temp_file.sync_all() {
            drop(temp_file);
            let _ = fs::remove_file(&temp_path);
            return Err(AppError::internal_cause(
                format!(
                    "could not flush temporary export artifact `{}`",
                    temp_path.display()
                ),
                error,
            ));
        }
        drop(temp_file);
        if let Err(error) = fs::rename(&temp_path, &final_path) {
            let _ = fs::remove_file(&temp_path);
            return Err(AppError::internal_cause(
                format!(
                    "could not commit export artifact from `{}` to `{}`",
                    temp_path.display(),
                    final_path.display()
                ),
                error,
            ));
        }
        Ok(())
    }

    fn delete_file_unlocked(&self, id: &str) -> AppResult<()> {
        let path = self.file_path(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AppError::internal_cause(
                format!("could not remove export artifact `{}`", path.display()),
                error,
            )),
        }
    }

    fn files_dir(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(EXPORT_HISTORY_FILES_DIR)
    }

    fn file_path(&self, id: &str) -> PathBuf {
        self.files_dir().join(format!("{id}.bin"))
    }

    fn stored_bytes_unlocked(&self, records: &[GangUpExportRecord]) -> AppResult<u64> {
        records
            .iter()
            .filter(|record| record.has_stored_file)
            .try_fold(0u64, |total, record| {
                let size = match fs::metadata(self.file_path(&record.id)) {
                    Ok(metadata) => metadata.len(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let path = self.file_path(&record.id);
                        return Err(AppError::Internal(format!(
                            "stored export artifact `{}` is missing",
                            path.display()
                        )));
                    }
                    Err(error) => {
                        let path = self.file_path(&record.id);
                        return Err(AppError::internal_cause(
                            format!(
                                "could not inspect stored export artifact `{}`",
                                path.display()
                            ),
                            error,
                        ));
                    }
                };
                Ok(total.saturating_add(size))
            })
    }
}

fn remove_artifact_path(path: &Path, description: &str) -> AppResult<()> {
    fs::remove_file(path).map_err(|error| {
        AppError::internal_cause(
            format!("could not remove {description} `{}`", path.display()),
            error,
        )
    })
}

fn validate_stored_bytes(bytes: &[u8]) -> AppResult<()> {
    if bytes.is_empty() {
        return Err(AppError::bad_request("stored export file cannot be empty"));
    }
    if bytes.len() > MAX_STORED_EXPORT_BYTES {
        return Err(AppError::bad_request(format!(
            "stored export file exceeds the {} MB limit",
            MAX_STORED_EXPORT_BYTES / 1024 / 1024
        )));
    }
    Ok(())
}

fn fallback_name(filename: Option<&str>, output_type: GangUpExportType) -> String {
    let output = match output_type {
        GangUpExportType::CleanPdf => "Clean PDF",
        GangUpExportType::LegacyDuploCutPlan => "Duplo cut-plan",
    };
    filename
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| format!("{name} - {output}"))
        .unwrap_or_else(|| output.to_string())
}

fn content_type(output_type: GangUpExportType) -> &'static str {
    match output_type {
        GangUpExportType::CleanPdf => "application/pdf",
        GangUpExportType::LegacyDuploCutPlan => "application/json",
    }
}

fn reject_legacy_export_type(output_type: GangUpExportType) -> AppResult<()> {
    match output_type {
        GangUpExportType::CleanPdf => Ok(()),
        GangUpExportType::LegacyDuploCutPlan => Err(AppError::bad_request(
            "Duplo cut-plan exports are no longer supported",
        )),
    }
}

fn unique_id(base: &str, records: &[GangUpExportRecord]) -> AppResult<String> {
    if records.iter().all(|record| record.id != base) {
        return Ok(base.to_string());
    }

    for suffix in 2..=MAX_EXPORT_HISTORY + 1 {
        let candidate = format!("{base}-{suffix}");
        if records.iter().all(|record| record.id != candidate) {
            return Ok(candidate);
        }
    }
    Err(AppError::Internal(
        "could not allocate a unique export history id".to_string(),
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

    fn temp_data_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pdf-tools-export-store-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn export_input() -> GangUpExportRecordInput {
        serde_json::from_value(serde_json::json!({
            "name": "Cards - Clean PDF",
            "sourceFilename": "cards.pdf",
            "outputType": "cleanPdf",
            "outputFilename": "gang-up-imposed.pdf",
            "request": {
                "sourcePdfSize": { "width": 3.5, "height": 2.0 },
                "sourcePageCount": 1,
                "finishedCutSize": { "width": 3.5, "height": 2.0 },
                "parentSheetSize": { "width": 12.0, "height": 18.0 },
                "quantityRequested": 100,
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

    #[test]
    fn legacy_duplo_history_does_not_block_new_pdf_exports() {
        let data_dir = temp_data_dir("legacy-duplo");
        fs::create_dir_all(&data_dir).unwrap();
        let legacy_record = GangUpExportRecord {
            id: "export-legacy".to_string(),
            name: "Cards - Duplo cut-plan".to_string(),
            source_filename: Some("cards.pdf".to_string()),
            exported_at: "1".to_string(),
            output_type: GangUpExportType::LegacyDuploCutPlan,
            output_filename: "gang-up-duplo-cut-plan.json".to_string(),
            has_stored_file: false,
            request: export_input().request,
            layout_summary: None,
        };
        fs::write(
            data_dir.join(EXPORT_HISTORY_FILE),
            serde_json::to_vec(&vec![legacy_record]).unwrap(),
        )
        .unwrap();
        let store = GangUpExportHistoryStore::new(data_dir.clone());

        store
            .create_generated(export_input(), b"%PDF-test")
            .unwrap();

        let records = store.list().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].output_type, GangUpExportType::CleanPdf);
        assert_eq!(records[1].output_type, GangUpExportType::LegacyDuploCutPlan);
        fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn new_legacy_duplo_history_records_are_rejected() {
        let data_dir = temp_data_dir("reject-legacy-duplo");
        let store = GangUpExportHistoryStore::new(data_dir.clone());
        let mut input = export_input();
        input.output_type = GangUpExportType::LegacyDuploCutPlan;

        assert!(store.create(input).is_err());
        assert!(!data_dir.exists());
    }

    #[test]
    fn generated_file_is_removed_when_metadata_commit_fails() {
        let data_dir = temp_data_dir("metadata-rollback");
        let store = GangUpExportHistoryStore::new(data_dir.clone());
        let mut input = export_input();
        input.name = Some("x".repeat(super::super::persistence::MAX_METADATA_BYTES as usize + 1));

        assert!(store.create_generated(input, b"%PDF-test").is_err());

        let files_dir = data_dir.join(EXPORT_HISTORY_FILES_DIR);
        assert!(files_dir.exists());
        assert_eq!(fs::read_dir(files_dir).unwrap().count(), 0);
        fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn generated_file_failure_does_not_create_metadata() {
        let data_dir = temp_data_dir("artifact-failure");
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(data_dir.join(EXPORT_HISTORY_FILES_DIR), b"not a directory").unwrap();
        let store = GangUpExportHistoryStore::new(data_dir.clone());

        assert!(store
            .create_generated(export_input(), b"%PDF-test")
            .is_err());
        assert!(!data_dir.join(EXPORT_HISTORY_FILE).exists());
        fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn delete_succeeds_after_metadata_commit_when_artifact_cleanup_fails() {
        let data_dir = temp_data_dir("delete-cleanup");
        let store = GangUpExportHistoryStore::new(data_dir.clone());
        let record = store
            .create_generated(export_input(), b"%PDF-test")
            .unwrap();
        let artifact_path = store.file_path(&record.id);
        fs::remove_file(&artifact_path).unwrap();
        fs::create_dir(&artifact_path).unwrap();

        store.delete(&record.id).unwrap();

        assert!(store.list().unwrap().is_empty());
        assert!(artifact_path.is_dir());
        fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn reconciliation_ignores_unrelated_entries_and_cleans_temp_artifacts() {
        let data_dir = temp_data_dir("orphan-reconciliation");
        let files_dir = data_dir.join(EXPORT_HISTORY_FILES_DIR);
        fs::create_dir_all(&files_dir).unwrap();
        for index in 0..(MAX_EXPORT_HISTORY * 4 + 10) {
            fs::write(files_dir.join(format!("unrelated-{index}.txt")), b"x").unwrap();
        }
        let temp_artifact = files_dir.join("interrupted.bin.tmp");
        fs::write(&temp_artifact, b"partial").unwrap();
        let store = GangUpExportHistoryStore::new(data_dir.clone());

        assert!(store.create_generated(export_input(), b"%PDF-test").is_ok());
        assert!(!temp_artifact.exists());
        assert!(files_dir.join("unrelated-0.txt").exists());
        fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn reconciliation_removes_more_than_the_retained_history_limit_of_orphans() {
        let data_dir = temp_data_dir("many-orphan-artifacts");
        let files_dir = data_dir.join(EXPORT_HISTORY_FILES_DIR);
        fs::create_dir_all(&files_dir).unwrap();
        for index in 0..(MAX_EXPORT_HISTORY * 4 + 10) {
            fs::write(files_dir.join(format!("orphan-{index}.bin")), b"stale").unwrap();
        }
        let store = GangUpExportHistoryStore::new(data_dir.clone());

        let record = store
            .create_generated(export_input(), b"%PDF-test")
            .unwrap();

        let entries = fs::read_dir(&files_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![store.file_path(&record.id)]);
        fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn stored_download_obeys_the_configured_response_limit() {
        let data_dir = temp_data_dir("download-limit");
        let store = GangUpExportHistoryStore::new(data_dir.clone());
        let record = store
            .create_generated(export_input(), b"%PDF-test")
            .unwrap();

        let error = match store.download(&record.id, Some(1)) {
            Ok(_) => panic!("stored download should honor the configured limit"),
            Err(error) => error,
        };
        assert!(matches!(error, AppError::PayloadTooLarge(_)));
        assert!(store.download(&record.id, None).is_ok());

        fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn missing_referenced_artifacts_are_rejected_during_capacity_checks() {
        let data_dir = temp_data_dir("missing-artifact");
        let store = GangUpExportHistoryStore::new(data_dir.clone());
        let record = store
            .create_generated(export_input(), b"%PDF-test")
            .unwrap();
        fs::remove_file(store.file_path(&record.id)).unwrap();

        let error = store
            .create_generated(export_input(), b"%PDF-another")
            .unwrap_err();

        assert!(error.to_string().contains("is missing"));
        fs::remove_dir_all(data_dir).unwrap();
    }
}
