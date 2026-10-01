use std::{collections::VecDeque, sync::Arc};

use axum::{
    extract::{Json, Multipart, Path, RawQuery, State},
    http::{header::CACHE_CONTROL, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use super::state::OperationPermit;
use crate::{
    adapters::{
        is_pdf, read_multipart, require_one_impose_source, stage_impose_multipart,
        validate_download_size, FileDownload, FormData, StagedImposeForm, StagedImposeUpload,
        UploadFile,
    },
    documents as image_pdf, documents as pdf_merge, documents as pdf_render,
    documents::{output_filename, OutputKind},
    error::{AppError, AppResult},
    imposition as gang_up_export, imposition as gang_up_layout, imposition as gang_up_pdf,
    imposition::{
        GangUpExportRecordInput, GangUpExportType, LayoutRequest, LayoutResult, PdfAnalysis,
        RecentGangUpLayoutSummary, SourceSessionFile,
    },
    progress::{mapped, report, ProgressCallback},
    AppState,
};

const MAX_STAGED_IMAGE_BATCH_INPUT_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) async fn analyze(
    State(state): State<Arc<AppState>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let form = read_multipart(multipart).await?;
    let operation_permit = state.admit_operation()?;
    Ok(no_store(
        analyze_form(state, form, operation_permit, None)
            .await?
            .into_response(),
    ))
}

pub(crate) async fn prepare_source(
    State(state): State<Arc<AppState>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let staging_dir = state.impose_staging_dir();
    let staged =
        stage_impose_multipart(multipart, &staging_dir, state.impose_upload_limits()).await?;
    let operation_permit = state.admit_operation()?;
    super::jobs::create_staged_prepare(state, staged, operation_permit)
}

pub(crate) async fn prepare_staged_source(
    state: Arc<AppState>,
    staged: StagedImposeForm,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
) -> AppResult<FileDownload> {
    if staged.files.len() == 1 && staged.files[0].is_pdf()? {
        let file = staged
            .files
            .into_iter()
            .next()
            .ok_or_else(|| AppError::bad_request("impose requires source artwork"))?;
        let artwork = gang_up_pdf::StagedArtworkPdf::uploaded_pdf(file);
        let (artwork, normalization) = state
            .run_cpu(operation_permit.clone(), move || {
                let normalization = gang_up_pdf::normalize_staged_imposition_geometry(
                    std::slice::from_ref(&artwork),
                )?;
                Ok((artwork, normalization))
            })
            .await?;
        return prepare_staged_pdf(
            state,
            artwork.into_upload(),
            operation_permit,
            progress,
            true,
            normalization,
        )
        .await;
    }

    let StagedImposeForm { files, fields } = staged;
    let filename = files
        .first()
        .map(|file| file.filename().to_string())
        .ok_or_else(|| AppError::bad_request("impose requires source artwork"))?;
    let max_download_bytes = state.max_download_bytes();
    let staging_dir = state.impose_staging_dir();
    let image_progress = progress.clone();
    let prepared = state
        .run_cpu(operation_permit.clone(), move || {
            prepare_staged_artwork(files, &staging_dir, max_download_bytes, image_progress)
        })
        .await?;
    let PreparedStagedArtwork {
        pdfs,
        generated_images_only,
        normalization,
    } = prepared;
    if generated_images_only && pdfs.len() == 1 {
        let file = pdfs
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Internal("prepared image PDF disappeared".to_string()))?;
        return prepare_staged_pdf(
            state,
            file,
            operation_permit,
            progress,
            false,
            normalization,
        )
        .await;
    }
    let pdfium = state.pdfium();
    report(&progress, 62, "Validated converted artwork geometry")?;
    let merge_progress = mapped(&progress, 63, 75);
    let bytes = state
        .run_pdfium(operation_permit.clone(), move || {
            pdf_merge::merge_staged_pdfs(&pdfium, &pdfs, max_download_bytes, merge_progress)
        })
        .await?;
    prepare_source_form_with_options(
        state,
        FormData {
            files: vec![UploadFile {
                filename,
                bytes: bytes.into(),
            }],
            fields,
        },
        operation_permit,
        mapped(&progress, 76, 94),
        !generated_images_only,
        normalization,
    )
    .await
}

