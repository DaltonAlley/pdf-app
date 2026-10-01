use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use pdfium_render::prelude::Pdfium;
use rayon::ThreadPool;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
    adapters::{
        initialize_impose_staging, ImposeUploadLimits, DEFAULT_IMPOSE_UPLOAD_TIMEOUT_SECONDS,
        DEFAULT_MAX_IMPOSE_UPLOAD_BYTES,
    },
    error::{AppError, AppResult},
    imposition::{GangUpExportHistoryStore, PresetStore, RecentGangUpStore, SourceSessionStore},
    jobs::JobStore,
};

const MAX_CPU_PERMITS: usize = 64;
/// Largest whole-megabyte value that can be represented as bytes.
pub const MAX_CONFIGURED_MEGABYTES: usize = usize::MAX / (1024 * 1024);

static PDFIUM_ACCESS: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Clone)]
/// Shared runtime resources and configured safety limits.
pub struct AppState {
    cpu: Arc<Semaphore>,
    cpu_pool: Arc<ThreadPool>,
    // Pdfium's process-global C API is not thread safe. The crate's
    // `thread_safe` feature makes wrapper types Send/Sync, but Pdfium still
    // requires callers to serialize operations.
    pdfium_access: Arc<Semaphore>,
    upload_admission: Option<Arc<Semaphore>>,
    admission: Option<Arc<Semaphore>>,
    pdfium: Arc<Pdfium>,
    max_render_pages: usize,
    max_download_bytes: Option<usize>,
    impose_upload_limits: ImposeUploadLimits,
    preset_store: PresetStore,
    recent_gang_up_store: RecentGangUpStore,
    gang_up_export_history_store: GangUpExportHistoryStore,
    source_session_store: SourceSessionStore,
    impose_staging_dir: Arc<PathBuf>,
    pub(crate) jobs: JobStore,
}

/// Admission permit retained for the lifetime of one operation.
///
/// `None` represents an installation configured without an operation limit.
pub(super) type OperationPermit = Option<Arc<OwnedSemaphorePermit>>;

impl AppState {
    /// Builds runtime state from environment configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration is invalid or the PDFium bindings
    /// or CPU worker pool cannot be initialized.
    pub fn from_env() -> AppResult<Self> {
        let host_parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let default_permits = host_parallelism.clamp(1, 4);
        let cpu_permits = bounded_setting(
            "PDF_TOOLS_CPU_PERMITS",
            read_env_usize("PDF_TOOLS_CPU_PERMITS", default_permits)?,
            1,
            MAX_CPU_PERMITS,
        )?;
        let max_render_pages = read_optional_env_usize("MAX_RENDER_PAGES")?
            .map(|value| bounded_setting("MAX_RENDER_PAGES", value, 1, usize::MAX))
            .transpose()?
            .unwrap_or(usize::MAX);
        let max_download_bytes = read_optional_env_usize("MAX_DOWNLOAD_MB")?
            .map(|value| bounded_setting("MAX_DOWNLOAD_MB", value, 1, MAX_CONFIGURED_MEGABYTES))
            .transpose()?
            .map(|value| megabytes_to_bytes("MAX_DOWNLOAD_MB", value))
            .transpose()?;
        let max_impose_upload_bytes = read_optional_env_usize("PDF_TOOLS_MAX_IMPOSE_UPLOAD_MB")?
            .map(|value| {
                bounded_setting(
                    "PDF_TOOLS_MAX_IMPOSE_UPLOAD_MB",
                    value,
                    1,
                    MAX_CONFIGURED_MEGABYTES,
                )
            })
            .transpose()?
            .map(|value| megabytes_to_bytes("PDF_TOOLS_MAX_IMPOSE_UPLOAD_MB", value))
            .transpose()?
            .unwrap_or(DEFAULT_MAX_IMPOSE_UPLOAD_BYTES);
        let impose_upload_timeout_seconds = bounded_setting(
            "PDF_TOOLS_IMPOSE_UPLOAD_TIMEOUT_SECONDS",
            read_env_usize(
                "PDF_TOOLS_IMPOSE_UPLOAD_TIMEOUT_SECONDS",
                DEFAULT_IMPOSE_UPLOAD_TIMEOUT_SECONDS,
            )?,
            1,
            3_600,
        )?;
        let max_active_operations = Some(
            read_optional_env_usize("PDF_TOOLS_MAX_ACTIVE_OPERATIONS")?.unwrap_or(cpu_permits),
        )
        .map(|value| {
            bounded_setting(
                "PDF_TOOLS_MAX_ACTIVE_OPERATIONS",
                value,
                1,
                Semaphore::MAX_PERMITS,
            )
        })
        .transpose()?;
        let data_dir = read_data_dir()?;
        let impose_staging_dir = Arc::new(initialize_impose_staging(&data_dir)?);
        let pdfium = Arc::new(bind_pdfium()?);
        let cpu_pool = Arc::new(build_cpu_pool(cpu_permits)?);

        Ok(Self {
            cpu: Arc::new(Semaphore::new(cpu_permits)),
            cpu_pool,
            pdfium_access: process_pdfium_access(),
            upload_admission: max_active_operations.map(|limit| Arc::new(Semaphore::new(limit))),
            admission: max_active_operations.map(|limit| Arc::new(Semaphore::new(limit))),
            pdfium,
            max_render_pages,
            max_download_bytes,
            impose_upload_limits: ImposeUploadLimits::new(
                max_impose_upload_bytes,
                std::time::Duration::from_secs(impose_upload_timeout_seconds as u64),
            ),
            preset_store: PresetStore::new(data_dir.clone()),
            recent_gang_up_store: RecentGangUpStore::new(data_dir.clone()),
            gang_up_export_history_store: GangUpExportHistoryStore::new(data_dir.clone()),
            source_session_store: SourceSessionStore::new(data_dir),
            impose_staging_dir,
            jobs: JobStore::new(max_active_operations),
        })
    }

