use std::sync::Arc;
use std::{fmt, str::FromStr};

use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};

use super::{convert, gang_up, pdf_ops, state::OperationPermit};
use crate::{
    adapters::{read_multipart, FileDownload, StagedImposeForm},
    error::{AppError, AppResult},
    progress::ProgressCallback,
    AppState,
};

pub(crate) fn create_staged_prepare(
    state: Arc<AppState>,
    staged: StagedImposeForm,
    operation_permit: OperationPermit,
) -> AppResult<Response> {
    let created_job = state.jobs.create_cancellable()?;
    let job_id = created_job.id;
    let cancellation = created_job.cancellation;
    let worker_state = state.clone();
    let worker_job_id = job_id.clone();
    let monitor_jobs = state.jobs.clone();
    let monitor_job_id = job_id.clone();
    let worker = tokio::spawn(async move {
        let progress_state = worker_state.clone();
        let progress_job_id = worker_job_id.clone();
        let progress_cancellation = cancellation.clone();
        let progress: ProgressCallback = Arc::new(move |percent, stage| {
            if progress_cancellation.is_cancelled() {
                return true;
            }
            !matches!(
                progress_state.jobs.update(&progress_job_id, percent, stage),
                Ok(crate::jobs::JobMutationOutcome::Applied)
            )
        });
        let _ = worker_state
            .jobs
            .update(&worker_job_id, 21, "Upload staged on disk");
        let result = gang_up::prepare_staged_source(
            worker_state.clone(),
            staged,
            operation_permit,
            Some(progress),
        )
        .await;
        finish_prepare_job(&worker_state, &worker_job_id, &cancellation, result);
    });
    spawn_staged_prepare_monitor(monitor_jobs, monitor_job_id, worker);
    Ok((
        StatusCode::ACCEPTED,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        job_id,
    )
        .into_response())
}

fn spawn_staged_prepare_monitor(
    jobs: crate::jobs::JobStore,
    job_id: String,
    worker: tokio::task::JoinHandle<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = worker.await {
            tracing::error!(
                %job_id,
                %error,
                "staged preparation task terminated unexpectedly"
            );
            if let Err(store_error) = jobs.fail(&job_id, "internal server error") {
                tracing::error!(
                    %job_id,
                    error = %store_error,
                    "unexpected staged preparation failure could not be stored"
                );
            }
        }
    })
}

fn finish_prepare_job(
    state: &AppState,
    job_id: &str,
    cancellation: &crate::jobs::JobCancellation,
    result: AppResult<FileDownload>,
) {
    if cancellation.is_cancelled() {
        cleanup_prepared_result(state, &result);
        let _ = state.jobs.cancel(job_id);
        return;
    }
    match result {
        Ok(file) => {
            let _ = state.jobs.complete(job_id, file);
        }
        Err(error) => {
            tracing::error!(%job_id, error = %error, "staged source preparation failed");
            let _ = state.jobs.fail(job_id, error.client_message());
        }
    }
}

fn cleanup_prepared_result(state: &AppState, result: &AppResult<FileDownload>) {
    let Ok(file) = result else { return };
    let Ok(response) = serde_json::from_slice::<serde_json::Value>(file.bytes.as_ref()) else {
        return;
    };
    let Some(source_id) = response.get("sourceId").and_then(|id| id.as_str()) else {
        return;
    };
    if let Err(error) = state.source_session_store().delete(source_id) {
        tracing::warn!(%error, "failed to clean cancelled source session");
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobAction {
    Convert,
    Merge,
    Split,
    GangUpAnalyze,
    GangUpPrepare,
    GangUpExport,
    GangUpExportSource,
}

impl JobAction {
    fn is_prepare(self) -> bool {
        matches!(self, Self::GangUpPrepare)
    }
}

impl FromStr for JobAction {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "convert" => Ok(Self::Convert),
            "merge" => Ok(Self::Merge),
            "split" => Ok(Self::Split),
            "gang-up-analyze" => Ok(Self::GangUpAnalyze),
            "gang-up-prepare" => Ok(Self::GangUpPrepare),
            "gang-up-export" => Ok(Self::GangUpExport),
            "gang-up-export-source" => Ok(Self::GangUpExportSource),
            _ => Err(AppError::bad_request("unknown job action")),
        }
    }
}

impl fmt::Display for JobAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Convert => "convert",
            Self::Merge => "merge",
            Self::Split => "split",
            Self::GangUpAnalyze => "gang-up-analyze",
            Self::GangUpPrepare => "gang-up-prepare",
            Self::GangUpExport => "gang-up-export",
            Self::GangUpExportSource => "gang-up-export-source",
        })
    }
}

