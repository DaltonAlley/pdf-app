use std::{
    io::Read,
    path::{Path, PathBuf},
};

use axum::extract::Multipart;
use bytes::Bytes;
use tokio::{fs, io::AsyncWriteExt};

use crate::error::{AppError, AppResult};

const MULTIPART_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const MAX_TEXT_FIELD_BYTES: usize = 64 * 1024;
const MAX_MULTIPART_TEXT_FIELDS: usize = 200;
const MAX_MULTIPART_FILES: usize = 512;
const MAX_MULTIPART_FILE_BYTES: usize = 512 * 1024 * 1024;
const MAX_MULTIPART_TOTAL_FILE_BYTES: usize = 1024 * 1024 * 1024;
const IMPOSE_STAGING_DIRECTORY: &str = "impose-staging";
const IMPOSE_STAGING_PREFIX: &str = "pdf-tools-impose-";
const IMPOSE_STAGING_SUFFIX: &str = ".upload";
pub(crate) const DEFAULT_MAX_IMPOSE_UPLOAD_BYTES: usize = 512 * 1024 * 1024;
pub(crate) const DEFAULT_IMPOSE_UPLOAD_TIMEOUT_SECONDS: usize = 120;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ImposeUploadLimits {
    max_total_bytes: usize,
    read_timeout: std::time::Duration,
}

impl ImposeUploadLimits {
    pub(crate) const fn new(max_total_bytes: usize, read_timeout: std::time::Duration) -> Self {
        Self {
            max_total_bytes,
            read_timeout,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct UploadFile {
    pub(crate) filename: String,
    pub(crate) bytes: Bytes,
}

#[derive(Debug, Default)]
pub(crate) struct FormData {
    pub(crate) files: Vec<UploadFile>,
    pub(crate) fields: Vec<(String, String)>,
}

/// A multipart impose upload staged on disk. Dropping it removes the staging
/// file, including when request processing is cancelled.
#[derive(Debug)]
pub(crate) struct StagedImposeUpload {
    filename: String,
    path: PathBuf,
}

impl StagedImposeUpload {
    #[cfg(test)]
    pub(crate) fn for_test(filename: impl Into<String>, path: PathBuf) -> Self {
        Self {
            filename: filename.into(),
            path,
        }
    }

    pub(crate) fn filename(&self) -> &str {
        &self.filename
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn byte_len(&self) -> AppResult<u64> {
        self.path
            .metadata()
            .map(|metadata| metadata.len())
            .map_err(|error| AppError::internal_cause("could not inspect staged source", error))
    }

    pub(crate) fn is_pdf(&self) -> AppResult<bool> {
        let mut input = std::fs::File::open(&self.path)
            .map_err(|error| AppError::internal_cause("could not open staged source", error))?;
        let mut header = [0u8; 5];
        let read = input
            .read(&mut header)
            .map_err(|error| AppError::internal_cause("could not inspect staged source", error))?;
        Ok(read == header.len() && is_pdf(&header))
    }

    /// Writes a generated intermediate back to staging so callers can release
    /// its in-memory representation before preparing the next artwork.
    pub(crate) fn from_bytes(
        staging_dir: &Path,
        filename: String,
        bytes: &[u8],
    ) -> AppResult<Self> {
        let path = allocate_staging_path(staging_dir)?;
        let upload = Self {
            filename,
            path: path.clone(),
        };
        std::fs::write(&path, bytes).map_err(|error| {
            AppError::storage_cause(
                format!("could not stage generated PDF `{}`", path.display()),
                error,
            )
        })?;
        Ok(upload)
    }

    /// Materializes bytes only at the image parser boundary. PDFs stay path-backed.
    pub(crate) fn into_image_upload(self) -> AppResult<UploadFile> {
        let mut input = std::fs::File::open(&self.path).map_err(|error| {
            AppError::internal_cause(
                format!("could not open staged image `{}`", self.path.display()),
                error,
            )
        })?;
        let length = input
            .metadata()
            .map_err(|error| AppError::internal_cause("could not inspect staged image", error))?
            .len();
        let capacity = usize::try_from(length).map_err(|_| {
            AppError::payload_too_large("staged image is too large for the image parser")
        })?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(capacity).map_err(|error| {
            AppError::payload_too_large(format!("staged image cannot be allocated: {error}"))
        })?;
        input
            .read_to_end(&mut bytes)
            .map_err(|error| AppError::internal_cause("could not read staged image", error))?;
        Ok(UploadFile {
            filename: self.filename.clone(),
            bytes: bytes.into(),
        })
    }
}

impl Drop for StagedImposeUpload {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %self.path.display(), %error, "staged impose upload could not be removed")
            }
        }
    }
}