#[derive(Debug)]
struct PreparedStagedArtwork {
    pdfs: Vec<StagedImposeUpload>,
    generated_images_only: bool,
    normalization: gang_up_pdf::OrientationNormalization,
}

fn prepare_staged_artwork(
    files: Vec<StagedImposeUpload>,
    staging_dir: &std::path::Path,
    max_download_bytes: Option<usize>,
    progress: Option<ProgressCallback>,
) -> AppResult<PreparedStagedArtwork> {
    let mut pending = VecDeque::from(files);
    let generated_images_only = pending.iter().try_fold(true, |only_images, file| {
        Ok::<_, AppError>(only_images && !file.is_pdf()?)
    })?;
    let mut pdfs = Vec::with_capacity(pending.len());
    let max_batch_files = rayon::current_num_threads().max(1);
    let total_images = pending.iter().try_fold(0_usize, |count, file| {
        Ok::<_, AppError>(count.saturating_add(usize::from(!file.is_pdf()?)))
    })?;
    let mut converted_images = 0_usize;

    while let Some(file) = pending.pop_front() {
        if file.is_pdf()? {
            pdfs.push(gang_up_pdf::StagedArtworkPdf::uploaded_pdf(file));
            continue;
        }

        let mut input_bytes = file.byte_len()?;
        let mut image_files = vec![file];
        while image_files.len() < max_batch_files {
            let Some(next) = pending.front() else {
                break;
            };
            if next.is_pdf()? {
                break;
            }
            let next_bytes = next.byte_len()?;
            if input_bytes.saturating_add(next_bytes) > MAX_STAGED_IMAGE_BATCH_INPUT_BYTES {
                break;
            }
            input_bytes = input_bytes.saturating_add(next_bytes);
            let next = pending
                .pop_front()
                .ok_or_else(|| AppError::Internal("staged image queue changed".to_string()))?;
            image_files.push(next);
        }

        let output_name = image_files
            .first()
            .map(|file| file.filename().to_string())
            .ok_or_else(|| AppError::Internal("staged image batch is empty".to_string()))?;
        let page_identities = image_files
            .iter()
            .map(|file| gang_up_pdf::ArtworkPageIdentity::first_page(file.filename().to_string()))
            .collect::<Vec<_>>();
        for file in &image_files {
            let byte_len = usize::try_from(file.byte_len()?).map_err(|_| {
                AppError::payload_too_large(format!(
                    "image `{}` is too large for this server",
                    file.filename()
                ))
            })?;
            image_pdf::validate_image_input_size(file.filename(), byte_len)?;
        }
        let images = image_files
            .into_iter()
            .map(StagedImposeUpload::into_image_upload)
            .collect::<AppResult<Vec<_>>>()?;
        let batch_len = images.len();
        let bytes = image_pdf::images_to_pdf_with_progress(
            images,
            max_download_bytes,
            progress.clone(),
            image_pdf::ImageProgressPlan::new(22, 60, converted_images, total_images),
        )?;
        converted_images = converted_images.saturating_add(batch_len);
        let upload = StagedImposeUpload::from_bytes(staging_dir, output_name, &bytes)?;
        pdfs.push(gang_up_pdf::StagedArtworkPdf::converted_images(
            upload,
            page_identities,
        ));
    }

    report(&progress, 61, "Validating artwork geometry")?;
    let normalization = gang_up_pdf::normalize_staged_imposition_geometry(&pdfs)?;
    Ok(PreparedStagedArtwork {
        pdfs: pdfs
            .into_iter()
            .map(gang_up_pdf::StagedArtworkPdf::into_upload)
            .collect(),
        generated_images_only,
        normalization,
    })
}