pub(crate) async fn create(
    State(state): State<Arc<AppState>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let form = read_multipart(multipart).await?;
    let action = form.required_field("action")?.parse::<JobAction>()?;
    let operation_permit = state.admit_operation()?;
    let created_job = state.jobs.create_cancellable()?;
    let job_id = created_job.id;
    let cancellation = created_job.cancellation;

    let worker_state = state.clone();
    let worker_job_id = job_id.clone();
    let monitor_state = state;
    let monitor_job_id = job_id.clone();
    let worker = tokio::spawn(async move {
        let _operation_permit = operation_permit;
        if let Err(error) = worker_state.jobs.update(&worker_job_id, 22, "Starting job") {
            tracing::error!(job_id = worker_job_id, %error, "job state could not be updated");
            return;
        }
        let progress_state = worker_state.clone();
        let progress_job_id = worker_job_id.clone();
        let progress_cancellation = cancellation.clone();
        let progress: ProgressCallback = Arc::new(move |percent, stage| {
            if progress_cancellation.is_cancelled() {
                return true;
            }
            if let Err(error) = progress_state.jobs.update(&progress_job_id, percent, stage) {
                tracing::error!(job_id = progress_job_id, %error, "job progress could not be stored");
                return true;
            }
            false
        });

        let result = match action {
            JobAction::Convert => {
                convert::convert_form(
                    worker_state.clone(),
                    form,
                    _operation_permit.clone(),
                    Some(progress),
                )
                .await
            }
            JobAction::Merge => {
                pdf_ops::merge_form(
                    worker_state.clone(),
                    form,
                    _operation_permit.clone(),
                    Some(progress),
                )
                .await
            }
            JobAction::Split => {
                pdf_ops::split_form(
                    worker_state.clone(),
                    form,
                    _operation_permit.clone(),
                    Some(progress),
                )
                .await
            }
            JobAction::GangUpAnalyze => {
                gang_up::analyze_form(
                    worker_state.clone(),
                    form,
                    _operation_permit.clone(),
                    Some(progress),
                )
                .await
            }
            JobAction::GangUpPrepare => {
                gang_up::prepare_source_form(
                    worker_state.clone(),
                    form,
                    _operation_permit.clone(),
                    Some(progress),
                )
                .await
            }
            JobAction::GangUpExport => gang_up::export_pdf_form(
                worker_state.clone(),
                form,
                _operation_permit.clone(),
                Some(progress),
            )
            .await
            .map(|download| download.file),
            JobAction::GangUpExportSource => gang_up::export_pdf_source_form(
                worker_state.clone(),
                form,
                _operation_permit.clone(),
                Some(progress),
            )
            .await
            .map(|download| download.file),
        };

        if cancellation.is_cancelled() {
            if action.is_prepare() {
                if let Ok(file) = &result {
                    if let Ok(response) =
                        serde_json::from_slice::<serde_json::Value>(file.bytes.as_ref())
                    {
                        if let Some(source_id) = response.get("sourceId").and_then(|id| id.as_str())
                        {
                            if let Err(error) =
                                worker_state.source_session_store().delete(source_id)
                            {
                                tracing::warn!(%error, "failed to clean cancelled source session");
                            }
                        }
                    }
                }
            }
            if let Err(error) = worker_state.jobs.cancel(&worker_job_id) {
                tracing::error!(job_id = worker_job_id, %error, "cancelled job state could not be stored");
            }
            return;
        }
        match result {
            Ok(file) => {
                if let Err(error) = worker_state.jobs.complete(&worker_job_id, file) {
                    tracing::error!(job_id = worker_job_id, %error, "completed job state could not be stored");
                }
            }
            Err(error) => {
                if matches!(
                    error,
                    AppError::ExternalTool(_)
                        | AppError::ExternalToolCause { .. }
                        | AppError::Internal(_)
                        | AppError::InternalCause { .. }
                ) {
                    tracing::error!(
                        job_id = worker_job_id,
                        action = %action,
                        error = %error,
                        "background job failed"
                    );
                } else {
                    tracing::warn!(
                        job_id = worker_job_id,
                        action = %action,
                        error = %error,
                        "background job rejected"
                    );
                }
                if let Err(store_error) = worker_state
                    .jobs
                    .fail(&worker_job_id, error.client_message())
                {
                    tracing::error!(
                        job_id = worker_job_id,
                        error = %store_error,
                        "failed job state could not be stored"
                    );
                }
            }
        }
    });
    tokio::spawn(async move {
        if let Err(error) = worker.await {
            if error.is_cancelled() {
                tracing::warn!(job_id = monitor_job_id, "background job task was cancelled");
                return;
            }
            tracing::error!(
                job_id = monitor_job_id,
                %error,
                "background job task terminated unexpectedly"
            );
            if let Err(store_error) = monitor_state
                .jobs
                .fail(&monitor_job_id, "internal server error")
            {
                tracing::error!(
                    job_id = monitor_job_id,
                    error = %store_error,
                    "unexpected job failure could not be stored"
                );
            }
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        job_id,
    )
        .into_response())
}

pub(crate) async fn cancel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    if !state.jobs.cancel(&id)? {
        return Err(AppError::bad_request("job not found"));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(crate) async fn status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let snapshot = state
        .jobs
        .snapshot(&id)?
        .ok_or_else(|| AppError::bad_request("job not found"))?;

    let mut lines = vec![
        format!("status={}", snapshot.status()),
        format!("percent={}", snapshot.percent()),
        format!("stage={}", line_value(snapshot.stage())),
    ];

    if let Some(filename) = snapshot.filename() {
        lines.push(format!("filename={}", line_value(filename)));
    }
    if let Some(content_type) = snapshot.content_type() {
        lines.push(format!("content_type={}", line_value(content_type)));
    }
    if let Some(error) = snapshot.error() {
        lines.push(format!("error={}", line_value(error)));
    }

    Ok((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        lines.join("\n"),
    )
        .into_response())
}

pub(crate) async fn download(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let snapshot = state
        .jobs
        .snapshot(&id)?
        .ok_or_else(|| AppError::bad_request("job not found"))?;

    if snapshot.status() != crate::jobs::JobStatus::Done {
        return Ok((StatusCode::CONFLICT, "job is not ready").into_response());
    }

    state
        .jobs
        .download(&id)?
        .map(|file| file.into_response())
        .ok_or_else(|| AppError::bad_request("job result is missing"))
}

fn line_value(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{JobStatus, JobStore};
    use tokio::sync::Semaphore;

    async fn assert_unexpected_worker_exit_is_terminal(worker_panics: bool) {
        let jobs = JobStore::new(Some(1));
        let created = jobs.create_cancellable().unwrap();
        let job_id = created.id;
        let staging_path = std::env::temp_dir().join(format!(
            "pdf-tools-worker-exit-{}-{worker_panics}.upload",
            std::process::id()
        ));
        std::fs::write(&staging_path, b"staged").unwrap();
        let staged =
            crate::adapters::StagedImposeUpload::for_test("source.pdf", staging_path.clone());
        let admission = Arc::new(Semaphore::new(1));
        let permit = Arc::new(admission.clone().try_acquire_owned().unwrap());
        let worker = tokio::spawn(async move {
            let _staged = staged;
            let _permit = permit;
            if worker_panics {
                panic!("injected staged preparation panic");
            }
            std::future::pending::<()>().await;
        });
        let abort = worker.abort_handle();
        let monitor = spawn_staged_prepare_monitor(jobs.clone(), job_id.clone(), worker);
        if !worker_panics {
            abort.abort();
        }
        monitor.await.unwrap();

        let snapshot = jobs.snapshot(&job_id).unwrap().unwrap();
        assert_eq!(snapshot.status(), JobStatus::Error);
        assert_eq!(snapshot.error(), Some("internal server error"));
        assert!(
            jobs.create_cancellable().is_ok(),
            "job-store admission was released"
        );
        assert!(!staging_path.exists(), "staged upload was removed");
        assert_eq!(
            admission.available_permits(),
            1,
            "operation permit was released"
        );
    }

    #[tokio::test]
    async fn staged_prepare_panic_becomes_terminal_error() {
        assert_unexpected_worker_exit_is_terminal(true).await;
    }

    #[tokio::test]
    async fn staged_prepare_abort_becomes_terminal_error() {
        assert_unexpected_worker_exit_is_terminal(false).await;
    }

    #[tokio::test]
    async fn cancellation_after_source_creation_removes_session_and_releases_admission() {
        let Some(pdfium) = crate::test_pdfium() else {
            return;
        };
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir = std::env::temp_dir().join(format!(
            "pdf-tools-cancelled-prepared-source-{}-{unique}",
            std::process::id(),
        ));
        let state =
            Arc::new(AppState::for_tests_with_data_dir(pdfium.shared(), data_dir.clone()).unwrap());
        let operation_permit = state.admit_operation().unwrap();
        let source_id = state
            .source_session_store()
            .create(&crate::adapters::UploadFile {
                filename: "source.pdf".to_string(),
                bytes: bytes::Bytes::from_static(b"%PDF-test"),
            })
            .unwrap();
        let created = state.jobs.create_cancellable().unwrap();
        state.jobs.cancel(&created.id).unwrap();
        let result = Ok(FileDownload::new(
            "application/json",
            "prepared.json",
            serde_json::to_vec(&serde_json::json!({ "sourceId": source_id })).unwrap(),
        ));

        finish_prepare_job(&state, &created.id, &created.cancellation, result);
        drop(operation_permit);

        let snapshot = state.jobs.snapshot(&created.id).unwrap().unwrap();
        assert_eq!(snapshot.status(), JobStatus::Error);
        assert_eq!(snapshot.error(), Some("operation was cancelled"));
        assert_eq!(
            std::fs::read_dir(data_dir.join("source-sessions"))
                .unwrap()
                .count(),
            0,
            "created source session was removed"
        );
        assert!(
            state.admit_operation().is_ok(),
            "operation admission was released"
        );
        std::fs::remove_dir_all(data_dir).unwrap();
    }
}
