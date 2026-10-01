//! Generic multipart form creation and execution.

use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{FormData, Request, RequestInit, Response};

use crate::{
    browser::{
        execute_job_download, http_response_failure, response_text, save_job_download,
        BrowserCancellation, FetchDeadline, JobProgress, ProgressSink,
    },
    files::{classify_file, FileKind, SelectedFile},
    transport::HttpRequestContext,
};

use super::{
    extract_chunk_size,
    model::{plan_merge_upload, GenericJobOperation, GenericJobSettings},
    ExtractOutputMode,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PdfPreflight {
    page_count: usize,
}

/// Verifies every selected PDF and returns its authoritative page count.
pub(crate) async fn preflight_pdf_files(
    files: &[SelectedFile],
    cancellation: &BrowserCancellation,
) -> Result<Vec<(u64, usize)>, String> {
    let mut counts = Vec::new();
    for selected in files {
        if cancellation.is_cancelled() {
            return Err("PDF inspection was cancelled.".into());
        }
        if classify_file(&selected.descriptor) != FileKind::Pdf {
            continue;
        }
        let count = preflight_pdf(selected, cancellation).await?;
        counts.push((selected.id, count));
    }
    Ok(counts)
}

async fn preflight_pdf(
    selected: &SelectedFile,
    cancellation: &BrowserCancellation,
) -> Result<usize, String> {
    let form = FormData::new().map_err(js_error)?;
    form.append_with_blob_and_filename("files", &selected.file, &selected.descriptor.name)
        .map_err(js_error)?;
    let deadline = FetchDeadline::new(Some(cancellation), 120_000)?;
    let init = RequestInit::new();
    init.set_method("POST");
    init.set_body(form.as_ref());
    init.set_signal(Some(&deadline.signal()));
    let request = Request::new_with_str_and_init("/pdf/inspect", &init).map_err(js_error)?;
    let response = JsFuture::from(window()?.fetch_with_request(&request))
        .await
        .map_err(|error| {
            deadline.request_error(
                Some(cancellation),
                "PDF inspection was cancelled.",
                "Checking the selected PDF took longer than 120 seconds. Try the upload again.",
                HttpRequestContext::Upload,
                error,
            )
        })?
        .dyn_into::<Response>()
        .map_err(js_error)?;
    let text = response_text(&response).await?;
    if !response.ok() {
        if matches!(response.status(), 400 | 422) {
            return Err(unreadable_pdf_message(&selected.descriptor.name));
        }
        return Err(http_response_failure(
            &response,
            &text,
            HttpRequestContext::Upload,
        ));
    }
    let inspected: PdfPreflight = serde_json::from_str(&text).map_err(|_| {
        "The PDF service returned an invalid inspection result. Try again.".to_owned()
    })?;
    if inspected.page_count == 0 {
        return Err(unreadable_pdf_message(&selected.descriptor.name));
    }
    Ok(inspected.page_count)
}

fn unreadable_pdf_message(filename: &str) -> String {
    format!("“{filename}” is damaged or is not a readable PDF. Remove or replace it to continue.")
}

fn window() -> Result<web_sys::Window, String> {
    web_sys::window().ok_or_else(|| "The browser window is unavailable.".into())
}

/// Builds a multipart form matching the existing Axum `/jobs` contract.
pub(crate) fn build_job_form(
    operation: GenericJobOperation,
    files: &[SelectedFile],
    settings: &GenericJobSettings,
) -> Result<FormData, String> {
    let form = FormData::new().map_err(js_error)?;
    match operation {
        GenericJobOperation::Merge => {
            let descriptors = files
                .iter()
                .map(|selected| selected.descriptor.clone())
                .collect::<Vec<_>>();
            let upload = plan_merge_upload(&descriptors).map_err(str::to_owned)?;
            append_text(&form, "action", "merge")?;
            for part in upload {
                let Some(selected) = files.get(part.source_index) else {
                    return Err("The selected PDF order changed while preparing the upload.".into());
                };
                form.append_with_blob_and_filename("files", &selected.file, part.filename)
                    .map_err(js_error)?;
            }
        }
        GenericJobOperation::Split => {
            let Some(source) = files.first() else {
                return Err("Choose one PDF to extract pages.".into());
            };
            if files.len() != 1 {
                return Err("Choose one PDF to extract pages.".into());
            }
            settings.extract_pages.validate().map_err(str::to_owned)?;
            append_text(&form, "action", "split")?;
            form.append_with_blob_and_filename("file", &source.file, &source.descriptor.name)
                .map_err(js_error)?;
            append_text(&form, "pages", settings.extract_pages.expression())?;
            append_text(&form, "outputMode", settings.extract_output_mode.as_str())?;
            if settings.extract_output_mode == ExtractOutputMode::Chunks {
                let chunk_size = extract_chunk_size(&settings.extract_chunk_size_draft)
                    .map_err(str::to_owned)?;
                append_text(&form, "chunkSize", &chunk_size.to_string())?;
            }
        }
        GenericJobOperation::PdfImage => {
            if files.is_empty() {
                return Err("Choose one or more PDFs to export pages as images.".into());
            }
            settings.export_pages.validate().map_err(str::to_owned)?;
            append_text(&form, "action", "convert")?;
            append_files(&form, "files", files)?;
            append_text(&form, "target", settings.target.as_str())?;
            append_text(&form, "pages", settings.export_pages.expression())?;
            let (dpi, jpeg_quality) = settings.quality.settings();
            append_text(&form, "dpi", &dpi.to_string())?;
            append_text(&form, "jpegQuality", &jpeg_quality.to_string())?;
        }
        GenericJobOperation::ImagePdf => {
            if files.is_empty() {
                return Err("Choose one or more PNG or JPEG images to create a PDF.".into());
            }
            append_text(&form, "action", "convert")?;
            append_files(&form, "files", files)?;
            append_text(&form, "target", "pdf")?;
            append_text(&form, "layout", "single")?;
            append_text(&form, "pageSize", "original")?;
        }
    }
    Ok(form)
}

/// Runs a background job through upload, polling, and download, then saves it.
pub(crate) async fn execute_job(
    form: FormData,
    progress: ProgressSink,
    cancellation: BrowserCancellation,
) -> Result<String, String> {
    let download = execute_job_download(form, "/jobs", progress.clone(), cancellation).await?;
    let filename = save_job_download(download)?;
    progress(JobProgress {
        percent: 100,
        stage: "Download ready".into(),
        detail: filename.clone(),
    });
    Ok(filename)
}

fn append_text(form: &FormData, name: &str, value: &str) -> Result<(), String> {
    form.append_with_str(name, value).map_err(js_error)
}

fn append_files(form: &FormData, name: &str, files: &[SelectedFile]) -> Result<(), String> {
    for selected in files {
        form.append_with_blob_and_filename(name, &selected.file, &selected.descriptor.name)
            .map_err(js_error)?;
    }
    Ok(())
}

fn js_error(value: wasm_bindgen::JsValue) -> String {
    value
        .as_string()
        .or_else(|| {
            value
                .dyn_ref::<js_sys::Error>()
                .map(|error| error.message().into())
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| "A browser operation failed.".into())
}