async fn prepare_staged_pdf(
    state: Arc<AppState>,
    staged: StagedImposeUpload,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
    flatten_annotations: bool,
    normalization: gang_up_pdf::OrientationNormalization,
) -> AppResult<FileDownload> {
    let filename = staged.filename().to_string();
    let path = staged.path().to_path_buf();
    let preset_store = state.preset_store();
    let presets = state
        .run_cpu(operation_permit.clone(), move || preset_store.list())
        .await?;
    if flatten_annotations {
        let pdfium = state.pdfium();
        let flatten_path = path.clone();
        let flatten_progress = progress.clone();
        state
            .run_pdfium(operation_permit.clone(), move || {
                gang_up_export::flatten_annotations_path(&pdfium, &flatten_path, flatten_progress)
            })
            .await?;
    } else {
        report(&progress, 36, "Image artwork is already prepared")?;
    }
    let source_store = state.source_session_store();
    let work_store = source_store.clone();
    let work_path = path.clone();
    let work_filename = filename.clone();
    let analysis_progress = progress.clone();
    let page_identities = normalization.clone();
    let (source_id, mut analysis) = state
        .run_cpu(operation_permit, move || {
            page_identities.tag_path(&work_path)?;
            let analysis = gang_up_pdf::analyze_pdf_path_structural(
                work_filename.clone(),
                &work_path,
                presets,
                analysis_progress,
            )?;
            let source_id = work_store.create_from_path(&work_filename, &work_path)?;
            Ok((source_id, analysis))
        })
        .await?;
    normalization.append_warning(&mut analysis);
    drop(staged);
    if let Err(error) = report(&progress, 94, "Preparing impose workspace") {
        if let Err(cleanup_error) = source_store.delete(&source_id) {
            tracing::warn!(%cleanup_error, "failed to clean cancelled source session");
        }
        return Err(error);
    }
    let bytes = serde_json::to_vec(&PreparedSourceResponse {
        source_id: source_id.clone(),
        analysis,
    })
    .map_err(|error| AppError::internal_cause("could not encode prepared PDF", error))?;
    match FileDownload::bounded(
        "application/json",
        "gang-up-prepared-source.json",
        bytes,
        state.max_download_bytes(),
    ) {
        Ok(download) => Ok(download),
        Err(error) => {
            let _ = source_store.delete(&source_id);
            Err(error)
        }
    }
}

