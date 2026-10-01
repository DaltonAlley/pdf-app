use std::{
    error::Error,
    fmt, io,
    sync::{Mutex, MutexGuard},
};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// Result type returned by application operations.
pub type AppResult<T> = Result<T, AppError>;

pub(crate) fn lock_mutex<'a, T>(
    mutex: &'a Mutex<T>,
    resource: &'static str,
) -> AppResult<MutexGuard<'a, T>> {
    mutex
        .lock()
        .map_err(|_| AppError::Internal(format!("{resource} is unavailable after a worker panic")))
}

#[derive(Debug)]
/// Error categories mapped to stable HTTP status codes and client messages.
///
/// Client-input and capacity errors expose their stored message. Dependency and
/// internal failures retain diagnostic context for logs while returning a stable,
/// non-sensitive message to HTTP clients. Cancellation maps to HTTP conflict.
pub enum AppError {
    /// The request was syntactically valid HTTP but contained invalid input.
    BadRequest(String),
    /// Invalid request data was rejected with an underlying parser cause.
    BadRequestCause {
        /// Stable message safe to return to the client.
        message: String,
        /// Original request parsing error.
        source: Box<dyn Error + Send + Sync>,
    },
    /// Input or generated output exceeded a configured safety limit.
    PayloadTooLarge(String),
    /// Capacity was temporarily unavailable.
    Unavailable(String),
    /// Server-side storage could not accept more data.
    InsufficientStorageCause {
        /// Stable message safe to return to the client.
        message: String,
        /// Original filesystem error retained for diagnostics.
        source: Box<dyn Error + Send + Sync>,
    },
    /// The request did not complete within its bounded time.
    RequestTimeout(String),
    /// The operation stopped in response to an explicit cancellation request.
    Cancelled(String),
    /// A PDF processing dependency failed.
    ExternalTool(String),
    /// A PDF processing dependency failed with an underlying cause.
    ExternalToolCause {
        /// Context describing the failed dependency operation.
        message: String,
        /// Original error returned by the dependency.
        source: Box<dyn Error + Send + Sync>,
    },
    /// An unexpected server-side operation failed.
    Internal(String),
    /// An unexpected server-side operation failed with an underlying cause.
    InternalCause {
        /// Context describing the failed operation.
        message: String,
        /// Original error returned by the dependency or system boundary.
        source: Box<dyn Error + Send + Sync>,
    },
}

impl AppError {
    /// Creates an invalid-input error.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    pub(crate) fn bad_request_cause(
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self::BadRequestCause {
            message: message.into(),
            source: Box::new(source),
        }
    }

    /// Creates a PDF processing dependency error.
    pub fn external_tool(message: impl Into<String>) -> Self {
        Self::ExternalTool(message.into())
    }

    pub(crate) fn external_tool_cause(
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self::ExternalToolCause {
            message: message.into(),
            source: Box::new(source),
        }
    }

    /// Creates a configured-limit error.
    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::PayloadTooLarge(message.into())
    }

    /// Creates a temporary-capacity error.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    pub(crate) fn storage_cause(message: impl Into<String>, source: io::Error) -> Self {
        if matches!(
            source.kind(),
            io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
        ) {
            Self::InsufficientStorageCause {
                message: "server storage is full; free space and try again".to_string(),
                source: Box::new(source),
            }
        } else {
            Self::internal_cause(message, source)
        }
    }

    pub(crate) fn internal_cause(
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self::InternalCause {
            message: message.into(),
            source: Box::new(source),
        }
    }

    pub(crate) fn client_message(&self) -> &str {
        match self {
            Self::BadRequest(message)
            | Self::PayloadTooLarge(message)
            | Self::Unavailable(message)
            | Self::RequestTimeout(message)
            | Self::Cancelled(message)
            | Self::BadRequestCause { message, .. }
            | Self::InsufficientStorageCause { message, .. } => message,
            Self::ExternalTool(_) | Self::ExternalToolCause { .. } => "PDF processing failed",
            Self::Internal(_) | Self::InternalCause { .. } => "internal server error",
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::BadRequest(_) | AppError::BadRequestCause { .. } => StatusCode::BAD_REQUEST,
            AppError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::InsufficientStorageCause { .. } => StatusCode::INSUFFICIENT_STORAGE,
            AppError::RequestTimeout(_) => StatusCode::REQUEST_TIMEOUT,
            AppError::Cancelled(_) => StatusCode::CONFLICT,
            AppError::ExternalTool(_) | AppError::ExternalToolCause { .. } => {
                StatusCode::BAD_GATEWAY
            }
            AppError::Internal(_) | AppError::InternalCause { .. } => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };

        if status.is_server_error() {
            tracing::error!(%status, error = %self, "request failed");
        }

        (status, self.client_message().to_owned()).into_response()
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::BadRequest(message)
            | AppError::PayloadTooLarge(message)
            | AppError::Unavailable(message)
            | AppError::RequestTimeout(message)
            | AppError::Cancelled(message)
            | AppError::ExternalTool(message)
            | AppError::Internal(message) => f.write_str(message),
            AppError::BadRequestCause { message, source }
            | AppError::InsufficientStorageCause { message, source }
            | AppError::ExternalToolCause { message, source }
            | AppError::InternalCause { message, source } => write!(f, "{message}: {source}"),
        }
    }
}

