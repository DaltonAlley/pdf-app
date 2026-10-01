//! Print-imposition domain.
//!
//! HTTP orchestration lives in `web`; this root exposes only the imposition
//! operations and persistence handles needed by that composition layer.

mod export;
mod export_history_store;
mod geometry;
mod intake;
mod layout;
mod mixed;
mod model;
mod pdf;
mod persistence;
mod preset_store;
mod recent_store;
mod source_store;
mod validation;

pub(crate) use export::{
    export_clean_imposed_pdf, export_clean_imposed_pdf_path, flatten_annotations_for_export,
    flatten_annotations_path, resolve_request_source_path,
};
pub(crate) use export_history_store::GangUpExportHistoryStore;
pub(crate) use intake::{
    normalize_staged_imposition_geometry, ArtworkPageIdentity, OrientationNormalization,
    StagedArtworkPdf,
};
pub(crate) use layout::generate_layout;
pub(crate) use model::{
    GangUpExportRecordInput, GangUpExportType, LayoutRequest, LayoutResult, PdfAnalysis,
    PresetInput, RecentGangUpJobInput, RecentGangUpLayoutSummary,
};
pub(crate) use pdf::{
    analyze_pdf, analyze_pdf_path_structural, analyze_pdf_structural, prepare_artwork_preview,
};
pub(crate) use preset_store::PresetStore;
pub(crate) use recent_store::RecentGangUpStore;
pub(crate) use source_store::{SourceSessionFile, SourceSessionStore};
pub(crate) use validation::validate_preset_input;