    /// Runs CPU-bound work on the bounded Rayon pool.
    ///
    /// # Errors
    ///
    /// Returns the work error or an error if the worker task or semaphore is
    /// unavailable.
    pub(super) async fn run_cpu<T, F>(
        &self,
        operation_permit: OperationPermit,
        work: F,
    ) -> AppResult<T>
    where
        T: Send + 'static,
        F: FnOnce() -> AppResult<T> + Send + 'static,
    {
        let permit = self.cpu.clone().acquire_owned().await?;
        let cpu_pool = self.cpu_pool.clone();
        tokio::task::spawn_blocking(move || {
            let _operation_permit = operation_permit;
            let _permit = permit;
            cpu_pool.install(work)
        })
        .await
        .map_err(AppError::from)?
    }

    /// Runs PDFium work while holding the process-wide serialization permit.
    ///
    /// # Errors
    ///
    /// Returns the work error or an error if a worker or permit is unavailable.
    pub(super) async fn run_pdfium<T, F>(
        &self,
        operation_permit: OperationPermit,
        work: F,
    ) -> AppResult<T>
    where
        T: Send + 'static,
        F: FnOnce() -> AppResult<T> + Send + 'static,
    {
        // Queue on the single process-wide PDFium permit before reserving CPU
        // capacity. Otherwise a burst of serialized PDFium requests can occupy
        // every CPU permit while all but one wait for PDFium, starving image
        // encoding, layout, and other CPU-only work. The selected PDFium job
        // may briefly hold this permit while it waits for a CPU worker; that is
        // the bounded tradeoff that preserves capacity for unrelated work.
        let pdfium_permit = self.pdfium_access.clone().acquire_owned().await?;
        self.run_cpu(operation_permit, move || {
            let _pdfium_permit = pdfium_permit;
            work()
        })
        .await
    }

    /// Attempts to reserve operation capacity without waiting.
    ///
    /// # Errors
    ///
    /// Returns an unavailable error when all configured permits are in use.
    #[doc(hidden)]
    pub fn admit_operation(&self) -> AppResult<Option<Arc<OwnedSemaphorePermit>>> {
        self.admission
            .as_ref()
            .map(|admission| {
                admission
                    .clone()
                    .try_acquire_owned()
                    .map(Arc::new)
                    .map_err(|_| AppError::unavailable("server is busy; try again later"))
            })
            .transpose()
    }

