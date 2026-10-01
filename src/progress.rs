use std::sync::Arc;

use crate::{AppError, AppResult};

/// Cooperative progress callback.
///
/// The callback returns `true` when processing should stop.
pub(crate) type ProgressCallback = Arc<dyn Fn(u8, String) -> bool + Send + Sync>;

pub(crate) fn report(
    progress: &Option<ProgressCallback>,
    percent: u8,
    stage: impl Into<String>,
) -> AppResult<()> {
    if let Some(progress) = progress {
        if progress(percent, stage.into()) {
            return Err(AppError::Cancelled("operation was cancelled".to_string()));
        }
    }
    Ok(())
}

/// Maps a producer's 0–100 progress into one slice of a larger operation.
pub(crate) fn mapped(
    progress: &Option<ProgressCallback>,
    start: u8,
    end: u8,
) -> Option<ProgressCallback> {
    let progress = progress.clone()?;
    Some(Arc::new(move |percent, stage| {
        progress(map_percent(percent, start, end), stage)
    }))
}

fn map_percent(percent: u8, start: u8, end: u8) -> u8 {
    let start = start.min(end);
    let span = usize::from(end.saturating_sub(start));
    start.saturating_add(((usize::from(percent.min(100)) * span) / 100) as u8)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn cancelled_progress_stops_cooperative_work() {
        let progress: ProgressCallback = Arc::new(|_, _| true);
        let error = report(&Some(progress), 50, "Rendering page").unwrap_err();
        assert_eq!(error.to_string(), "operation was cancelled");
        assert!(matches!(error, AppError::Cancelled(_)));
    }

    #[test]
    fn report_allows_progress_without_cancellation() {
        let progress: ProgressCallback = Arc::new(|_, _| false);
        assert!(report(&Some(progress), 50, "Working").is_ok());
        assert!(report(&None, 50, "Working").is_ok());
    }

    #[test]
    fn mapped_progress_stays_inside_its_operation_slice() {
        assert_eq!(map_percent(0, 63, 75), 63);
        assert_eq!(map_percent(50, 63, 75), 69);
        assert_eq!(map_percent(100, 63, 75), 75);
        assert_eq!(map_percent(255, 63, 75), 75);
    }
}
