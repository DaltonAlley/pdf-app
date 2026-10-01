//! Concrete file-transfer adapters shared at domain boundaries.

mod download;
#[cfg(test)]
mod test_pdfium;
mod upload;

pub(crate) use download::{validate_download_size, BoundedBytes, FileDownload};
#[cfg(test)]
pub(crate) use test_pdfium::{test_pdfium, TestPdfium};
pub(crate) use upload::{
    initialize_impose_staging, is_pdf, read_multipart, require_at_least_pdfs, require_files,
    require_images, require_one_impose_source, require_one_pdf, require_pdfs,
    stage_impose_multipart, FormData, ImposeUploadLimits, StagedImposeForm, StagedImposeUpload,
    UploadFile, DEFAULT_IMPOSE_UPLOAD_TIMEOUT_SECONDS, DEFAULT_MAX_IMPOSE_UPLOAD_BYTES,
};