pub(crate) async fn analyze_form(
    state: Arc<AppState>,
    form: FormData,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
) -> AppResult<FileDownload> {
    report(&progress, 25, "Validating source artwork")?;
    let file = require_one_impose_source(form, "cannot analyze impose source")?;
    let file = prepare_impose_source(
        state.clone(),
        file,
        operation_permit.clone(),
        progress.clone(),
    )
    .await?;
    let preset_store = state.preset_store();
    report(&progress, 30, "Loading impose presets")?;
    let presets = state
        .run_cpu(operation_permit.clone(), move || preset_store.list())
        .await?;
    let pdfium = state.pdfium();
    let analysis_progress = progress.clone();
    let analysis = state
        .run_pdfium(operation_permit, move || {
            gang_up_pdf::analyze_pdf(&pdfium, file, presets, analysis_progress)
        })
        .await?;
    report(&progress, 94, "Preparing impose workspace")?;
    let bytes = serde_json::to_vec(&analysis)
        .map_err(|error| AppError::internal_cause("could not encode PDF analysis", error))?;
    FileDownload::bounded(
        "application/json",
        "gang-up-analysis.json",
        bytes,
        state.max_download_bytes(),
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreparedSourceResponse {
    source_id: String,
    analysis: PdfAnalysis,
}

pub(crate) async fn prepare_source_form(
    state: Arc<AppState>,
    form: FormData,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
) -> AppResult<FileDownload> {
    prepare_source_form_with_options(
        state,
        form,
        operation_permit,
        progress,
        true,
        gang_up_pdf::OrientationNormalization::default(),
    )
    .await
}

async fn prepare_source_form_with_options(
    state: Arc<AppState>,
    form: FormData,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
    flatten_annotations: bool,
    normalization: gang_up_pdf::OrientationNormalization,
) -> AppResult<FileDownload> {
    report(&progress, 25, "Validating source artwork")?;
    let file = require_one_impose_source(form, "cannot prepare impose source")?;
    let file = prepare_impose_source(
        state.clone(),
        file,
        operation_permit.clone(),
        progress.clone(),
    )
    .await?;
    let preset_store = state.preset_store();
    report(&progress, 28, "Loading impose presets")?;
    let presets = state
        .run_cpu(operation_permit.clone(), move || preset_store.list())
        .await?;
    let prepared = if flatten_annotations {
        let pdfium = state.pdfium();
        let flatten_progress = progress.clone();
        state
            .run_pdfium(operation_permit.clone(), move || {
                gang_up_export::flatten_annotations_for_export(&pdfium, file, flatten_progress)
            })
            .await?
    } else {
        report(&progress, 36, "Image artwork is already prepared")?;
        file
    };
    let source_store = state.source_session_store();
    let work_source_store = source_store.clone();
    let preparation_progress = progress.clone();
    let page_identities = normalization.clone();
    let (source_id, mut analysis) = state
        .run_cpu(operation_permit, move || {
            let prepared = page_identities.tag_upload(prepared)?;
            report(&preparation_progress, 32, "Saving source PDF")?;
            let source_id = work_source_store.create(&prepared)?;
            match gang_up_pdf::analyze_pdf_structural(
                prepared,
                presets,
                preparation_progress.clone(),
            ) {
                Ok(analysis) => Ok((source_id, analysis)),
                Err(error) => {
                    if let Err(cleanup_error) = work_source_store.delete(&source_id) {
                        tracing::warn!(%cleanup_error, "failed to clean rejected source session");
                    }
                    Err(error)
                }
            }
        })
        .await?;
    normalization.append_warning(&mut analysis);
    if let Err(error) = report(&progress, 94, "Preparing impose workspace") {
        if let Err(cleanup_error) = source_store.delete(&source_id) {
            tracing::warn!(%cleanup_error, "failed to clean cancelled source session");
        }
        return Err(error);
    }
    let response = (|| {
        let bytes = serde_json::to_vec(&PreparedSourceResponse {
            source_id: source_id.clone(),
            analysis,
        })
        .map_err(|error| AppError::internal_cause("could not encode prepared PDF", error))?;
        FileDownload::bounded(
            "application/json",
            "gang-up-prepared-source.json",
            bytes,
            state.max_download_bytes(),
        )
    })();
    if response.is_err() {
        if let Err(cleanup_error) = source_store.delete(&source_id) {
            tracing::warn!(%cleanup_error, "failed to clean unusable source session");
        }
    }
    response
}

pub(crate) async fn delete_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    state.source_session_store().delete(&id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(crate) async fn renew_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    state.source_session_store().renew(&id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(crate) async fn preview_source_page(
    State(state): State<Arc<AppState>>,
    Path((id, page_number)): Path<(String, usize)>,
    RawQuery(query): RawQuery,
) -> AppResult<Response> {
    let source_bleed_override = preview_source_bleed_query(query.as_deref())?;
    let operation_permit = state.admit_operation()?;
    let source = state.source_session_store().load_path(&id)?;
    let staging_dir = state.impose_staging_dir();
    let max_bytes = state.max_download_bytes();
    let preview = state
        .run_cpu(operation_permit.clone(), move || {
            gang_up_pdf::prepare_artwork_preview(
                &source.path,
                &staging_dir,
                &[page_number],
                source_bleed_override,
                max_bytes,
            )
        })
        .await?;
    let pdfium = state.pdfium();
    let rasters = state
        .run_pdfium(operation_permit.clone(), move || {
            pdf_render::raster_preview_pages(&pdfium, preview.path(), vec![page_number])
        })
        .await?;
    let raster = rasters
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal("preview raster was not produced".to_string()))?;
    let bytes = state
        .run_cpu(operation_permit, move || {
            pdf_render::encode_preview_page(raster)
        })
        .await?;
    let mut response = ([(axum::http::header::CONTENT_TYPE, "image/png")], bytes).into_response();
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=300"),
    );
    Ok(response)
}

fn preview_source_bleed_query(query: Option<&str>) -> AppResult<Option<f64>> {
    fn decode(value: &str) -> AppResult<String> {
        let mut bytes = value.bytes();
        let mut decoded = Vec::with_capacity(value.len());
        while let Some(byte) = bytes.next() {
            decoded.push(match byte {
                b'+' => b' ',
                b'%' => {
                    let high = bytes.next().and_then(|b| (b as char).to_digit(16));
                    let low = bytes.next().and_then(|b| (b as char).to_digit(16));
                    match (high, low) {
                        (Some(high), Some(low)) => (high * 16 + low) as u8,
                        _ => return Err(AppError::bad_request("invalid preview query encoding")),
                    }
                }
                value => value,
            });
        }
        String::from_utf8(decoded)
            .map_err(|_| AppError::bad_request("invalid preview query encoding"))
    }
    let mut amount = None;
    for field in query
        .unwrap_or_default()
        .split('&')
        .filter(|field| !field.is_empty())
    {
        let (key, value) = field.split_once('=').unwrap_or((field, ""));
        if decode(key)? != "sourceBleedOverride" {
            continue;
        }
        if amount.is_some() {
            return Err(AppError::bad_request(
                "duplicate sourceBleedOverride query field",
            ));
        }
        amount = Some(
            decode(value)?
                .parse::<f64>()
                .map_err(|_| AppError::bad_request("invalid source bleed override"))?,
        );
    }
    if amount.is_some_and(|amount| !amount.is_finite() || !(0.001..=1.0).contains(&amount)) {
        return Err(AppError::bad_request(
            "source bleed override must be from 0.001 to 1 inch",
        ));
    }
    Ok(amount)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PreviewBatchRequest {
    page_numbers: Vec<usize>,
    #[serde(default)]
    source_bleed_override: Option<f64>,
}

pub(crate) async fn preview_source_pages(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<PreviewBatchRequest>,
) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    let source = state.source_session_store().load_path(&id)?;
    let staging_dir = state.impose_staging_dir();
    let max_bytes = state.max_download_bytes();
    let numbers = request.page_numbers.clone();
    let preview = state
        .run_cpu(operation_permit.clone(), move || {
            gang_up_pdf::prepare_artwork_preview(
                &source.path,
                &staging_dir,
                &numbers,
                request.source_bleed_override,
                max_bytes,
            )
        })
        .await?;
    let pdfium = state.pdfium();
    let rasters = state
        .run_pdfium(operation_permit.clone(), move || {
            pdf_render::raster_preview_pages(&pdfium, preview.path(), request.page_numbers)
        })
        .await?;
    let bytes = state
        .run_cpu(operation_permit, move || {
            pdf_render::encode_preview_batch(rasters)
        })
        .await?;
    validate_download_size(bytes.len(), state.max_download_bytes())?;
    Ok(no_store(
        (
            [(
                axum::http::header::CONTENT_TYPE,
                "application/vnd.pdf-tools.preview-batch",
            )],
            bytes,
        )
            .into_response(),
    ))
}

pub(crate) async fn layout(
    State(state): State<Arc<AppState>>,
    Json(mut request): Json<LayoutRequest>,
) -> AppResult<Response> {
    let operation_permit = state.admit_operation()?;
    let source = request
        .source_id
        .as_ref()
        .map(|id| state.source_session_store().load_path(id))
        .transpose()?;
    let result = state
        .run_cpu(operation_permit, move || {
            if let Some(source) = source {
                gang_up_pdf::resolve_request_source_path(&source.path, &mut request)?;
            }
            gang_up_layout::generate_layout(request)
        })
        .await?;
    Ok(no_store(Json(result).into_response()))
}

#[derive(Clone, Copy)]
enum ExportWarning {
    HistoryPersistenceFailed,
}

impl ExportWarning {
    const fn header_value(self) -> &'static str {
        match self {
            Self::HistoryPersistenceFailed => "history-persistence-failed",
        }
    }
}

pub(crate) async fn export_pdf(
    State(state): State<Arc<AppState>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let form = read_multipart(multipart).await?;
    let operation_permit = state.admit_operation()?;
    let download = export_pdf_form(state, form, operation_permit, None).await?;
    response_with_export_ids(
        download.file,
        download.export_id.as_deref(),
        download.cut_plan_export_id.as_deref(),
        download.warning,
    )
}

pub(crate) struct GangUpExportDownload {
    pub(crate) file: FileDownload,
    export_id: Option<String>,
    cut_plan_export_id: Option<String>,
    warning: Option<ExportWarning>,
}

pub(crate) async fn export_pdf_form(
    state: Arc<AppState>,
    form: FormData,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
) -> AppResult<GangUpExportDownload> {
    report(&progress, 25, "Validating impose settings")?;
    let request_json = form.required_field("layoutRequest")?;
    let request: LayoutRequest = serde_json::from_str(request_json)
        .map_err(|_| AppError::bad_request("layoutRequest is not valid JSON"))?;
    let file = require_one_impose_source(form, "cannot export impose source")?;
    let file = prepare_impose_source(
        state.clone(),
        file,
        operation_permit.clone(),
        progress.clone(),
    )
    .await?;
    export_pdf_file(
        state,
        ExportPdfSource::Memory(file),
        request,
        operation_permit,
        progress,
        false,
    )
    .await
}

async fn prepare_impose_source(
    state: Arc<AppState>,
    file: UploadFile,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
) -> AppResult<UploadFile> {
    if is_pdf(&file.bytes) {
        return Ok(file);
    }

    let filename = file.filename.clone();
    let max_download_bytes = state.max_download_bytes();
    report(&progress, 27, "Converting artwork for imposition")?;
    let prepared = state
        .run_cpu(operation_permit, move || {
            let bytes = image_pdf::images_to_pdf(vec![file], max_download_bytes, progress)?;
            gang_up_pdf::OrientationNormalization::image(filename.clone()).tag_upload(UploadFile {
                filename,
                bytes: bytes.into(),
            })
        })
        .await?;
    Ok(prepared)
}

pub(crate) async fn export_pdf_source_form(
    state: Arc<AppState>,
    form: FormData,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
) -> AppResult<GangUpExportDownload> {
    report(&progress, 25, "Loading prepared source PDF")?;
    let request: LayoutRequest = serde_json::from_str(form.required_field("layoutRequest")?)
        .map_err(|_| AppError::bad_request("layoutRequest is not valid JSON"))?;
    let source_id = form.required_field("sourceId")?.to_string();
    let source_store = state.source_session_store();
    let file = state
        .run_cpu(operation_permit.clone(), move || {
            source_store.load_path(&source_id)
        })
        .await?;
    export_pdf_file(
        state,
        ExportPdfSource::Path(file),
        request,
        operation_permit,
        progress,
        true,
    )
    .await
}

enum ExportPdfSource {
    Memory(UploadFile),
    Path(SourceSessionFile),
}

impl ExportPdfSource {
    fn filename(&self) -> &str {
        match self {
            Self::Memory(file) => &file.filename,
            Self::Path(file) => &file.filename,
        }
    }
}

async fn export_pdf_file(
    state: Arc<AppState>,
    file: ExportPdfSource,
    request: LayoutRequest,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
    source_prepared: bool,
) -> AppResult<GangUpExportDownload> {
    let source_filename = file.filename().to_string();
    let imposed_filename = output_filename(&source_filename, OutputKind::ImposedPdf);
    let file = if source_prepared {
        report(&progress, 38, "Using prepared source artwork")?;
        file
    } else {
        let pdfium = state.pdfium();
        let flatten_progress = progress.clone();
        let ExportPdfSource::Memory(file) = file else {
            return Err(AppError::Internal(
                "unprepared source path is invalid".to_string(),
            ));
        };
        ExportPdfSource::Memory(
            state
                .run_pdfium(operation_permit.clone(), move || {
                    gang_up_export::flatten_annotations_for_export(&pdfium, file, flatten_progress)
                })
                .await?,
        )
    };
    let history_store = state.gang_up_export_history_store();
    let max_download_bytes = state.max_download_bytes();
    let export_progress = progress.clone();

    let download = state
        .run_cpu(operation_permit, move || {
            let artifact = match file {
                ExportPdfSource::Memory(file) => {
                    gang_up_export::export_clean_imposed_pdf(file, request, export_progress.clone())
                }
                ExportPdfSource::Path(file) => gang_up_export::export_clean_imposed_pdf_path(
                    &file.path,
                    request,
                    export_progress.clone(),
                ),
            }?;
            validate_download_size(artifact.bytes.len(), max_download_bytes)?;
            report(&export_progress, 92, "Saving export history")?;
            let pdf_input = generated_export_record_input(
                &source_filename,
                &imposed_filename,
                GangUpExportType::CleanPdf,
                artifact.canonical_request.clone(),
                &artifact.layout,
            );
            let history = history_store.create_generated(pdf_input, &artifact.bytes);
            let (record, warning) = match history {
                Ok(record) => (Some(record), None),
                Err(error) => {
                    tracing::warn!(%error, "imposed PDF downloaded without saving export history");
                    (None, Some(ExportWarning::HistoryPersistenceFailed))
                }
            };
            let file = FileDownload::new("application/pdf", imposed_filename, artifact.bytes);
            Ok((file, record.map(|record| record.id), warning))
        })
        .await?;

    report(&progress, 96, "Preparing imposed PDF download")?;
    Ok(GangUpExportDownload {
        file: download.0,
        export_id: download.1,
        cut_plan_export_id: None,
        warning: download.2,
    })
}

fn generated_export_record_input(
    source_filename: &str,
    output_filename: &str,
    output_type: GangUpExportType,
    request: LayoutRequest,
    layout: &LayoutResult,
) -> GangUpExportRecordInput {
    let output_name = "Clean PDF";
    GangUpExportRecordInput {
        name: Some(format!("{source_filename} - {output_name}")),
        source_filename: Some(source_filename.to_string()),
        output_type,
        output_filename: output_filename.to_string(),
        stored_file: None,
        request,
        layout_summary: Some(RecentGangUpLayoutSummary {
            pieces_per_sheet: layout.pieces_per_sheet,
            sheets_required: layout.sheets_required,
            total_pieces_produced: layout.total_pieces_produced,
            extra_pieces_produced: layout.extra_pieces_produced,
        }),
    }
}

fn response_with_export_ids(
    file: FileDownload,
    id: Option<&str>,
    cut_plan_id: Option<&str>,
    warning: Option<ExportWarning>,
) -> AppResult<Response> {
    let mut response = file.into_response();
    if let Some(id) = id {
        response.headers_mut().insert(
            HeaderName::from_static("x-gang-up-export-id"),
            HeaderValue::from_str(id)
                .map_err(|_| AppError::Internal("could not encode export record id".to_string()))?,
        );
    }
    if let Some(cut_plan_id) = cut_plan_id {
        response.headers_mut().insert(
            HeaderName::from_static("x-gang-up-cut-plan-export-id"),
            HeaderValue::from_str(cut_plan_id).map_err(|_| {
                AppError::Internal("could not encode cut-plan export record id".to_string())
            })?,
        );
    }
    if let Some(warning) = warning {
        response.headers_mut().insert(
            HeaderName::from_static("x-gang-up-export-warning"),
            HeaderValue::from_static(warning.header_value()),
        );
    }
    Ok(response)
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_query_preserves_defaults_and_decodes_one_optional_amount() {
        assert_eq!(preview_source_bleed_query(None).unwrap(), None);
        assert_eq!(
            preview_source_bleed_query(Some("other=value")).unwrap(),
            None
        );
        assert_eq!(
            preview_source_bleed_query(Some("sourceBleedOverride=0.125")).unwrap(),
            Some(0.125)
        );
        assert_eq!(
            preview_source_bleed_query(Some("sourceBleedOverride=%30%2e125")).unwrap(),
            Some(0.125)
        );
        assert_eq!(
            preview_source_bleed_query(Some("sourceBleedOverride=%2b0.125")).unwrap(),
            Some(0.125)
        );
        for query in [
            "sourceBleedOverride=",
            "sourceBleedOverride=%",
            "sourceBleedOverride=%GG",
            "sourceBleedOverride=%ff",
            "sourceBleedOverride=+0.125",
            "sourceBleedOverride=0.125&sourceBleedOverride=0.25",
            "sourceBleedOverride=0.125&source%42leedOverride=0.25",
        ] {
            assert!(
                preview_source_bleed_query(Some(query)).is_err(),
                "accepted {query}"
            );
        }
        for value in [
            "NaN", "inf", "Infinity", "-0.125", "%2d0.125", "0", "1.01", "1e-4",
        ] {
            assert!(
                preview_source_bleed_query(Some(&format!("sourceBleedOverride={value}"))).is_err(),
                "accepted {value}"
            );
        }
    }
    use pdfium_render::prelude::PdfPagePaperSize;

    fn staged_png(
        staging_dir: &std::path::Path,
        filename: &str,
        width: u32,
        height: u32,
    ) -> StagedImposeUpload {
        use image::ImageEncoder as _;

        let pixels = image::ImageBuffer::from_pixel(width, height, image::Rgb([30, 90, 150]));
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(
                pixels.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        StagedImposeUpload::from_bytes(staging_dir, filename.to_string(), &bytes).unwrap()
    }

    #[test]
    fn one_image_is_prepared_as_a_direct_source_without_pdfium_merge() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let staging_dir = std::env::temp_dir().join(format!(
            "pdf-tools-direct-image-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&staging_dir).unwrap();
        let files = vec![staged_png(&staging_dir, "artwork.png", 600, 300)];

        let prepared = prepare_staged_artwork(files, &staging_dir, None, None).unwrap();

        assert!(prepared.generated_images_only);
        assert_eq!(prepared.pdfs.len(), 1);
        drop(prepared);
        std::fs::remove_dir(staging_dir).unwrap();
    }

    #[test]
    fn mixed_image_geometry_retains_physical_size_assumptions_and_identity() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let staging_dir = std::env::temp_dir().join(format!(
            "pdf-tools-image-geometry-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&staging_dir).unwrap();
        let files = vec![
            staged_png(&staging_dir, "front.png", 300, 600),
            staged_png(&staging_dir, "back.png", 600, 600),
        ];

        let prepared = prepare_staged_artwork(files, &staging_dir, None, None).unwrap();
        assert_eq!(prepared.pdfs.len(), 1);
        let path = prepared.pdfs[0].path();
        prepared.normalization.tag_path(path).unwrap();
        let analysis =
            gang_up_pdf::analyze_pdf_path_structural("images.pdf".into(), path, Vec::new(), None)
                .unwrap();
        assert_eq!(analysis.source_pages.len(), 2);
        assert!(analysis
            .source_pages
            .iter()
            .all(|p| p.physical_size_assumed));
        assert_eq!(
            analysis.source_pages[0].filename.as_deref(),
            Some("front.png")
        );
        assert_eq!(
            analysis.source_pages[1].filename.as_deref(),
            Some("back.png")
        );
        assert_eq!(analysis.source_pages[0].source_pdf_size.width, 1.0);
        assert_eq!(analysis.source_pages[1].source_pdf_size.width, 2.0);
        drop(prepared);
        std::fs::remove_dir(staging_dir).unwrap();
    }

    #[test]
    fn history_failure_warning_is_exposed_without_export_ids() {
        let response = response_with_export_ids(
            FileDownload::new("application/pdf", "export.pdf", b"%PDF-test".to_vec()),
            None,
            None,
            Some(ExportWarning::HistoryPersistenceFailed),
        )
        .unwrap();

        assert_eq!(
            response.headers().get("x-gang-up-export-warning").unwrap(),
            "history-persistence-failed"
        );
        assert!(!response.headers().contains_key("x-gang-up-export-id"));
        assert!(!response
            .headers()
            .contains_key("x-gang-up-cut-plan-export-id"));
    }

    #[tokio::test]
    async fn prepare_source_rolls_back_when_response_exceeds_download_limit() {
        let Some(pdfium) = crate::test_pdfium() else {
            return;
        };
        let mut document = pdfium.create_new_pdf().unwrap();
        document
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a4())
            .unwrap();
        let bytes = document.save_to_bytes().unwrap();
        drop(document);

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir = std::env::temp_dir().join(format!(
            "pdf-tools-prepare-rollback-{}-{unique}",
            std::process::id()
        ));
        let state = Arc::new(
            AppState::for_tests_with_data_dir_and_download_limit(
                pdfium.shared(),
                data_dir.clone(),
                Some(1),
            )
            .unwrap(),
        );
        let form = FormData {
            files: vec![UploadFile {
                filename: "source.pdf".to_string(),
                bytes: bytes.into(),
            }],
            fields: Vec::new(),
        };

        let error =
            match prepare_source_form(state.clone(), form, state.admit_operation().unwrap(), None)
                .await
            {
                Ok(_) => panic!("response limit should reject the prepared source response"),
                Err(error) => error,
            };

        assert!(matches!(error, AppError::PayloadTooLarge(_)));
        let session_dir = data_dir.join("source-sessions");
        assert_eq!(std::fs::read_dir(session_dir).unwrap().count(), 0);
        std::fs::remove_dir_all(data_dir).unwrap();
    }
}
