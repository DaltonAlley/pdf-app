use pdfium_render::prelude::Pdfium;

use super::pdf_io;
use crate::{
    adapters::{StagedImposeUpload, UploadFile},
    error::{AppError, AppResult},
    progress::{report, ProgressCallback},
    MAX_SOURCE_PDF_PAGES,
};

pub(crate) fn merge_pdfs(
    pdfium: &Pdfium,
    files: Vec<UploadFile>,
    max_download_bytes: Option<usize>,
    progress: Option<ProgressCallback>,
) -> AppResult<Vec<u8>> {
    if files.is_empty() {
        return Err(AppError::bad_request("no documents to merge"));
    }

    let file_count = files.len();
    let mut merged = pdf_io::create_pdf(pdfium, "merged")?;
    let mut total_pages = 0usize;

    for (index, file) in files.into_iter().enumerate() {
        let document = pdf_io::load_pdf(pdfium, file.bytes).map_err(|error| {
            AppError::bad_request_cause(format!("could not read `{}`", file.filename), error)
        })?;
        append_document(&mut merged, &document, &file.filename, &mut total_pages)?;

        let completed = index + 1;
        report(
            &progress,
            30 + ((completed * 50) / file_count) as u8,
            format!("Merged {completed} of {file_count} PDFs"),
        )?;
    }

    report(&progress, 90, "Writing merged PDF")?;
    pdf_io::save_pdf_bounded(&merged, "merged", max_download_bytes)
}

/// Merges staged PDFs in order while loading only the current input document.
/// The staging owners remain alive for the call and clean their files on drop.
pub(crate) fn merge_staged_pdfs(
    pdfium: &Pdfium,
    files: &[StagedImposeUpload],
    max_download_bytes: Option<usize>,
    progress: Option<ProgressCallback>,
) -> AppResult<Vec<u8>> {
    if files.is_empty() {
        return Err(AppError::bad_request("no documents to merge"));
    }

    let mut merged = pdf_io::create_pdf(pdfium, "merged")?;
    let mut total_pages = 0usize;

    for (index, file) in files.iter().enumerate() {
        let document = pdf_io::load_pdf_path(pdfium, file.path()).map_err(|error| {
            AppError::bad_request_cause(format!("could not read `{}`", file.filename()), error)
        })?;
        append_document(&mut merged, &document, file.filename(), &mut total_pages)?;

        let completed = index + 1;
        report(
            &progress,
            30 + ((completed * 50) / files.len()) as u8,
            format!("Merged {completed} of {} PDFs", files.len()),
        )?;
    }

    report(&progress, 90, "Writing merged PDF")?;
    pdf_io::save_pdf_bounded(&merged, "merged", max_download_bytes)
}

fn append_document(
    merged: &mut pdfium_render::prelude::PdfDocument<'_>,
    document: &pdfium_render::prelude::PdfDocument<'_>,
    filename: &str,
    total_pages: &mut usize,
) -> AppResult<()> {
    if document.pages().is_empty() {
        return Err(AppError::bad_request("cannot merge a PDF with no pages"));
    }
    *total_pages = total_pages
        .checked_add(document.pages().len() as usize)
        .ok_or_else(|| AppError::payload_too_large("source PDFs contain too many pages"))?;
    if *total_pages > MAX_SOURCE_PDF_PAGES {
        return Err(AppError::payload_too_large(format!(
            "source PDFs are limited to {MAX_SOURCE_PDF_PAGES} total pages"
        )));
    }
    merged
        .pages_mut()
        .append(document)
        .map_err(|error| AppError::internal_cause(format!("could not append `{filename}`"), error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfium_render::prelude::{PdfPagePaperSize, PdfPoints, Pdfium};

    fn pdfium_or_skip() -> Option<crate::adapters::TestPdfium> {
        let pdfium = crate::test_pdfium();
        if pdfium.is_none() {
            eprintln!("skipping PDFium test: PDF_TOOLS_PDFIUM_PATH is not set");
        }
        pdfium
    }

    fn sample_pdf_bytes(pdfium: &Pdfium, page_count: usize) -> Vec<u8> {
        let mut doc = pdfium.create_new_pdf().unwrap();
        for _ in 0..page_count {
            doc.pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .unwrap();
        }
        doc.save_to_bytes().unwrap()
    }

    #[test]
    fn merge_documents_preserves_page_count_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let bytes = merge_pdfs(
            &pdfium,
            vec![
                UploadFile {
                    filename: "one.pdf".to_string(),
                    bytes: sample_pdf_bytes(&pdfium, 1).into(),
                },
                UploadFile {
                    filename: "two.pdf".to_string(),
                    bytes: sample_pdf_bytes(&pdfium, 1).into(),
                },
            ],
            None,
            None,
        )
        .unwrap();
        let merged = pdfium.load_pdf_from_byte_vec(bytes, None).unwrap();
        assert_eq!(merged.pages().len(), 2);
    }

    #[test]
    fn merge_rejects_empty_pdf_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };

        let empty = pdfium.create_new_pdf().unwrap().save_to_bytes().unwrap();
        let error = merge_pdfs(
            &pdfium,
            vec![UploadFile {
                filename: "empty.pdf".to_string(),
                bytes: empty.into(),
            }],
            None,
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("no pages"));
    }

    #[test]
    fn merge_enforces_download_limit_during_serialization() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let error = merge_pdfs(
            &pdfium,
            vec![UploadFile {
                filename: "one.pdf".to_string(),
                bytes: sample_pdf_bytes(&pdfium, 1).into(),
            }],
            Some(1),
            None,
        )
        .unwrap_err();

        assert!(matches!(error, AppError::PayloadTooLarge(_)));
    }

    #[test]
    fn staged_merge_reads_paths_in_order_and_owners_clean_up() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let paths = ["first", "second"].map(|name| {
            std::env::temp_dir().join(format!(
                "pdf-tools-merge-{name}-{}-{unique}.pdf",
                std::process::id()
            ))
        });
        let widths = [PdfPoints::new(200.0), PdfPoints::new(400.0)];
        let files = paths
            .iter()
            .zip(widths)
            .map(|(path, width)| {
                let mut document = pdfium.create_new_pdf().unwrap();
                document
                    .pages_mut()
                    .create_page_at_end(PdfPagePaperSize::from_points(width, PdfPoints::new(300.0)))
                    .unwrap();
                document.save_to_file(path).unwrap();
                StagedImposeUpload::for_test("artwork.pdf", path.clone())
            })
            .collect::<Vec<_>>();

        let bytes = merge_staged_pdfs(&pdfium, &files, None, None).unwrap();
        let merged = pdfium.load_pdf_from_byte_vec(bytes, None).unwrap();
        assert_eq!(merged.pages().get(0).unwrap().width().value, 200.0);
        assert_eq!(merged.pages().get(1).unwrap().width().value, 400.0);
        assert!(paths.iter().all(|path| path.exists()));
        drop(merged);
        drop(files);
        assert!(paths.iter().all(|path| !path.exists()));
    }
}
