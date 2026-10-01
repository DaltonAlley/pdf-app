//! Print-imposition domain with browser transport and Leptos presentation adapters.
#![cfg_attr(test, allow(unused_imports))]

mod model;
mod preview_scheduler;

// The nested adapters consume the pure domain through this module boundary.
#[cfg(test)]
use model::request_issue;
use model::{
    decode_preview_batch, desired_artwork_box, duplex_mirror_axes, gutter_bands,
    initial_preparation_percent, initial_preview_pages, initial_request, presented_preview_view,
    preview_artwork_image, preview_artwork_rect, preview_cache_window, preview_clip_rect,
    preview_failure_availability, preview_layout_identity, preview_loading_label,
    preview_navigation_label, preview_page_number, preview_page_numbers, preview_placement_key,
    preview_protected_pages, preview_source_image_rect, raster_artwork_box, request_from_analysis,
    request_issue_with_analysis, request_signature, request_with_manual_source_bleed,
    restore_request_from_analysis, select_layout_mode, set_all_repeat_quantities,
    set_imposition_mode_preserving_repeat_drafts, set_sides_preserving_repeat_drafts,
    should_remove_failed_preview_url, should_settle_preview_navigation, size_label,
    supports_duplex, update_repeat_quantity, validate_layout_result, validate_prepared_source,
    BleedOption, DuplexFlipEdge, ExportLifecycle, GangUpPreset, ImpositionMode,
    InitialPreparationPhase, LayoutLifecycle, LayoutMode, LayoutRequest, LayoutResult, PdfAnalysis,
    PiecePlacement, PreparationLifecycle, PreparedPdfSource, PresetInput, PreviewImageRect,
    PreviewLifecycle, PreviewRect, PreviewSide, PreviewViewIdentity, RepeatQuantityDrafts, Sides,
    SizeInches, MAX_PLACEMENTS,
};
#[cfg(target_arch = "wasm32")]
use preview_scheduler::{
    http_failure_kind, preview_cache_evictions, request_batches, BatchAttempt, PreviewCacheLimits,
    PreviewFailureKind, PreviewPlan, PreviewScheduler,
};

#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(target_arch = "wasm32")]
mod finished;
#[cfg(target_arch = "wasm32")]
mod presets;
#[cfg(target_arch = "wasm32")]
mod preview;
#[cfg(target_arch = "wasm32")]
mod view;

#[cfg(target_arch = "wasm32")]
pub(crate) use view::ImposeWorkspace;

#[cfg(test)]
mod contract_tests {
    use super::{initial_request, request_issue, request_signature};

    #[test]
    fn module_seam_builds_a_valid_initial_layout_request() {
        let request = initial_request();

        assert_eq!(request_issue(&request), None);
        assert!(request_signature(&request).is_ok());
    }
}