impl Error for AppError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::BadRequestCause { source, .. }
            | Self::InsufficientStorageCause { source, .. }
            | Self::ExternalToolCause { source, .. }
            | Self::InternalCause { source, .. } => Some(source.as_ref()),
            Self::BadRequest(_)
            | Self::PayloadTooLarge(_)
            | Self::Unavailable(_)
            | Self::RequestTimeout(_)
            | Self::Cancelled(_)
            | Self::ExternalTool(_)
            | Self::Internal(_) => None,
        }
    }
}

impl From<io::Error> for AppError {
    fn from(value: io::Error) -> Self {
        Self::internal_cause("I/O operation failed", value)
    }
}

impl From<axum::extract::multipart::MultipartError> for AppError {
    fn from(value: axum::extract::multipart::MultipartError) -> Self {
        if value.status() == StatusCode::PAYLOAD_TOO_LARGE {
            Self::PayloadTooLarge("upload is larger than the configured limit".to_string())
        } else {
            Self::bad_request_cause("could not read multipart upload", value)
        }
    }
}

impl From<tokio::task::JoinError> for AppError {
    fn from(value: tokio::task::JoinError) -> Self {
        Self::internal_cause("worker task failed", value)
    }
}

impl From<tokio::sync::AcquireError> for AppError {
    fn from(value: tokio::sync::AcquireError) -> Self {
        Self::internal_cause("worker permit became unavailable", value)
    }
}

impl From<zip::result::ZipError> for AppError {
    fn from(value: zip::result::ZipError) -> Self {
        Self::internal_cause("ZIP operation failed", value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn poisoned_locks_become_contextual_errors() {
        let mutex = Arc::new(Mutex::new(()));
        let worker_mutex = mutex.clone();
        let _ = std::thread::spawn(move || {
            let _guard = worker_mutex.lock().unwrap();
            panic!("poison test mutex");
        })
        .join();

        let error = lock_mutex(&mutex, "test store").unwrap_err();
        assert_eq!(
            error.to_string(),
            "test store is unavailable after a worker panic"
        );
    }

    #[test]
    fn boundary_errors_preserve_their_original_source() {
        let error = AppError::internal_cause(
            "could not read metadata",
            io::Error::new(io::ErrorKind::PermissionDenied, "access denied"),
        );

        assert_eq!(error.to_string(), "could not read metadata: access denied");
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("access denied")
        );
    }

    #[test]
    fn full_storage_has_an_actionable_client_error() {
        for kind in [io::ErrorKind::StorageFull, io::ErrorKind::QuotaExceeded] {
            let error = AppError::storage_cause("could not write upload", io::Error::from(kind));

            assert!(matches!(error, AppError::InsufficientStorageCause { .. }));
            assert_eq!(
                error.client_message(),
                "server storage is full; free space and try again"
            );
            assert!(error.source().is_some());
        }
    }

    #[test]
    fn caused_bad_requests_have_stable_client_messages() {
        let error = AppError::BadRequestCause {
            message: "invalid request body".to_string(),
            source: Box::new(io::Error::new(io::ErrorKind::InvalidData, "parser detail")),
        };

        assert_eq!(error.client_message(), "invalid request body");
        assert_eq!(error.to_string(), "invalid request body: parser detail");
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("parser detail")
        );
    }
}
