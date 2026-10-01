use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SizeInches {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarginsInches {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GuttersInches {
    pub horizontal: f64,
    pub vertical: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Orientation {
    Portrait,
    Landscape,
    Square,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OrientationPreference {
    #[default]
    Auto,
    Portrait,
    Landscape,
    Upright,
    QuarterTurn,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum FinishedSizeMode {
    #[default]
    Common,
    Original,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ArtworkFitMode {
    Contain,
    Cover,
    /// Explicitly fit each axis independently into the finished cut frame.
    Stretch,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CropPosition {
    pub x: f64,
    pub y: f64,
}

impl Default for CropPosition {
    fn default() -> Self {
        Self { x: 0.5, y: 0.5 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtworkFit {
    pub mode: ArtworkFitMode,
    #[serde(default)]
    pub position: CropPosition,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourcePage {
    pub source_pdf_size: SizeInches,
    #[serde(default)]
    pub source_trim_box: Option<PdfBox>,
    #[serde(default)]
    pub preview_box: Option<PdfBox>,
    #[serde(default)]
    pub physical_size_assumed: bool,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub original_page_number: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PageOverride {
    pub page_number: usize,
    #[serde(default)]
    pub finished_cut_size: Option<SizeInches>,
    #[serde(default)]
    pub artwork_fit: Option<ArtworkFit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlanRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PagePlan {
    pub position_travel: CropPosition,
    pub page_number: usize,
    pub source_pdf_size: SizeInches,
    pub preview_box: Option<PdfBox>,
    pub finished_cut_size: SizeInches,
    pub bleed_amount: f64,
    pub artwork: PlanRect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Sides {
    Single,
    Double,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LayoutMode {
    Auto,
    MaxPieces,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum BleedOption {
    UseAsIs,
    ScaleToBleed,
    FitInside,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum BleedSource {
    None,
    Detected,
    Manual,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ImpositionMode {
    #[default]
    Repeat,
    Unique,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DuplexFlipEdge {
    LongEdge,
    ShortEdge,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PdfBox {
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BleedDetection {
    pub detected: bool,
    pub amount_per_side: f64,
    pub horizontal: f64,
    pub vertical: f64,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProductionWarning {
    #[serde(default)]
    kind: ProductionWarningKind,
    pub problem: String,
    pub impact: String,
    pub fix: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ProductionWarningKind {
    #[default]
    General,
}

impl ProductionWarning {
    pub(crate) fn new(
        problem: impl Into<String>,
        impact: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            kind: ProductionWarningKind::General,
            problem: problem.into(),
            impact: impact.into(),
            fix: fix.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PdfAnalysis {
    #[serde(default)]
    pub source_pages: Vec<SourcePage>,
    pub filename: String,
    pub page_count: usize,
    pub source_pdf_size: SizeInches,
    pub orientation: Orientation,
    pub media_box: Option<PdfBox>,
    pub crop_box: Option<PdfBox>,
    pub bleed_box: Option<PdfBox>,
    pub trim_box: Option<PdfBox>,
    pub likely_bleed: BleedDetection,
    pub matched_preset_id: Option<String>,
    pub suggested_finished_cut_size: Option<SizeInches>,
    pub appears_duplex: bool,
    #[serde(default)]
    pub orientation_adjusted_pages: usize,
    pub warnings: Vec<ProductionWarning>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LayoutRequest {
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub source_pages: Vec<SourcePage>,
    #[serde(default)]
    pub finished_size_mode: FinishedSizeMode,
    #[serde(default)]
    pub artwork_fit: Option<ArtworkFit>,
    #[serde(default)]
    pub page_overrides: Vec<PageOverride>,
    pub source_pdf_size: SizeInches,
    #[serde(default)]
    pub source_trim_box: Option<PdfBox>,
    #[serde(default)]
    pub source_page_count: Option<usize>,
    pub finished_cut_size: SizeInches,
    pub parent_sheet_size: SizeInches,
    pub quantity_requested: usize,
    #[serde(default)]
    pub imposition_mode: ImpositionMode,
    #[serde(default)]
    pub impression_quantities: Option<Vec<usize>>,
    #[serde(default)]
    pub orientation_preference: OrientationPreference,
    pub sides: Sides,
    #[serde(default)]
    pub duplex: Option<DuplexSettings>,
    pub layout_mode: LayoutMode,
    pub bleed_option: BleedOption,
    #[serde(default)]
    pub source_bleed_override: Option<f64>,
    #[serde(default = "default_created_bleed_amount")]
    pub created_bleed_amount: f64,
    pub gutter: GuttersInches,
    pub manual: Option<ManualLayout>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManualLayout {
    pub rows: usize,
    pub columns: usize,
    pub rotation_degrees: u16,
    pub margins: Option<MarginsInches>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LayoutResult {
    #[serde(default)]
    pub source_bleed_override: Option<f64>,
    #[serde(default)]
    pub page_plans: Vec<PagePlan>,
    pub source_pdf_size: SizeInches,
    pub source_trim_box: Option<PdfBox>,
    pub source_page_count: Option<usize>,
    pub finished_cut_size: SizeInches,
    pub parent_sheet_size: SizeInches,
    pub quantity_requested: usize,
    pub imposition_mode: ImpositionMode,
    pub impression_quantities: Option<Vec<usize>>,
    pub orientation_preference: OrientationPreference,
    pub impressions_requested: usize,
    pub pieces_per_sheet: usize,
    pub sheets_required: usize,
    pub total_pieces_produced: usize,
    pub extra_pieces_produced: usize,
    pub unused_positions: usize,
    pub waste_percent: f64,
    pub rotation_degrees: u16,
    pub rows: usize,
    pub columns: usize,
    pub margins: MarginsInches,
    pub gutters: GuttersInches,
    pub placements: Vec<PiecePlacement>,
    pub duplex: Option<DuplexSettings>,
    pub bleed: BleedSettings,
    pub created_bleed_amount: f64,
    pub warnings: Vec<ProductionWarning>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecentGangUpJob {
    pub id: String,
    pub name: String,
    pub source_filename: Option<String>,
    pub saved_at: String,
    pub request: LayoutRequest,
    pub layout_summary: Option<RecentGangUpLayoutSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecentGangUpJobInput {
    pub name: Option<String>,
    pub source_filename: Option<String>,
    pub request: LayoutRequest,
    pub layout_summary: Option<RecentGangUpLayoutSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum GangUpExportType {
    CleanPdf,
    #[serde(rename = "duploCutPlan")]
    LegacyDuploCutPlan,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OutputPreference {
    #[default]
    CleanPdf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GangUpExportRecord {
    pub id: String,
    pub name: String,
    pub source_filename: Option<String>,
    pub exported_at: String,
    pub output_type: GangUpExportType,
    pub output_filename: String,
    #[serde(default)]
    pub has_stored_file: bool,
    pub request: LayoutRequest,
    pub layout_summary: Option<RecentGangUpLayoutSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GangUpExportRecordInput {
    pub name: Option<String>,
    pub source_filename: Option<String>,
    pub output_type: GangUpExportType,
    pub output_filename: String,
    #[serde(default)]
    pub stored_file: Option<GangUpStoredExportFileInput>,
    pub request: LayoutRequest,
    pub layout_summary: Option<RecentGangUpLayoutSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GangUpStoredExportFileInput {
    pub content_type: String,
    pub base64: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecentGangUpLayoutSummary {
    pub pieces_per_sheet: usize,
    pub sheets_required: usize,
    pub total_pieces_produced: usize,
    pub extra_pieces_produced: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiecePlacement {
    pub index: usize,
    pub row: usize,
    pub column: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub finished_x: f64,
    pub finished_y: f64,
    pub finished_width: f64,
    pub finished_height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DuplexSettings {
    pub flip_edge: DuplexFlipEdge,
    pub rotate_back_180: bool,
    #[serde(skip_deserializing)]
    pub back_alignment: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BleedSettings {
    pub option: BleedOption,
    pub detected_amount_per_side: f64,
    pub effective_amount_per_side: f64,
    pub source: BleedSource,
    pub source_larger_than_cut: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplex_back_alignment_is_output_only() {
        let mut settings: DuplexSettings = serde_json::from_value(serde_json::json!({
            "flipEdge": "shortEdge",
            "rotateBack180": true,
            "backAlignment": "caller-controlled text"
        }))
        .unwrap();

        assert_eq!(settings.flip_edge, DuplexFlipEdge::ShortEdge);
        assert!(settings.rotate_back_180);
        assert!(settings.back_alignment.is_empty());

        settings.back_alignment = "derived alignment".to_string();
        let encoded = serde_json::to_value(settings).unwrap();
        assert_eq!(encoded["backAlignment"], "derived alignment");
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GangUpPreset {
    #[serde(default)]
    pub imposition_mode: ImpositionMode,
    #[serde(default)]
    pub finished_size_mode: FinishedSizeMode,
    #[serde(default)]
    pub artwork_fit: Option<ArtworkFit>,
    #[serde(default)]
    pub source_bleed_override: Option<f64>,
    pub id: String,
    pub name: String,
    pub finished_cut_size: SizeInches,
    pub parent_sheet_size: SizeInches,
    pub bleed_handling: BleedOption,
    #[serde(default = "default_created_bleed_amount")]
    pub created_bleed_amount: f64,
    pub gutter: GuttersInches,
    pub orientation_preference: OrientationPreference,
    pub sides: Sides,
    pub layout_preference: LayoutMode,
    #[serde(default)]
    pub manual: Option<ManualLayout>,
    #[serde(default)]
    pub duplex: Option<DuplexSettings>,
    pub output_preference: OutputPreference,
    pub built_in: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PresetInput {
    #[serde(default)]
    pub imposition_mode: ImpositionMode,
    #[serde(default)]
    pub finished_size_mode: FinishedSizeMode,
    #[serde(default)]
    pub artwork_fit: Option<ArtworkFit>,
    #[serde(default)]
    pub source_bleed_override: Option<f64>,
    pub id: Option<String>,
    pub name: String,
    pub finished_cut_size: SizeInches,
    pub parent_sheet_size: SizeInches,
    pub bleed_handling: BleedOption,
    #[serde(default = "default_created_bleed_amount")]
    pub created_bleed_amount: f64,
    pub gutter: GuttersInches,
    pub orientation_preference: OrientationPreference,
    pub sides: Sides,
    pub layout_preference: LayoutMode,
    #[serde(default)]
    pub manual: Option<ManualLayout>,
    #[serde(default)]
    pub duplex: Option<DuplexSettings>,
    pub output_preference: Option<OutputPreference>,
}

pub(crate) fn orientation_for(size: SizeInches) -> Orientation {
    if (size.width - size.height).abs() <= 0.01 {
        Orientation::Square
    } else if size.width > size.height {
        Orientation::Landscape
    } else {
        Orientation::Portrait
    }
}

fn default_created_bleed_amount() -> f64 {
    0.125
}
