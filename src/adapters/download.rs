use axum::{
    http::{
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE},
        HeaderValue,
    },
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use std::io::{self, Write};

use crate::{AppError, AppResult};

/// A generated file and the metadata needed to return it as a download.
pub struct FileDownload {
    /// MIME type sent in the `Content-Type` response header.
    pub content_type: String,
    /// Suggested basename for the downloaded file.
    pub filename: String,
    /// Complete generated file contents.
    pub bytes: Bytes,
}

impl FileDownload {
    pub(crate) fn new(
        content_type: impl Into<String>,
        filename: impl Into<String>,
        bytes: impl Into<Bytes>,
    ) -> Self {
        Self {
            content_type: content_type.into(),
            filename: filename.into(),
            bytes: bytes.into(),
        }
    }

    pub(crate) fn bounded(
        content_type: impl Into<String>,
        filename: impl Into<String>,
        bytes: Vec<u8>,
        max_bytes: Option<usize>,
    ) -> AppResult<Self> {
        validate_download_size(bytes.len(), max_bytes)?;
        Ok(Self::new(content_type, filename, bytes))
    }

    /// Converts the file into a no-cache attachment response.
    ///
    /// Unsafe filename characters are replaced before the response headers are
    /// constructed. Invalid MIME types fall back to `application/octet-stream`.
    pub fn into_response(self) -> Response {
        let filename = safe_download_name(&self.filename);
        let content_disposition =
            HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"download\""));
        (
            [
                (
                    CONTENT_TYPE,
                    HeaderValue::from_str(&self.content_type)
                        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
                ),
                (CONTENT_DISPOSITION, content_disposition),
                (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            ],
            self.bytes,
        )
            .into_response()
    }
}

pub(crate) fn validate_download_size(size: usize, max_bytes: Option<usize>) -> AppResult<()> {
    if let Some(max_bytes) = max_bytes.filter(|max_bytes| size > *max_bytes) {
        return Err(download_limit_error(max_bytes));
    }
    Ok(())
}

fn download_limit_error(max_bytes: usize) -> AppError {
    AppError::payload_too_large(format!(
        "generated download exceeds the configured {max_bytes}-byte limit"
    ))
}

pub(crate) struct BoundedBytes {
    bytes: Vec<u8>,
    max_bytes: Option<usize>,
    limit_exceeded: bool,
}

impl BoundedBytes {
    pub(crate) fn new(capacity: usize, max_bytes: Option<usize>) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.map_or(capacity, |max| capacity.min(max))),
            max_bytes,
            limit_exceeded: false,
        }
    }

    pub(crate) fn finish(
        self,
        result: Result<(), impl std::error::Error + Send + Sync + 'static>,
        context: &str,
    ) -> AppResult<Vec<u8>> {
        if self.limit_exceeded {
            return Err(download_limit_error(self.max_bytes.unwrap_or(0)));
        }
        result.map_err(|error| AppError::internal_cause(context, error))?;
        Ok(self.bytes)
    }
}

impl Write for BoundedBytes {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let new_len =
            self.bytes.len().checked_add(buffer.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::FileTooLarge, "download is too large")
            })?;
        if self.max_bytes.is_some_and(|max| new_len > max) {
            self.limit_exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "download exceeded configured limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn safe_download_name(value: &str) -> String {
    let basename = value.rsplit(['/', '\\']).next().unwrap_or("download");
    let mut name = basename
        .chars()
        .take(120)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    while name.starts_with('.') {
        name.remove(0);
    }
    if name.is_empty() {
        name.push_str("download");
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_names_cannot_inject_headers_or_paths() {
        assert_eq!(safe_download_name("../report.pdf"), "report.pdf");
        assert_eq!(safe_download_name("bad\"\r\nname.pdf"), "bad___name.pdf");
        assert_eq!(safe_download_name("..."), "download");
        assert_eq!(safe_download_name(""), "download");
    }

    #[test]
    fn downloads_are_unlimited_unless_a_limit_is_configured() {
        let above_old_hard_limit = 257 * 1024 * 1024;

        assert!(validate_download_size(above_old_hard_limit, None).is_ok());
        assert!(validate_download_size(above_old_hard_limit, Some(256 * 1024 * 1024)).is_err());
    }

    #[test]
    fn download_limit_messages_preserve_exact_byte_limits() {
        let error = validate_download_size(513, Some(512)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "generated download exceeds the configured 512-byte limit"
        );
    }

    #[test]
    fn bounded_bytes_rejects_writes_before_growing_past_the_limit() {
        let mut writer = BoundedBytes::new(16, Some(4));
        let result = writer.write_all(b"12345");
        let error = writer.finish(result, "write failed").unwrap_err();

        assert!(matches!(error, AppError::PayloadTooLarge(_)));
    }
}
