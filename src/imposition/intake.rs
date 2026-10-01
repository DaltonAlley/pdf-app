//! Bounded intake validation and source provenance for mixed artwork.
use super::{geometry::source_page_geometry_with_bleed, model::PdfAnalysis};
use crate::{
    adapters::{StagedImposeUpload, UploadFile},
    error::{AppError, AppResult},
    MAX_SOURCE_PDF_PAGES,
};
use lopdf::Document;
use std::path::Path;

#[derive(Debug)]
pub(crate) struct ArtworkPageIdentity {
    filename: String,
    page_number: usize,
}
impl ArtworkPageIdentity {
    pub(crate) fn first_page(filename: String) -> Self {
        Self {
            filename,
            page_number: 1,
        }
    }
}
#[derive(Debug)]
pub(crate) struct StagedArtworkPdf {
    upload: StagedImposeUpload,
    page_identities: Option<Vec<ArtworkPageIdentity>>,
}
impl StagedArtworkPdf {
    pub(crate) fn uploaded_pdf(upload: StagedImposeUpload) -> Self {
        Self {
            upload,
            page_identities: None,
        }
    }
    pub(crate) fn converted_images(
        upload: StagedImposeUpload,
        page_identities: Vec<ArtworkPageIdentity>,
    ) -> Self {
        Self {
            upload,
            page_identities: Some(page_identities),
        }
    }
    pub(crate) fn into_upload(self) -> StagedImposeUpload {
        self.upload
    }
    fn page_identity(&self, number: usize) -> (&str, usize) {
        self.page_identities
            .as_ref()
            .and_then(|ids| ids.get(number.saturating_sub(1)))
            .map(|id| (id.filename.as_str(), id.page_number))
            .unwrap_or((self.upload.filename(), number))
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OrientationNormalization {
    identities: Vec<(String, usize, bool)>,
}
impl OrientationNormalization {
    pub(crate) fn image(filename: String) -> Self {
        Self {
            identities: vec![(filename, 1, true)],
        }
    }
    pub(crate) fn append_warning(self, analysis: &mut PdfAnalysis) {
        for (page, (filename, number, assumed)) in
            analysis.source_pages.iter_mut().zip(self.identities)
        {
            page.filename = Some(filename);
            page.original_page_number = Some(number);
            page.physical_size_assumed = assumed;
        }
    }
    pub(crate) fn tag_path(&self, path: &Path) -> AppResult<()> {
        if self.identities.is_empty() {
            return Ok(());
        }
        let mut document = Document::load(path)
            .map_err(|e| AppError::bad_request_cause("could not read prepared artwork", e))?;
        self.tag_document(&mut document)?;
        document
            .save(path)
            .map_err(|e| AppError::internal_cause("could not save artwork identities", e))?;
        Ok(())
    }
    pub(crate) fn tag_upload(&self, file: UploadFile) -> AppResult<UploadFile> {
        if self.identities.is_empty() {
            return Ok(file);
        }
        let mut document = Document::load_mem(&file.bytes)
            .map_err(|e| AppError::bad_request_cause("could not read prepared artwork", e))?;
        self.tag_document(&mut document)?;
        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .map_err(|e| AppError::internal_cause("could not save artwork identities", e))?;
        Ok(UploadFile {
            filename: file.filename,
            bytes: bytes.into(),
        })
    }
    fn tag_document(&self, document: &mut Document) -> AppResult<()> {
        let pages = document.get_pages();
        if pages.len() != self.identities.len() {
            return Err(AppError::bad_request("prepared artwork page count changed"));
        }
        for (id, (filename, number, assumed)) in pages.into_values().zip(&self.identities) {
            let page = document
                .get_dictionary_mut(id)
                .map_err(|e| AppError::bad_request_cause("invalid source page", e))?;
            page.set(
                "PdfToolsSourceFilename",
                lopdf::Object::string_literal(filename.as_bytes().to_vec()),
            );
            page.set("PdfToolsSourcePage", *number as i64);
            page.set("PdfToolsAssumedPhysicalSize", *assumed);
        }
        Ok(())
    }
}
/// Preserve each valid page's physical size and orientation, never match it to page one.
pub(crate) fn normalize_staged_imposition_geometry(
    files: &[StagedArtworkPdf],
) -> AppResult<OrientationNormalization> {
    if files.is_empty() {
        return Err(AppError::bad_request("no artwork files to impose"));
    }
    let mut result = OrientationNormalization::default();
    for file in files {
        let document = Document::load(file.upload.path()).map_err(|e| {
            AppError::bad_request_cause(
                format!("could not inspect `{}`", file.upload.filename()),
                e,
            )
        })?;
        let pages = document.get_pages();
        if pages.is_empty() {
            return Err(AppError::bad_request("source PDF has no pages"));
        }
        if file
            .page_identities
            .as_ref()
            .is_some_and(|ids| ids.len() != pages.len())
        {
            return Err(AppError::bad_request(
                "source image identities do not match pages",
            ));
        }
        if result.identities.len().saturating_add(pages.len()) > MAX_SOURCE_PDF_PAGES {
            return Err(AppError::payload_too_large(
                "source PDFs contain too many pages",
            ));
        }
        for (number, id) in pages {
            let (filename, local_page) = file.page_identity(number as usize);
            source_page_geometry_with_bleed(&document, id, None).map_err(|e| {
                AppError::bad_request_cause(
                    format!("could not inspect `{filename}` page {local_page}"),
                    e,
                )
            })?;
            result.identities.push((
                filename.to_string(),
                local_page,
                file.page_identities.is_some(),
            ));
        }
    }
    Ok(result)
}