    /// Attempts to reserve multipart-upload capacity without waiting.
    ///
    /// # Errors
    ///
    /// Returns an unavailable error when all configured permits are in use.
    #[doc(hidden)]
    pub fn admit_upload(&self) -> AppResult<Option<OwnedSemaphorePermit>> {
        self.upload_admission
            .as_ref()
            .map(|admission| {
                admission
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| AppError::unavailable("server is busy; try again later"))
            })
            .transpose()
    }

    /// Returns the maximum number of pages accepted by one render request.
    pub(super) fn max_render_pages(&self) -> usize {
        self.max_render_pages
    }

    /// Returns the optional maximum generated-download size in bytes.
    pub(super) fn max_download_bytes(&self) -> Option<usize> {
        self.max_download_bytes
    }

    pub(crate) fn impose_upload_limits(&self) -> ImposeUploadLimits {
        self.impose_upload_limits
    }

    /// Returns the shared PDFium instance.
    #[doc(hidden)]
    pub fn pdfium(&self) -> Arc<Pdfium> {
        self.pdfium.clone()
    }

    /// Creates a pending job for black-box HTTP boundary tests.
    ///
    /// # Errors
    ///
    /// Returns an error when job capacity is exhausted or the job store is unavailable.
    #[doc(hidden)]
    pub fn create_test_job(&self) -> AppResult<String> {
        self.jobs.create()
    }

    /// Fails a pending job for black-box HTTP boundary tests.
    ///
    /// # Errors
    ///
    /// Returns an error when the job store is unavailable.
    #[doc(hidden)]
    pub fn fail_test_job(&self, id: &str, message: impl Into<String>) -> AppResult<()> {
        self.jobs.fail(id, message).map(|_| ())
    }

    pub(crate) fn preset_store(&self) -> PresetStore {
        self.preset_store.clone()
    }

    pub(crate) fn recent_gang_up_store(&self) -> RecentGangUpStore {
        self.recent_gang_up_store.clone()
    }

    pub(crate) fn gang_up_export_history_store(&self) -> GangUpExportHistoryStore {
        self.gang_up_export_history_store.clone()
    }

    pub(crate) fn source_session_store(&self) -> SourceSessionStore {
        self.source_session_store.clone()
    }

    pub(crate) fn impose_staging_dir(&self) -> Arc<PathBuf> {
        self.impose_staging_dir.clone()
    }

    /// Returns the managed impose staging directory for black-box tests.
    #[doc(hidden)]
    pub fn impose_staging_path(&self) -> &std::path::Path {
        self.impose_staging_dir.as_ref()
    }

    #[doc(hidden)]
    pub fn for_tests(pdfium: Arc<Pdfium>) -> AppResult<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(1);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let data_dir = std::env::temp_dir().join(format!(
            "pdf-tools-test-data-{}-{timestamp}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        Self::for_tests_with_data_dir(pdfium, data_dir)
    }

    #[doc(hidden)]
    pub fn for_tests_with_download_limit(
        pdfium: Arc<Pdfium>,
        max_download_bytes: Option<usize>,
    ) -> AppResult<Self> {
        let mut state = Self::for_tests(pdfium)?;
        state.max_download_bytes = max_download_bytes;
        Ok(state)
    }

    /// Builds test state with a small canonical impose-upload budget.
    ///
    /// # Errors
    ///
    /// Returns an error when test state cannot be initialized.
    #[doc(hidden)]
    pub fn for_tests_with_impose_upload_limit(
        pdfium: Arc<Pdfium>,
        max_total_bytes: usize,
    ) -> AppResult<Self> {
        let mut state = Self::for_tests(pdfium)?;
        state.impose_upload_limits = ImposeUploadLimits::new(
            max_total_bytes,
            std::time::Duration::from_secs(DEFAULT_IMPOSE_UPLOAD_TIMEOUT_SECONDS as u64),
        );
        Ok(state)
    }

    #[doc(hidden)]
    pub fn for_tests_with_data_dir(pdfium: Arc<Pdfium>, data_dir: PathBuf) -> AppResult<Self> {
        Self::for_tests_with_data_dir_and_download_limit(pdfium, data_dir, None)
    }

    #[doc(hidden)]
    pub fn for_tests_with_data_dir_and_download_limit(
        pdfium: Arc<Pdfium>,
        data_dir: PathBuf,
        max_download_bytes: Option<usize>,
    ) -> AppResult<Self> {
        let impose_staging_dir = Arc::new(initialize_impose_staging(&data_dir)?);
        Ok(Self {
            cpu: Arc::new(Semaphore::new(1)),
            cpu_pool: Arc::new(build_cpu_pool(1)?),
            pdfium_access: process_pdfium_access(),
            upload_admission: Some(Arc::new(Semaphore::new(1))),
            admission: Some(Arc::new(Semaphore::new(1))),
            pdfium,
            max_render_pages: 100,
            max_download_bytes,
            impose_upload_limits: ImposeUploadLimits::new(
                DEFAULT_MAX_IMPOSE_UPLOAD_BYTES,
                std::time::Duration::from_secs(DEFAULT_IMPOSE_UPLOAD_TIMEOUT_SECONDS as u64),
            ),
            preset_store: PresetStore::new(data_dir.clone()),
            recent_gang_up_store: RecentGangUpStore::new(data_dir.clone()),
            gang_up_export_history_store: GangUpExportHistoryStore::new(data_dir.clone()),
            source_session_store: SourceSessionStore::new(data_dir),
            impose_staging_dir,
            jobs: JobStore::new(Some(1)),
        })
    }
}

fn build_cpu_pool(threads: usize) -> AppResult<ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("pdf-cpu-{index}"))
        .build()
        .map_err(|error| AppError::internal_cause("could not create CPU worker pool", error))
}

fn process_pdfium_access() -> Arc<Semaphore> {
    PDFIUM_ACCESS
        .get_or_init(|| Arc::new(Semaphore::new(1)))
        .clone()
}

