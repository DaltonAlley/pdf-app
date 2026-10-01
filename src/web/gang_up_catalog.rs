//! Saved imposition preset, recent-job, and export-history HTTP handlers.

use std::sync::Arc;

use axum::{
    extract::{Json, Path, State},
    http::{header::CACHE_CONTROL, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};

use crate::{
    error::{AppError, AppResult},
    imposition as gang_up_layout, imposition as gang_up_validation,
    imposition::{GangUpExportRecordInput, LayoutRequest, PresetInput, RecentGangUpJobInput},
    AppState,
};

const MAX_NAME_BYTES: usize = 200;

pub(crate) async fn presets(State(state): State<Arc<AppState>>) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    let store = state.preset_store();
    let values = state
        .run_cpu(operation_permit, move || store.list())
        .await?;
    Ok(no_store(Json(values).into_response()))
}

pub(crate) async fn create_preset(
    State(state): State<Arc<AppState>>,
    Json(input): Json<PresetInput>,
) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    gang_up_validation::validate_preset_input(&input)?;
    let store = state.preset_store();
    let value = state
        .run_cpu(operation_permit, move || store.create(input))
        .await?;
    Ok(no_store((StatusCode::CREATED, Json(value)).into_response()))
}

pub(crate) async fn update_preset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(input): Json<PresetInput>,
) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    validate_id(&id, "preset")?;
    gang_up_validation::validate_preset_input(&input)?;
    let store = state.preset_store();
    let value = state
        .run_cpu(operation_permit, move || store.update(&id, input))
        .await?;
    Ok(no_store(Json(value).into_response()))
}

pub(crate) async fn delete_preset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    validate_id(&id, "preset")?;
    let store = state.preset_store();
    state
        .run_cpu(operation_permit, move || store.delete(&id))
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(crate) async fn recent_jobs(State(state): State<Arc<AppState>>) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    let store = state.recent_gang_up_store();
    let values = state
        .run_cpu(operation_permit, move || store.list())
        .await?;
    Ok(no_store(Json(values).into_response()))
}

pub(crate) async fn create_recent_job(
    State(state): State<Arc<AppState>>,
    Json(input): Json<RecentGangUpJobInput>,
) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    validate_optional_name(input.name.as_deref(), "recent job name")?;
    let store = state.recent_gang_up_store();
    let value = state
        .run_cpu(operation_permit, move || {
            validate_saved_request(&input.request)?;
            store.create(input)
        })
        .await?;
    Ok(no_store((StatusCode::CREATED, Json(value)).into_response()))
}

pub(crate) async fn delete_recent_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    validate_id(&id, "recent job")?;
    let store = state.recent_gang_up_store();
    state
        .run_cpu(operation_permit, move || store.delete(&id))
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(crate) async fn export_history(State(state): State<Arc<AppState>>) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    let store = state.gang_up_export_history_store();
    let values = state
        .run_cpu(operation_permit, move || store.list())
        .await?;
    Ok(no_store(Json(values).into_response()))
}

pub(crate) async fn create_export_history_record(
    State(state): State<Arc<AppState>>,
    Json(input): Json<GangUpExportRecordInput>,
) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    validate_optional_name(input.name.as_deref(), "export name")?;
    validate_name(&input.output_filename, "export filename")?;
    let store = state.gang_up_export_history_store();
    let value = state
        .run_cpu(operation_permit, move || {
            validate_saved_request(&input.request)?;
            store.create(input)
        })
        .await?;
    Ok(no_store((StatusCode::CREATED, Json(value)).into_response()))
}

pub(crate) async fn download_export_history_record(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    validate_id(&id, "export history")?;
    let store = state.gang_up_export_history_store();
    let max_download_bytes = state.max_download_bytes();
    let file = state
        .run_cpu(operation_permit, move || {
            store.download(&id, max_download_bytes)
        })
        .await?;
    Ok(file.into_response())
}

pub(crate) async fn delete_export_history_record(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    validate_id(&id, "export history")?;
    let store = state.gang_up_export_history_store();
    state
        .run_cpu(operation_permit, move || store.delete(&id))
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn validate_saved_request(request: &LayoutRequest) -> AppResult<()> {
    gang_up_layout::generate_layout(request.clone()).map(|_| ())
}

fn validate_id(id: &str, label: &str) -> AppResult<()> {
    validate_name(id, label)
}

fn validate_optional_name(value: Option<&str>, label: &str) -> AppResult<()> {
    if let Some(value) = value {
        validate_name(value, label)?;
    }
    Ok(())
}

fn validate_name(value: &str, label: &str) -> AppResult<()> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::bad_request(format!("{label} is required")));
    }
    if value.len() > MAX_NAME_BYTES || value.chars().any(char::is_control) {
        return Err(AppError::bad_request(format!(
            "{label} must be at most {MAX_NAME_BYTES} bytes without control characters"
        )));
    }
    Ok(())
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
