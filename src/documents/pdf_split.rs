use std::io::{Cursor, Write};

use pdfium_render::prelude::Pdfium;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

use super::{output_filename::source_stem, page_selection::PageSelection, pdf_io};
use crate::{
    adapters::{validate_download_size, FormData, UploadFile},
    error::{AppError, AppResult},
    progress::{report, ProgressCallback},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtractOutputMode {
    Individual,
    Combined,
    Chunks { chunk_size: usize },
}

impl ExtractOutputMode {
    pub(crate) fn from_form(form: &FormData) -> AppResult<Self> {
        let output_mode = form.field("outputMode").unwrap_or("individual");
        let chunk_size = form.field("chunkSize");
        match output_mode {
            "individual" => {
                reject_unused_chunk_size(chunk_size)?;
                Ok(Self::Individual)
            }
            "combined" => {
                reject_unused_chunk_size(chunk_size)?;
                Ok(Self::Combined)
            }
            "chunks" => {
                let chunk_size = chunk_size
                    .ok_or_else(|| AppError::bad_request("chunkSize is required for chunks"))?
                    .parse::<usize>()
                    .map_err(|_| AppError::bad_request("chunkSize must be a positive integer"))?;
                if chunk_size == 0 || chunk_size > crate::MAX_SOURCE_PDF_PAGES {
                    return Err(AppError::bad_request(format!(
                        "chunkSize must be between 1 and {}",
                        crate::MAX_SOURCE_PDF_PAGES
                    )));
                }
                Ok(Self::Chunks { chunk_size })
            }
            _ => Err(AppError::bad_request(
                "outputMode must be individual, combined, or chunks",
            )),
        }
    }

    pub(crate) fn is_combined(self) -> bool {
        matches!(self, Self::Combined)
    }
}

fn reject_unused_chunk_size(chunk_size: Option<&str>) -> AppResult<()> {
    if chunk_size.is_some() {
        return Err(AppError::bad_request(
            "chunkSize is only valid when outputMode is chunks",
        ));
    }
    Ok(())
}

#[cfg(test)]
fn split_pdf_pages(
    pdfium: &Pdfium,
    file: UploadFile,
    selection: PageSelection,
    max_pages: usize,
    progress: Option<ProgressCallback>,
    max_download_bytes: Option<usize>,
) -> AppResult<Vec<u8>> {
    extract_pdf_pages(
        pdfium,
        file,
        selection,
        max_pages,
        ExtractOutputMode::Individual,
        progress,
        max_download_bytes,
    )
}

pub(crate) fn extract_pdf_pages(
    pdfium: &Pdfium,
    file: UploadFile,
    selection: PageSelection,
    max_pages: usize,
    output_mode: ExtractOutputMode,
    progress: Option<ProgressCallback>,
    max_download_bytes: Option<usize>,
) -> AppResult<Vec<u8>> {
    let source_name = source_stem(&file.filename);
    let document = pdf_io::load_pdf(pdfium, file.bytes)?;
    let page_count = pdf_io::page_count(document.pages().len())?;
    if page_count == 0 {
        return Err(AppError::bad_request("cannot split a PDF with no pages"));
    }
    if page_count > crate::MAX_SOURCE_PDF_PAGES {
        return Err(AppError::payload_too_large(format!(
            "source PDFs are limited to {} pages",
            crate::MAX_SOURCE_PDF_PAGES
        )));
    }

    report(&progress, 60, "Extracting pages")?;
    let pages = pdf_io::resolve_pages(selection, page_count, "cannot split a PDF with no pages")?;
    if pages.len() > max_pages {
        return Err(AppError::bad_request(format!(
            "requested {} pages; limit is {max_pages}",
            pages.len()
        )));
    }
    match output_mode {
        ExtractOutputMode::Combined => {
            combine_pages(pdfium, &document, &pages, &progress, max_download_bytes)
        }
        mode @ (ExtractOutputMode::Individual | ExtractOutputMode::Chunks { .. }) => {
            zip_page_chunks(
                pdfium,
                &document,
                &source_name,
                &pages,
                mode,
                &progress,
                max_download_bytes,
            )
        }
    }
}

fn combine_pages(
    pdfium: &Pdfium,
    document: &pdfium_render::prelude::PdfDocument<'_>,
    pages: &[usize],
    progress: &Option<ProgressCallback>,
    max_download_bytes: Option<usize>,
) -> AppResult<Vec<u8>> {
    let mut output = pdf_io::create_pdf(pdfium, "extracted")?;
    copy_pages(&mut output, document, pages, progress, 0, pages.len())?;
    report(
        progress,
        88,
        format!("Combined {} selected pages", pages.len()),
    )?;
    pdf_io::save_pdf_bounded(&output, "extracted", max_download_bytes)
}

fn zip_page_chunks(
    pdfium: &Pdfium,
    document: &pdfium_render::prelude::PdfDocument<'_>,
    source_name: &str,
    pages: &[usize],
    output_mode: ExtractOutputMode,
    progress: &Option<ProgressCallback>,
    max_download_bytes: Option<usize>,
) -> AppResult<Vec<u8>> {
    let (chunk_size, individual_names) = match output_mode {
        ExtractOutputMode::Individual => (1, true),
        ExtractOutputMode::Chunks { chunk_size } => (chunk_size, false),
        ExtractOutputMode::Combined => {
            return Err(AppError::Internal(
                "combined extraction cannot be packaged as chunks".to_string(),
            ));
        }
    };
    let mut cursor = Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(&mut cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    let selected_page_count = pages.len();
    let chunks = pages.chunks(chunk_size).collect::<Vec<_>>();
    let mut completed = 0usize;
    for (chunk_index, chunk) in chunks.iter().enumerate() {
        let mut output = pdf_io::create_pdf(pdfium, "split")?;
        copy_pages(
            &mut output,
            document,
            chunk,
            progress,
            completed,
            selected_page_count,
        )?;
        let bytes = pdf_io::save_pdf_bounded(&output, "split", max_download_bytes)?;

        let entry_name = if individual_names {
            format!("{source_name}-page-{:04}.pdf", chunk[0])
        } else {
            format!("{source_name}-chunk-{:04}.pdf", chunk_index + 1)
        };
        zip.start_file(entry_name, options)?;
        zip.write_all(&bytes)?;
        let output_bytes = zip
            .get_ref()
            .map(|cursor| cursor.position() as usize)
            .ok_or_else(|| AppError::Internal("split ZIP writer is unavailable".to_string()))?;
        validate_download_size(output_bytes, max_download_bytes)?;
        completed += chunk.len();
        report(
            progress,
            60 + ((completed * 28) / selected_page_count.max(1)) as u8,
            format!("Extracted {completed} of {selected_page_count} pages"),
        )?;
    }

    report(progress, 90, "Packaging extracted PDFs")?;
    zip.finish()?;
    let bytes = cursor.into_inner();
    validate_download_size(bytes.len(), max_download_bytes)?;
    Ok(bytes)
}

fn copy_pages(
    output: &mut pdfium_render::prelude::PdfDocument<'_>,
    document: &pdfium_render::prelude::PdfDocument<'_>,
    pages: &[usize],
    progress: &Option<ProgressCallback>,
    completed: usize,
    selected_page_count: usize,
) -> AppResult<()> {
    // Check before every native copy and once more before the caller serializes.
    // Even when integer percentages repeat, cancellation is polled on every page.
    for copied in 0..=pages.len() {
        let completed = completed + copied;
        report(
            progress,
            60 + ((completed * 28) / selected_page_count.max(1)) as u8,
            format!("Extracted {completed} of {selected_page_count} pages"),
        )?;
        let Some(&page) = pages.get(copied) else {
            break;
        };
        let destination = output.pages().len();
        output
            .pages_mut()
            .copy_page_from_document(document, pdf_io::page_index(page)?, destination)
            .map_err(|err| AppError::internal_cause(format!("could not copy page {page}"), err))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Read,
        sync::{Arc, Mutex},
    };

    use pdfium_render::prelude::{PdfPageObjectsCommon, PdfPagePaperSize, PdfPoints, Pdfium};
    use zip::ZipArchive;

    fn pdfium_or_skip() -> Option<crate::adapters::TestPdfium> {
        let pdfium = crate::test_pdfium();
        if pdfium.is_none() {
            eprintln!("skipping PDFium test: PDF_TOOLS_PDFIUM_PATH is not set");
        }
        pdfium
    }

    fn sample_pdf_bytes(pdfium: &Pdfium, page_count: usize) -> Vec<u8> {
        let mut doc = pdfium.create_new_pdf().unwrap();
        let font = doc.fonts_mut().helvetica();
        for number in 1..=page_count {
            let mut page = doc
                .pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .unwrap();
            page.objects_mut()
                .create_text_object(
                    PdfPoints::new(40.0),
                    PdfPoints::new(700.0),
                    format!("Source page {number}"),
                    font,
                    PdfPoints::new(20.0),
                )
                .unwrap();
        }
        doc.save_to_bytes().unwrap()
    }

    fn page_texts(pdfium: &Pdfium, bytes: Vec<u8>) -> Vec<String> {
        let document = pdfium.load_pdf_from_byte_vec(bytes, None).unwrap();
        document
            .pages()
            .iter()
            .map(|page| page.text().unwrap().all().trim().to_string())
            .collect()
    }

    fn zip_entries(bytes: Vec<u8>) -> Vec<(String, Vec<u8>)> {
        let mut archive = ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut entries = Vec::new();
        for index in 0..archive.len() {
            let mut file = archive.by_index(index).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            entries.push((file.name().to_string(), bytes));
        }
        entries
    }

    #[test]
    fn split_pages_zips_selected_page_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let bytes = split_pdf_pages(
            &pdfium,
            UploadFile {
                filename: "source.pdf".to_string(),
                bytes: sample_pdf_bytes(&pdfium, 2).into(),
            },
            PageSelection::Pages(vec![2]),
            10,
            None,
            None,
        )
        .unwrap();
        let entries = zip_entries(bytes);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "source-page-0002.pdf");
        let split = pdfium
            .load_pdf_from_byte_vec(entries[0].1.clone(), None)
            .unwrap();
        assert_eq!(split.pages().len(), 1);
    }

    #[test]
    fn split_pages_zips_all_pages_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let bytes = split_pdf_pages(
            &pdfium,
            UploadFile {
                filename: "source.pdf".to_string(),
                bytes: sample_pdf_bytes(&pdfium, 2).into(),
            },
            PageSelection::All,
            10,
            None,
            None,
        )
        .unwrap();
        let entries = zip_entries(bytes);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "source-page-0001.pdf");
        assert_eq!(entries[1].0, "source-page-0002.pdf");
        for (_, bytes) in entries {
            let split = pdfium.load_pdf_from_byte_vec(bytes, None).unwrap();
            assert_eq!(split.pages().len(), 1);
        }
    }

    #[test]
    fn extract_options_preserve_default_and_validate_chunk_contract() {
        assert_eq!(
            ExtractOutputMode::from_form(&FormData::default()).unwrap(),
            ExtractOutputMode::Individual
        );
        assert_eq!(
            ExtractOutputMode::from_form(&FormData {
                files: Vec::new(),
                fields: vec![
                    ("outputMode".to_string(), "chunks".to_string()),
                    ("chunkSize".to_string(), "3".to_string()),
                ],
            })
            .unwrap(),
            ExtractOutputMode::Chunks { chunk_size: 3 }
        );
        for fields in [
            vec![("outputMode".to_string(), "chunks".to_string())],
            vec![
                ("outputMode".to_string(), "chunks".to_string()),
                ("chunkSize".to_string(), "0".to_string()),
            ],
            vec![
                ("outputMode".to_string(), "combined".to_string()),
                ("chunkSize".to_string(), "2".to_string()),
            ],
            vec![("outputMode".to_string(), "archive".to_string())],
        ] {
            assert!(ExtractOutputMode::from_form(&FormData {
                files: Vec::new(),
                fields,
            })
            .is_err());
        }
    }

    #[test]
    fn combined_mode_returns_one_pdf_in_selected_order_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let bytes = extract_pdf_pages(
            &pdfium,
            UploadFile {
                filename: "source.pdf".to_string(),
                bytes: sample_pdf_bytes(&pdfium, 4).into(),
            },
            PageSelection::Pages(vec![4, 2, 4, 3]),
            10,
            ExtractOutputMode::Combined,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            page_texts(&pdfium, bytes),
            [
                "Source page 4",
                "Source page 2",
                "Source page 4",
                "Source page 3"
            ]
        );
    }

    #[test]
    fn chunk_mode_packages_multi_page_pdfs_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let bytes = extract_pdf_pages(
            &pdfium,
            UploadFile {
                filename: "source.pdf".to_string(),
                bytes: sample_pdf_bytes(&pdfium, 5).into(),
            },
            PageSelection::Pages(vec![5, 2, 5, 1, 4]),
            10,
            ExtractOutputMode::Chunks { chunk_size: 2 },
            None,
            None,
        )
        .unwrap();
        let entries = zip_entries(bytes);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.0.as_str())
                .collect::<Vec<_>>(),
            [
                "source-chunk-0001.pdf",
                "source-chunk-0002.pdf",
                "source-chunk-0003.pdf"
            ]
        );
        let contents = entries
            .into_iter()
            .map(|(_, bytes)| page_texts(&pdfium, bytes))
            .collect::<Vec<_>>();
        assert_eq!(
            contents,
            [
                vec!["Source page 5", "Source page 2"],
                vec!["Source page 5", "Source page 1"],
                vec!["Source page 4"],
            ]
        );
    }

    #[test]
    fn cancellation_stops_remaining_native_page_copies() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let source = pdfium
            .load_pdf_from_byte_vec(sample_pdf_bytes(&pdfium, 3), None)
            .unwrap();
        // A large denominator deliberately keeps consecutive percentages equal.
        for stop_after in [0, 1, 2] {
            let mut output = pdfium.create_new_pdf().unwrap();
            let events = Arc::new(Mutex::new(Vec::new()));
            let recorded = events.clone();
            let progress: ProgressCallback = Arc::new(move |percent, stage| {
                recorded.lock().unwrap().push((percent, stage.clone()));
                stage == format!("Extracted {stop_after} of 100 pages")
            });
            let error =
                copy_pages(&mut output, &source, &[3, 1, 2], &Some(progress), 0, 100).unwrap_err();
            assert!(matches!(error, AppError::Cancelled(_)));
            assert_eq!(
                pdf_io::page_count(output.pages().len()).unwrap(),
                stop_after
            );
            let events = events.lock().unwrap();
            assert_eq!(events.len(), stop_after + 1);
            assert!(events.iter().all(|(percent, _)| *percent == 60));
        }
    }

    #[test]
    fn cancellation_before_serialization_wins_over_zero_byte_output_limit() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let source = sample_pdf_bytes(&pdfium, 3);
        for (mode, copied) in [
            (ExtractOutputMode::Individual, 1),
            (ExtractOutputMode::Combined, 3),
            (ExtractOutputMode::Chunks { chunk_size: 2 }, 2),
        ] {
            let extract = |progress| {
                extract_pdf_pages(
                    &pdfium,
                    UploadFile {
                        filename: "source.pdf".to_string(),
                        bytes: source.clone().into(),
                    },
                    PageSelection::All,
                    10,
                    mode,
                    progress,
                    Some(0),
                )
            };
            // Serialization is guaranteed to fail if reached. The control proves it.
            assert!(matches!(
                extract(None).unwrap_err(),
                AppError::PayloadTooLarge(_)
            ));
            let events = Arc::new(Mutex::new(Vec::new()));
            let recorded = events.clone();
            let progress: ProgressCallback = Arc::new(move |percent, stage| {
                recorded.lock().unwrap().push((percent, stage.clone()));
                stage == format!("Extracted {copied} of 3 pages")
            });
            assert!(matches!(
                extract(Some(progress)).unwrap_err(),
                AppError::Cancelled(_)
            ));
            let events = events.lock().unwrap();
            assert_eq!(events.len(), copied + 2); // Initial extraction plus 0..=copied.
            assert_eq!(
                events.last().unwrap().1,
                format!("Extracted {copied} of 3 pages")
            );
        }
    }

    #[test]
    fn split_checks_the_completed_zip_size() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let source = sample_pdf_bytes(&pdfium, 1);
        let complete = split_pdf_pages(
            &pdfium,
            UploadFile {
                filename: "source.pdf".to_string(),
                bytes: source.clone().into(),
            },
            PageSelection::All,
            10,
            None,
            None,
        )
        .unwrap();

        let error = split_pdf_pages(
            &pdfium,
            UploadFile {
                filename: "source.pdf".to_string(),
                bytes: source.into(),
            },
            PageSelection::All,
            10,
            None,
            Some(complete.len() - 1),
        )
        .unwrap_err();

        assert!(matches!(error, AppError::PayloadTooLarge(_)));
    }

    #[test]
    fn page_entry_stem_uses_shared_filename_policy() {
        assert_eq!(source_stem(r"folder\source file.pdf"), "source-file");
        assert_eq!(source_stem("source.PdF"), "source");
        assert_eq!(source_stem("/tmp/.pdf"), "document");
        assert_eq!(source_stem("plain"), "plain");
    }
}