/// Converts a whole-megabyte configuration value to bytes.
///
/// # Errors
///
/// Returns an internal configuration error when the byte count cannot be
/// represented by `usize`.
pub fn megabytes_to_bytes(name: &str, megabytes: usize) -> AppResult<usize> {
    megabytes
        .checked_mul(1024 * 1024)
        .ok_or_else(|| AppError::Internal(format!("environment variable {name} is too large")))
}

fn read_data_dir() -> AppResult<PathBuf> {
    match std::env::var("PDF_TOOLS_DATA_DIR") {
        Ok(value) if !value.trim().is_empty() => Ok(PathBuf::from(value.trim())),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(PathBuf::from("data")),
        Err(error @ std::env::VarError::NotUnicode(_)) => Err(AppError::internal_cause(
            "environment variable PDF_TOOLS_DATA_DIR is not valid Unicode",
            error,
        )),
    }
}

/// Reads an unsigned integer environment variable or returns `default`.
///
/// # Errors
///
/// Returns an error when the environment value is not valid Unicode or is not
/// an unsigned integer.
pub fn read_env_usize(name: &str, default: usize) -> AppResult<usize> {
    read_env(name, default)
}

/// Reads an optional unsigned integer environment variable.
///
/// Missing and empty values produce `None`.
///
/// # Errors
///
/// Returns an error when the environment value is not valid Unicode or is not
/// an unsigned integer.
pub fn read_optional_env_usize(name: &str) -> AppResult<Option<usize>> {
    match std::env::var(name) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => value.trim().parse().map(Some).map_err(|error| {
            AppError::internal_cause(
                format!("environment variable {name} has an invalid value"),
                error,
            )
        }),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error @ std::env::VarError::NotUnicode(_)) => Err(AppError::internal_cause(
            format!("environment variable {name} is not valid Unicode"),
            error,
        )),
    }
}

/// Reads a 16-bit unsigned integer environment variable or returns `default`.
///
/// # Errors
///
/// Returns an error when the environment value is not valid Unicode or is not
/// a 16-bit unsigned integer.
pub fn read_env_u16(name: &str, default: u16) -> AppResult<u16> {
    read_env(name, default)
}

fn read_env<T>(name: &str, default: T) -> AppResult<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match std::env::var(name) {
        Ok(value) => value.parse().map_err(|error| {
            AppError::internal_cause(
                format!("environment variable {name} has an invalid value"),
                error,
            )
        }),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error @ std::env::VarError::NotUnicode(_)) => Err(AppError::internal_cause(
            format!("environment variable {name} is not valid Unicode"),
            error,
        )),
    }
}

/// Validates that a configured value lies in an inclusive range.
///
/// # Errors
///
/// Returns an internal configuration error when `value` is outside
/// `min..=max`.
pub fn bounded_setting(name: &str, value: usize, min: usize, max: usize) -> AppResult<usize> {
    if !(min..=max).contains(&value) {
        return Err(AppError::Internal(format!(
            "environment variable {name} must be between {min} and {max}"
        )));
    }
    Ok(value)
}

fn bind_pdfium() -> AppResult<Pdfium> {
    let bindings = match std::env::var("PDF_TOOLS_PDFIUM_PATH") {
        Ok(path) if !path.trim().is_empty() => Pdfium::bind_to_library(path.trim())
            .map_err(|err| AppError::external_tool_cause("could not load PDFium", err))?,
        _ => Pdfium::bind_to_system_library()
            .map_err(|err| AppError::external_tool_cause("could not load PDFium", err))?,
    };

    Ok(Pdfium::new(bindings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_settings_reject_values_that_can_overflow_semaphores() {
        assert!(bounded_setting("TEST", 0, 1, 64).is_err());
        assert!(bounded_setting("TEST", usize::MAX, 1, 64).is_err());
        assert_eq!(bounded_setting("TEST", 64, 1, 64).unwrap(), 64);
    }

    #[test]
    fn megabytes_convert_to_bytes_without_saturation() {
        assert_eq!(megabytes_to_bytes("TEST", 256).unwrap(), 268_435_456);
        assert!(megabytes_to_bytes("TEST", usize::MAX).is_err());
    }

    #[test]
    fn cpu_pool_uses_the_configured_parallelism() {
        let pool = build_cpu_pool(3).unwrap();
        assert_eq!(pool.current_num_threads(), 3);
    }

    #[tokio::test]
    async fn single_pdfium_permit_serializes_waiters() {
        let pdfium = Arc::new(Semaphore::new(1));
        let first_permit = pdfium.clone().acquire_owned().await.unwrap();
        let waiting_pdfium = pdfium.clone();
        let waiter = tokio::spawn(async move { waiting_pdfium.acquire_owned().await });
        tokio::task::yield_now().await;

        assert!(!waiter.is_finished());
        drop(first_permit);
        assert!(waiter.await.unwrap().is_ok());
    }
}
