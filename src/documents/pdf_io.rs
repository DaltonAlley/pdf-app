use std::path::Path;

use bytes::Bytes;
use lopdf::{Document as LoDocument, Object};
use pdfium_render::prelude::{PdfDocument, PdfPageIndex, Pdfium};

use super::page_selection::PageSelection;
use crate::{
    adapters::BoundedBytes,
    error::{AppError, AppResult},
};

pub(crate) fn load_pdf<'a>(pdfium: &'a Pdfium, bytes: Bytes) -> AppResult<PdfDocument<'a>> {
    pdfium
        .load_pdf_from_byte_vec(bytes.into(), None)
        .map_err(|err| AppError::bad_request(format!("could not read PDF: {err}")))
}

pub(crate) fn load_pdf_path<'a>(pdfium: &'a Pdfium, path: &Path) -> AppResult<PdfDocument<'a>> {
    pdfium
        .load_pdf_from_file(path, None)
        .map_err(|err| AppError::bad_request(format!("could not read PDF: {err}")))
}

pub(crate) fn create_pdf<'a>(pdfium: &'a Pdfium, action: &str) -> AppResult<PdfDocument<'a>> {
    pdfium
        .create_new_pdf()
        .map_err(|err| AppError::internal_cause(format!("could not create {action} PDF"), err))
}

pub(crate) fn save_pdf_bounded(
    document: &PdfDocument<'_>,
    action: &str,
    max_bytes: Option<usize>,
) -> AppResult<Vec<u8>> {
    let mut output = BoundedBytes::new(4 * 1024, max_bytes);
    let result = document.save_to_writer(&mut output);
    output.finish(result, &format!("could not save {action} PDF"))
}

pub(crate) fn estimated_lopdf_output_capacity(document: &LoDocument) -> usize {
    document
        .objects
        .values()
        .fold(4 * 1024usize, |capacity, object| {
            let payload = match object {
                Object::Stream(stream) => stream.content.len(),
                Object::String(bytes, _) => bytes.len(),
                _ => 96,
            };
            capacity.saturating_add(payload).saturating_add(32)
        })
}

pub(crate) fn page_count(count: PdfPageIndex) -> AppResult<usize> {
    usize::try_from(count).map_err(|error| {
        AppError::internal_cause(format!("invalid PDF page count: {count}"), error)
    })
}

pub(crate) fn page_index(page_number: usize) -> AppResult<PdfPageIndex> {
    let zero_based = page_number
        .checked_sub(1)
        .ok_or_else(|| AppError::bad_request("page numbers start at 1"))?;

    PdfPageIndex::try_from(zero_based)
        .map_err(|_| AppError::bad_request(format!("page {page_number} is out of range")))
}

pub(crate) fn resolve_pages(
    selection: PageSelection,
    page_count: usize,
    empty_message: &str,
) -> AppResult<Vec<usize>> {
    let pages = selection.resolve(page_count)?;

    if pages.is_empty() {
        return Err(AppError::bad_request(empty_message));
    }

    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_index_rejects_zero() {
        assert!(page_index(0).is_err());
    }

    #[test]
    fn page_index_converts_to_zero_based_pdfium_index() {
        assert_eq!(page_index(1).unwrap(), 0);
        assert_eq!(page_index(3).unwrap(), 2);
    }

    #[test]
    fn resolve_pages_rejects_empty_and_out_of_range_selections() {
        assert_eq!(
            resolve_pages(PageSelection::All, 0, "empty")
                .unwrap_err()
                .to_string(),
            "empty"
        );
        assert_eq!(
            resolve_pages(PageSelection::Pages(vec![]), 2, "empty")
                .unwrap_err()
                .to_string(),
            "no page numbers specified"
        );
        assert_eq!(
            resolve_pages(PageSelection::Pages(vec![3]), 2, "empty")
                .unwrap_err()
                .to_string(),
            "page 3 is out of range; PDF has 2 pages"
        );
        assert_eq!(
            resolve_pages(PageSelection::Pages(vec![0]), 2, "empty")
                .unwrap_err()
                .to_string(),
            "page numbers start at 1"
        );
    }
}