pub(crate) struct StagedImposeForm {
    pub(crate) files: Vec<StagedImposeUpload>,
    pub(crate) fields: Vec<(String, String)>,
}

/// Streams impose sources to disk within an explicit aggregate byte budget and
/// elapsed read deadline. Staging remains disk-backed throughout intake.
pub(crate) async fn stage_impose_multipart(
    multipart: Multipart,
    staging_dir: &Path,
    limits: ImposeUploadLimits,
) -> AppResult<StagedImposeForm> {
    tokio::time::timeout(
        limits.read_timeout,
        collect_staged_impose_multipart(multipart, staging_dir, limits.max_total_bytes),
    )
    .await
    .map_err(|_| AppError::RequestTimeout("impose upload timed out".to_string()))?
}

async fn collect_staged_impose_multipart(
    mut multipart: Multipart,
    staging_dir: &Path,
    max_total_bytes: usize,
) -> AppResult<StagedImposeForm> {
    let mut staged = Vec::new();
    let mut fields = Vec::new();
    let mut text_field_count = 0usize;
    let mut total_file_bytes = 0usize;

    while let Some(mut field) = multipart.next_field().await? {
        let name = field.name().unwrap_or("").to_string();
        if matches!(name.as_str(), "file" | "files" | "files[]") {
            if staged.len() >= MAX_MULTIPART_FILES {
                return Err(AppError::bad_request("too many impose artwork files"));
            }
            let filename = field
                .file_name()
                .map(safe_display_name)
                .unwrap_or_else(|| "upload".to_string());
            let path = allocate_staging_path(staging_dir)?;
            let upload = StagedImposeUpload {
                filename,
                path: path.clone(),
            };
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
                .map_err(|error| {
                    AppError::storage_cause(
                        format!("could not create staged impose source `{}`", path.display()),
                        error,
                    )
                })?;
            let mut size = 0usize;
            while let Some(chunk) = field.chunk().await? {
                size = size.checked_add(chunk.len()).ok_or_else(|| {
                    AppError::payload_too_large("uploaded file size cannot be represented")
                })?;
                total_file_bytes =
                    checked_impose_upload_growth(total_file_bytes, chunk.len(), max_total_bytes)?;
                output.write_all(&chunk).await.map_err(|error| {
                    AppError::storage_cause("could not write staged impose source", error)
                })?;
            }
            output.flush().await.map_err(|error| {
                AppError::storage_cause("could not flush staged impose source", error)
            })?;
            drop(output);
            if size == 0 {
                return Err(AppError::bad_request("uploaded file is empty"));
            }
            staged.push(upload);
        } else if !name.is_empty() {
            text_field_count += 1;
            if text_field_count > MAX_MULTIPART_TEXT_FIELDS {
                return Err(AppError::payload_too_large(format!(
                    "multipart forms are limited to {MAX_MULTIPART_TEXT_FIELDS} text fields"
                )));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = field.chunk().await? {
                let new_len = bytes.len().checked_add(chunk.len()).ok_or_else(|| {
                    AppError::payload_too_large("multipart text field is too large")
                })?;
                if new_len > MAX_TEXT_FIELD_BYTES {
                    return Err(AppError::payload_too_large(format!(
                        "multipart text fields are limited to {MAX_TEXT_FIELD_BYTES} bytes"
                    )));
                }
                bytes.extend_from_slice(&chunk);
            }
            let value = String::from_utf8(bytes)
                .map_err(|_| AppError::bad_request(format!("field `{name}` is not valid UTF-8")))?;
            fields.push((name, value.trim().to_string()));
        }
    }
    Ok(StagedImposeForm {
        files: if staged.is_empty() {
            return Err(AppError::bad_request(
                "impose requires one or more PDF, PNG, or JPEG files",
            ));
        } else {
            staged
        },
        fields,
    })
}

fn checked_impose_upload_growth(
    current_total_bytes: usize,
    chunk_bytes: usize,
    max_total_bytes: usize,
) -> AppResult<usize> {
    let total_bytes = current_total_bytes
        .checked_add(chunk_bytes)
        .ok_or_else(|| {
            AppError::payload_too_large("total impose upload size cannot be represented")
        })?;
    if total_bytes > max_total_bytes {
        return Err(AppError::payload_too_large(format!(
            "impose artwork exceeds the configured {} MiB total upload limit",
            max_total_bytes / (1024 * 1024)
        )));
    }
    Ok(total_bytes)
}

pub(crate) fn initialize_impose_staging(data_dir: &Path) -> AppResult<PathBuf> {
    let staging_dir = data_dir.join(IMPOSE_STAGING_DIRECTORY);
    std::fs::create_dir_all(&staging_dir).map_err(|error| {
        AppError::storage_cause(
            format!(
                "could not create impose staging directory `{}`",
                staging_dir.display()
            ),
            error,
        )
    })?;
    for entry in std::fs::read_dir(&staging_dir).map_err(|error| {
        AppError::internal_cause(
            format!(
                "could not inspect impose staging directory `{}`",
                staging_dir.display()
            ),
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            AppError::internal_cause("could not inspect an impose staging entry", error)
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(IMPOSE_STAGING_PREFIX) && name.ends_with(IMPOSE_STAGING_SUFFIX) {
            std::fs::remove_file(entry.path()).map_err(|error| {
                AppError::internal_cause("could not remove an orphaned impose upload", error)
            })?;
        }
    }
    Ok(staging_dir)
}

fn allocate_staging_path(staging_dir: &Path) -> AppResult<PathBuf> {
    for _ in 0..4 {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| AppError::internal_cause("could not allocate impose upload", error))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = staging_dir.join(format!(
            "{IMPOSE_STAGING_PREFIX}{suffix}{IMPOSE_STAGING_SUFFIX}"
        ));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(AppError::Internal(
        "could not allocate a unique impose upload path".to_string(),
    ))
}

impl FormData {
    pub(crate) fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn required_field(&self, name: &str) -> AppResult<&str> {
        self.field(name)
            .ok_or_else(|| AppError::bad_request(format!("missing field `{name}`")))
    }

    pub(crate) fn into_one_file(self) -> AppResult<UploadFile> {
        if self.files.len() != 1 {
            return Err(AppError::bad_request("expected exactly one file"));
        }

        self.files
            .into_iter()
            .next()
            .ok_or_else(|| AppError::bad_request("expected exactly one file"))
    }
}

pub(crate) async fn read_multipart(multipart: Multipart) -> AppResult<FormData> {
    tokio::time::timeout(MULTIPART_READ_TIMEOUT, collect_multipart(multipart))
        .await
        .map_err(|_| AppError::RequestTimeout("upload timed out".to_string()))?
}

async fn collect_multipart(mut multipart: Multipart) -> AppResult<FormData> {
    let mut form = FormData::default();
    let mut text_field_count = 0usize;
    let mut total_file_bytes = 0usize;

    while let Some(mut field) = multipart.next_field().await? {
        let name = field.name().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }

        let filename = field
            .file_name()
            .map(safe_display_name)
            .unwrap_or_else(|| "upload".to_string());
        if name == "file" || name == "files" || name == "files[]" {
            if form.files.len() >= MAX_MULTIPART_FILES {
                return Err(AppError::payload_too_large(format!(
                    "multipart forms are limited to {MAX_MULTIPART_FILES} files"
                )));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = field.chunk().await? {
                let (file_len, total_len) =
                    checked_file_growth(bytes.len(), total_file_bytes, chunk.len())?;
                bytes.reserve(file_len - bytes.len());
                bytes.extend_from_slice(&chunk);
                total_file_bytes = total_len;
            }
            if bytes.is_empty() {
                return Err(AppError::bad_request("uploaded file is empty"));
            }
            form.files.push(UploadFile {
                filename,
                bytes: bytes.into(),
            });
        } else {
            text_field_count += 1;
            if text_field_count > MAX_MULTIPART_TEXT_FIELDS {
                return Err(AppError::payload_too_large(format!(
                    "multipart forms are limited to {MAX_MULTIPART_TEXT_FIELDS} text fields"
                )));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = field.chunk().await? {
                let new_len = bytes.len().checked_add(chunk.len()).ok_or_else(|| {
                    AppError::payload_too_large("multipart text field is too large")
                })?;
                if new_len > MAX_TEXT_FIELD_BYTES {
                    return Err(AppError::payload_too_large(format!(
                        "multipart text fields are limited to {MAX_TEXT_FIELD_BYTES} bytes"
                    )));
                }
                bytes.extend_from_slice(&chunk);
            }
            let value = String::from_utf8(bytes)
                .map_err(|_| AppError::bad_request(format!("field `{name}` is not valid UTF-8")))?;
            form.fields.push((name, value.trim().to_string()));
        }
    }

    Ok(form)
}

fn checked_file_growth(
    current_file_bytes: usize,
    current_total_bytes: usize,
    chunk_bytes: usize,
) -> AppResult<(usize, usize)> {
    let file_bytes = current_file_bytes
        .checked_add(chunk_bytes)
        .ok_or_else(|| AppError::payload_too_large("uploaded file is too large"))?;
    let total_bytes = current_total_bytes
        .checked_add(chunk_bytes)
        .ok_or_else(|| AppError::payload_too_large("multipart files are too large"))?;
    if file_bytes > MAX_MULTIPART_FILE_BYTES {
        return Err(AppError::payload_too_large(format!(
            "each uploaded file is limited to {MAX_MULTIPART_FILE_BYTES} bytes"
        )));
    }
    if total_bytes > MAX_MULTIPART_TOTAL_FILE_BYTES {
        return Err(AppError::payload_too_large(format!(
            "multipart files are limited to {MAX_MULTIPART_TOTAL_FILE_BYTES} total bytes"
        )));
    }
    Ok((file_bytes, total_bytes))
}

fn safe_display_name(name: &str) -> String {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or("upload");
    let basename = basename
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(character, '"' | '<' | '>' | '|' | ':' | '*' | '?')
            {
                '-'
            } else {
                character
            }
        })
        .take(200)
        .collect::<String>();

    if basename.trim_matches([' ', '.', '-']).is_empty() {
        "upload".to_string()
    } else {
        basename
    }
}

pub(crate) fn is_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF-")
}

pub(crate) fn is_supported_image(bytes: &[u8]) -> bool {
    image::guess_format(bytes)
        .map(|format| matches!(format, image::ImageFormat::Png | image::ImageFormat::Jpeg))
        .unwrap_or(false)
}

pub(crate) fn require_files(files: &[UploadFile]) -> AppResult<()> {
    if files.is_empty() {
        Err(AppError::bad_request("expected at least one file"))
    } else {
        Ok(())
    }
}

pub(crate) fn require_pdfs(files: &[UploadFile], context: &str) -> AppResult<()> {
    require_files(files)?;
    if let Some(file) = files.iter().find(|file| !is_pdf(&file.bytes)) {
        return Err(AppError::bad_request(format!(
            "{context}: `{}`",
            file.filename
        )));
    }
    Ok(())
}

pub(crate) fn require_images(files: &[UploadFile], context: &str) -> AppResult<()> {
    require_files(files)?;
    if let Some(file) = files.iter().find(|file| !is_supported_image(&file.bytes)) {
        return Err(AppError::bad_request(format!(
            "{context}: `{}`",
            file.filename
        )));
    }
    Ok(())
}

pub(crate) fn require_at_least_pdfs(
    files: &[UploadFile],
    min: usize,
    count_message: &str,
) -> AppResult<()> {
    if files.len() < min {
        return Err(AppError::bad_request(count_message));
    }
    require_pdfs(files, "expected PDF input")
}

pub(crate) fn require_one_pdf(form: FormData, context: &str) -> AppResult<UploadFile> {
    let file = form.into_one_file()?;
    if !is_pdf(&file.bytes) {
        return Err(AppError::bad_request(format!(
            "{context}: `{}`",
            file.filename
        )));
    }
    Ok(file)
}

pub(crate) fn require_one_impose_source(form: FormData, context: &str) -> AppResult<UploadFile> {
    let file = form
        .into_one_file()
        .map_err(|_| AppError::bad_request("impose requires exactly one PDF, PNG, or JPEG file"))?;
    if !is_pdf(&file.bytes) && !is_supported_image(&file.bytes) {
        return Err(AppError::bad_request(format!(
            "{context}: `{}` is not a PDF, PNG, or JPEG",
            file.filename
        )));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staging_directory(label: &str) -> PathBuf {
        let data_dir = std::env::temp_dir().join(format!(
            "pdf-tools-upload-test-{}-{label}",
            std::process::id()
        ));
        initialize_impose_staging(&data_dir).unwrap()
    }

    #[test]
    fn dropping_staged_impose_upload_removes_partial_file() {
        let staging_dir = staging_directory("partial-cleanup");
        let path = allocate_staging_path(&staging_dir).unwrap();
        std::fs::write(&path, b"partial upload").unwrap();
        let staged = StagedImposeUpload {
            filename: "source.pdf".to_string(),
            path: path.clone(),
        };
        assert!(path.exists());
        drop(staged);
        assert!(!path.exists());
        std::fs::remove_dir_all(staging_dir.parent().unwrap()).unwrap();
    }

    fn file(filename: &str, bytes: &[u8]) -> UploadFile {
        UploadFile {
            filename: filename.to_string(),
            bytes: bytes.to_vec().into(),
        }
    }

    #[test]
    fn generated_staging_file_is_removed_on_drop() {
        let staging_dir = staging_directory("generated-cleanup");
        let staged =
            StagedImposeUpload::from_bytes(&staging_dir, "converted.pdf".to_string(), b"pdf")
                .unwrap();
        let path = staged.path().to_path_buf();
        assert_eq!(std::fs::read(&path).unwrap(), b"pdf");
        drop(staged);
        assert!(!path.exists());
        std::fs::remove_dir_all(staging_dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn initialization_removes_only_orphaned_impose_uploads() {
        let staging_dir = staging_directory("orphan-cleanup");
        let orphan = staging_dir.join("pdf-tools-impose-orphan.upload");
        let unrelated = staging_dir.join("keep.txt");
        std::fs::write(&orphan, b"orphan").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        let initialized = initialize_impose_staging(staging_dir.parent().unwrap()).unwrap();

        assert_eq!(initialized, staging_dir);
        assert!(!orphan.exists());
        assert!(unrelated.exists());
        std::fs::remove_dir_all(staging_dir.parent().unwrap()).unwrap();
    }

    fn sample_image_bytes(format: image::ImageFormat) -> Vec<u8> {
        let image = image::ImageBuffer::from_fn(2, 2, |_, _| image::Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), format)
            .unwrap();
        bytes
    }

    #[test]
    fn safe_display_name_removes_directories_and_unsafe_characters() {
        assert_eq!(safe_display_name("../foo.pdf"), "foo.pdf");
        assert_eq!(safe_display_name(r"C:\temp\foo.pdf"), "foo.pdf");
        assert_eq!(safe_display_name("my file (1).pdf"), "my file (1).pdf");
        assert_eq!(safe_display_name("café.pdf"), "café.pdf");
        assert_eq!(safe_display_name("..."), "upload");
        assert_eq!(safe_display_name("()"), "()");
    }

    #[test]
    fn file_growth_enforces_per_file_and_total_limits() {
        assert!(checked_file_growth(MAX_MULTIPART_FILE_BYTES, 0, 1).is_err());
        assert!(checked_file_growth(0, MAX_MULTIPART_TOTAL_FILE_BYTES, 1).is_err());
        assert_eq!(checked_file_growth(10, 20, 5).unwrap(), (15, 25));
    }

    #[test]
    fn impose_upload_growth_enforces_aggregate_limit_with_checked_arithmetic() {
        assert_eq!(checked_impose_upload_growth(10, 5, 15).unwrap(), 15);
        assert!(checked_impose_upload_growth(15, 1, 15).is_err());
        assert!(checked_impose_upload_growth(usize::MAX, 1, usize::MAX).is_err());
    }

    #[test]
    fn require_files_rejects_empty_uploads() {
        assert_eq!(
            require_files(&[]).unwrap_err().to_string(),
            "expected at least one file"
        );
    }

    #[test]
    fn pdf_validation_reports_offending_filename() {
        let error = require_pdfs(
            &[file("notes.txt", b"not a pdf")],
            "target=png/jpeg requires PDF input",
        )
        .unwrap_err();

        assert!(error.to_string().contains("notes.txt"));
    }

    #[test]
    fn image_validation_reports_offending_filename() {
        let error = require_images(
            &[file("notes.txt", b"not an image")],
            "target=pdf requires PNG or JPEG input",
        )
        .unwrap_err();

        assert!(error.to_string().contains("notes.txt"));
    }

    #[test]
    fn require_at_least_pdfs_checks_count_before_type() {
        let error =
            require_at_least_pdfs(&[file("notes.txt", b"not a pdf")], 2, "need two").unwrap_err();

        assert_eq!(error.to_string(), "need two");
    }

    #[test]
    fn require_one_pdf_rejects_multiple_files_and_invalid_type() {
        let multiple = FormData {
            files: vec![file("one.pdf", b"%PDF-one"), file("two.pdf", b"%PDF-two")],
            fields: Vec::new(),
        };
        assert_eq!(
            require_one_pdf(multiple, "split requires a PDF input")
                .unwrap_err()
                .to_string(),
            "expected exactly one file"
        );

        let invalid = FormData {
            files: vec![file("notes.txt", b"not a pdf")],
            fields: Vec::new(),
        };
        let error = require_one_pdf(invalid, "split requires a PDF input").unwrap_err();
        assert!(error.to_string().contains("notes.txt"));
    }

    #[test]
    fn supported_image_detection_accepts_png_and_jpeg_only() {
        assert!(is_supported_image(&sample_image_bytes(
            image::ImageFormat::Png
        )));
        assert!(is_supported_image(&sample_image_bytes(
            image::ImageFormat::Jpeg
        )));
        assert!(!is_supported_image(b"plain text"));
    }

    #[test]
    fn pdf_detection_uses_magic_prefix() {
        assert!(is_pdf(b"%PDF-1.7\n"));
        assert!(!is_pdf(b"not a pdf"));
    }
}
