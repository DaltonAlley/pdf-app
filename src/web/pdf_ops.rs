use std::sync::Arc;

use axum::{
    extract::{Multipart, State},
    response::Response,
    Json,
};

use super::state::OperationPermit;
use crate::{
    adapters::{read_multipart, require_at_least_pdfs, require_one_pdf, FileDownload, FormData},
    documents as pdf_merge, documents as pdf_split,
    documents::{output_filename, OutputKind, PageSelection},
    error::{AppError, AppResult},
    progress::{report, ProgressCallback},
    AppState,
};

/// General PDF intake deliberately does not apply imposition geometry rules.
pub(crate) async fn inspect(
    State(state): State<Arc<AppState>>,
    multipart: Multipart,
) -> AppResult<Json<serde_json::Value>> {
    let form = read_multipart(multipart).await?;
    let file = require_one_pdf(form, "Choose one readable PDF to inspect.")?;
    let operation_permit = state.admit_operation()?;
    let pdfium = state.pdfium();
    let count = state
        .run_pdfium(operation_permit, move || {
            let document = crate::documents::load_pdf(&pdfium, file.bytes)?;
            let count = crate::documents::page_count(document.pages().len())?;
            if count == 0 {
                return Err(AppError::bad_request("The PDF contains no pages."));
            }
            // Opening each page catches unreadable page-tree entries without
            // requiring pages to share a size, orientation, trim, or bleed.
            for index in document.pages().as_range() {
                document.pages().get(index).map_err(|error| {
                    AppError::bad_request_cause("The PDF contains an unreadable page.", error)
                })?;
            }
            Ok(count)
        })
        .await?;
    Ok(Json(serde_json::json!({ "pageCount": count })))
}

pub(crate) async fn merge(
    State(state): State<Arc<AppState>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let form = read_multipart(multipart).await?;
    let operation_permit = state.admit_operation()?;
    Ok(merge_form(state, form, operation_permit, None)
        .await?
        .into_response())
}

pub(crate) async fn split(
    State(state): State<Arc<AppState>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let form = read_multipart(multipart).await?;
    let operation_permit = state.admit_operation()?;
    Ok(split_form(state, form, operation_permit, None)
        .await?
        .into_response())
}

pub(crate) async fn merge_form(
    state: Arc<AppState>,
    form: FormData,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
) -> AppResult<FileDownload> {
    let max_download_bytes = state.max_download_bytes();
    require_at_least_pdfs(&form.files, 2, "merge requires at least two PDF files")?;
    let source_filename = form
        .files
        .first()
        .map(|file| file.filename.clone())
        .ok_or_else(|| AppError::bad_request("merge requires at least two PDF files"))?;

    report(
        &progress,
        25,
        format!("Preparing {} PDFs", form.files.len()),
    )?;
    let files = form.files;
    let pdfium = state.pdfium();
    let progress_for_cpu = progress.clone();
    let bytes = state
        .run_pdfium(operation_permit, move || {
            pdf_merge::merge_pdfs(&pdfium, files, max_download_bytes, progress_for_cpu)
        })
        .await?;

    report(&progress, 95, "Preparing download")?;
    pdf_response(
        &output_filename(&source_filename, OutputKind::CombinedPdf),
        bytes,
        max_download_bytes,
    )
}

pub(crate) async fn split_form(
    state: Arc<AppState>,
    form: FormData,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
) -> AppResult<FileDownload> {
    let pages = PageSelection::parse(form.required_field("pages")?)?;
    let output_mode = pdf_split::ExtractOutputMode::from_form(&form)?;
    let file = require_one_pdf(form, "split requires a PDF input")?;
    let output_kind = match output_mode {
        pdf_split::ExtractOutputMode::Individual => OutputKind::ExtractedPages,
        pdf_split::ExtractOutputMode::Combined => OutputKind::ExtractedCombinedPdf,
        pdf_split::ExtractOutputMode::Chunks { .. } => OutputKind::ExtractedChunks,
    };
    let output_filename = output_filename(&file.filename, output_kind);
    let content_type = if output_mode.is_combined() {
        "application/pdf"
    } else {
        "application/zip"
    };

    report(&progress, 30, "Reading PDF")?;
    let pdfium = state.pdfium();
    let max_pages = state.max_render_pages();
    let max_download_bytes = state.max_download_bytes();
    let progress_for_cpu = progress.clone();
    let bytes = state
        .run_pdfium(operation_permit, move || {
            pdf_split::extract_pdf_pages(
                &pdfium,
                file,
                pages,
                max_pages,
                output_mode,
                progress_for_cpu,
                max_download_bytes,
            )
        })
        .await?;

    report(&progress, 95, "Preparing download")?;
    FileDownload::bounded(content_type, output_filename, bytes, max_download_bytes)
}

fn pdf_response(
    filename: &str,
    bytes: Vec<u8>,
    max_download_bytes: Option<usize>,
) -> AppResult<FileDownload> {
    FileDownload::bounded("application/pdf", filename, bytes, max_download_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfium_render::prelude::{PdfPagePaperSize, Pdfium};

    fn sample_pdf_bytes(pdfium: &Pdfium, page_count: usize) -> Vec<u8> {
        let mut document = pdfium.create_new_pdf().unwrap();
        for _ in 0..page_count {
            document
                .pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .unwrap();
        }
        document.save_to_bytes().unwrap()
    }

    #[tokio::test]
    async fn split_form_returns_mode_specific_download_contracts() {
        let Some(pdfium) = crate::test_pdfium() else {
            return;
        };
        let source = sample_pdf_bytes(&pdfium, 3);
        let state = Arc::new(AppState::for_tests(pdfium.shared()).unwrap());

        for (fields, content_type, filename) in [
            (
                vec![
                    ("pages".to_string(), "1-3".to_string()),
                    ("outputMode".to_string(), "combined".to_string()),
                ],
                "application/pdf",
                "source-pages.pdf",
            ),
            (
                vec![
                    ("pages".to_string(), "1-3".to_string()),
                    ("outputMode".to_string(), "chunks".to_string()),
                    ("chunkSize".to_string(), "2".to_string()),
                ],
                "application/zip",
                "source-chunks.zip",
            ),
        ] {
            let form = FormData {
                files: vec![crate::adapters::UploadFile {
                    filename: "source.pdf".to_string(),
                    bytes: source.clone().into(),
                }],
                fields,
            };
            let permit = state.admit_operation().unwrap();
            let download = split_form(state.clone(), form, permit, None).await.unwrap();
            assert_eq!(download.content_type, content_type);
            assert_eq!(download.filename, filename);
            assert!(!download.bytes.is_empty());
        }
    }
}
