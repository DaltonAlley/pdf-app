//! Target-neutral imposition request, lifecycle, validation, and preview geometry.
#![cfg_attr(test, allow(dead_code))]

use std::{collections::BTreeSet, sync::Arc};

use serde::{Deserialize, Serialize};

const MIN_DIMENSION: f64 = 0.01;
const MAX_DIMENSION: f64 = 100.0;
const MAX_IMPRESSIONS: usize = 10_000;
const MAX_GRID_AXIS: usize = 100;
pub(crate) const MAX_PLACEMENTS: usize = 512;
const PREVIEW_BATCH_MAGIC: &[u8; 8] = b"PDFPV001";

pub(crate) fn parse_numeric_input(text: &str, min: f64, max: f64, step: f64) -> Option<f64> {
    text.parse::<f64>().ok().filter(|number| {
        number.is_finite() && (min..=max).contains(number) && (step != 1.0 || number.fract() == 0.0)
    })
}

pub(crate) fn numeric_draft_matches_value(
    text: &str,
    value: f64,
    min: f64,
    max: f64,
    step: f64,
) -> bool {
    parse_numeric_input(text, min, max, step).is_some_and(|parsed| parsed == value)
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SizeInches {
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginsInches {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuttersInches {
    pub horizontal: f64,
    pub vertical: f64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Orientation {
    Portrait,
    Landscape,
    Square,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OrientationPreference {
    #[default]
    Auto,
    Portrait,
    Landscape,
    Upright,
    QuarterTurn,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FinishedSizeMode {
    #[default]
    Common,
    Original,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ArtworkFitMode {
    #[default]
    Contain,
    Cover,
    Stretch,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtworkPosition {
    pub x: f64,
    pub y: f64,
}

impl Default for ArtworkPosition {
    fn default() -> Self {
        Self { x: 0.5, y: 0.5 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ArtworkFit {
    pub mode: ArtworkFitMode,
    pub position: ArtworkPosition,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePage {
    pub source_pdf_size: SizeInches,
    pub source_trim_box: Option<PdfBox>,
    #[serde(default)]
    pub physical_size_assumed: bool,
    pub preview_box: Option<PdfBox>,
    pub filename: Option<String>,
    pub original_page_number: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageOverride {
    pub page_number: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_cut_size: Option<SizeInches>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artwork_fit: Option<ArtworkFit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtworkRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PagePlan {
    pub page_number: usize,
    pub source_pdf_size: SizeInches,
    pub finished_cut_size: SizeInches,
    pub bleed_amount: f64,
    pub artwork: ArtworkRect,
    pub preview_box: Option<PdfBox>,
    pub position_travel: Option<ArtworkPosition>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Sides {
    Single,
    Double,
}

/// Keeps the user's simplex and duplex repeat quantities while switching print modes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepeatQuantityDrafts {
    page_count: Option<usize>,
    single: Vec<usize>,
    double: Vec<usize>,
}

impl RepeatQuantityDrafts {
    pub fn from_request(request: &LayoutRequest) -> Self {
        let mut drafts = Self::default();
        drafts.rebase(request.source_page_count);
        if let (Some(page_count), Some(values)) = (
            request.source_page_count,
            request.impression_quantities.as_deref(),
        ) {
            let normalized = normalized_quantities(page_count, request.sides, Some(values), 1);
            *drafts.for_sides_mut(request.sides) = normalized;
        }
        drafts
    }

    pub fn rebase(&mut self, page_count: Option<usize>) {
        if self.page_count == page_count {
            return;
        }
        self.page_count = page_count;
        let Some(page_count) = page_count else {
            self.single.clear();
            self.double.clear();
            return;
        };
        self.single = normalized_quantities(page_count, Sides::Single, Some(&self.single), 1);
        self.double = normalized_quantities(page_count, Sides::Double, Some(&self.double), 1);
    }

    pub fn remember(&mut self, sides: Sides, values: &[usize]) {
        let Some(page_count) = self.page_count else {
            return;
        };
        *self.for_sides_mut(sides) = normalized_quantities(page_count, sides, Some(values), 1);
    }

    pub fn values(&self, sides: Sides) -> Vec<usize> {
        self.for_sides(sides).to_vec()
    }

    fn for_sides(&self, sides: Sides) -> &[usize] {
        match sides {
            Sides::Single => &self.single,
            Sides::Double => &self.double,
        }
    }

    fn for_sides_mut(&mut self, sides: Sides) -> &mut Vec<usize> {
        match sides {
            Sides::Single => &mut self.single,
            Sides::Double => &mut self.double,
        }
    }
}

pub fn set_sides_preserving_repeat_drafts(
    request: &mut LayoutRequest,
    drafts: &mut RepeatQuantityDrafts,
    next: Sides,
) {
    drafts.rebase(request.source_page_count);
    if request.imposition_mode == ImpositionMode::Repeat {
        if let Some(values) = request.impression_quantities.as_deref() {
            drafts.remember(request.sides, values);
        }
    }
    request.sides = next;
    request.duplex = if next == Sides::Double {
        Some(request.duplex.clone().unwrap_or(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        }))
    } else {
        None
    };
    if request.imposition_mode == ImpositionMode::Repeat {
        let values = drafts.values(next);
        request.quantity_requested = values.iter().sum();
        request.impression_quantities = Some(values);
    }
}

/// Selects how source pages are consumed while preserving each print mode's repeat draft.
pub fn set_imposition_mode_preserving_repeat_drafts(
    request: &mut LayoutRequest,
    drafts: &mut RepeatQuantityDrafts,
    mode: ImpositionMode,
) {
    if let Some(values) = request.impression_quantities.as_deref() {
        drafts.rebase(request.source_page_count);
        drafts.remember(request.sides, values);
    }
    request.imposition_mode = mode;
    request.impression_quantities = if mode == ImpositionMode::Repeat {
        drafts.rebase(request.source_page_count);
        Some(drafts.values(request.sides))
    } else {
        None
    };
    if let Some(values) = &request.impression_quantities {
        request.quantity_requested = values.iter().sum();
    }
}

pub fn update_repeat_quantity(
    request: &mut LayoutRequest,
    drafts: &mut RepeatQuantityDrafts,
    index: usize,
    quantity: usize,
) {
    let Some(values) = request.impression_quantities.as_mut() else {
        return;
    };
    let Some(slot) = values.get_mut(index) else {
        return;
    };
    *slot = quantity;
    request.quantity_requested = values.iter().sum();
    drafts.rebase(request.source_page_count);
    drafts.remember(request.sides, values);
}

/// Applies one copy count to every simplex page or duplex page pair in the active draft.
pub fn set_all_repeat_quantities(
    request: &mut LayoutRequest,
    drafts: &mut RepeatQuantityDrafts,
    quantity: usize,
) {
    let Some(values) = request.impression_quantities.as_mut() else {
        return;
    };
    values.fill(quantity);
    request.quantity_requested = values.iter().copied().fold(0_usize, usize::saturating_add);
    drafts.rebase(request.source_page_count);
    drafts.remember(request.sides, values);
}

pub fn select_layout_mode(
    request: &mut LayoutRequest,
    mode: LayoutMode,
    seed: Option<&LayoutResult>,
) {
    request.layout_mode = mode;
    request.manual = if mode == LayoutMode::Manual {
        let (rows, columns, rotation_degrees) = seed
            .map(|layout| (layout.rows, layout.columns, layout.rotation_degrees))
            .unwrap_or((1, 1, 0));
        Some(ManualLayout {
            rows,
            columns,
            rotation_degrees,
            margins: None,
        })
    } else {
        None
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LayoutMode {
    Auto,
    MaxPieces,
    Manual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BleedOption {
    UseAsIs,
    ScaleToBleed,
    FitInside,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BleedSource {
    None,
    Detected,
    Manual,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ImpositionMode {
    #[default]
    Repeat,
    Unique,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DuplexFlipEdge {
    LongEdge,
    ShortEdge,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfBox {
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleedDetection {
    pub detected: bool,
    pub amount_per_side: f64,
    pub horizontal: f64,
    pub vertical: f64,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductionWarning {
    pub problem: String,
    pub impact: String,
    pub fix: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfAnalysis {
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

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedPdfSource {
    pub source_id: String,
    pub analysis: PdfAnalysis,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplexSettings {
    pub flip_edge: DuplexFlipEdge,
    pub rotate_back_180: bool,
    #[serde(default)]
    pub back_alignment: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualLayout {
    pub rows: usize,
    pub columns: usize,
    pub rotation_degrees: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margins: Option<MarginsInches>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutRequest {
    /// Browser-only intent, never inferred from artwork geometry.
    #[serde(skip)]
    pub finished_dimensions_chosen: [bool; 2],
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_pages: Option<Vec<SourcePage>>,
    #[serde(default)]
    pub finished_size_mode: FinishedSizeMode,
    #[serde(default)]
    pub artwork_fit: Option<ArtworkFit>,
    #[serde(default)]
    pub page_overrides: Vec<PageOverride>,
    pub source_pdf_size: SizeInches,
    pub source_trim_box: Option<PdfBox>,
    pub source_page_count: Option<usize>,
    pub finished_cut_size: SizeInches,
    pub parent_sheet_size: SizeInches,
    pub quantity_requested: usize,
    pub imposition_mode: ImpositionMode,
    pub impression_quantities: Option<Vec<usize>>,
    pub orientation_preference: OrientationPreference,
    pub sides: Sides,
    pub duplex: Option<DuplexSettings>,
    pub layout_mode: LayoutMode,
    pub bleed_option: BleedOption,
    pub source_bleed_override: Option<f64>,
    pub created_bleed_amount: f64,
    pub gutter: GuttersInches,
    pub manual: Option<ManualLayout>,
}

/// A reusable impose setup returned by the saved-preset catalog.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GangUpPreset {
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
    #[serde(default)]
    pub output_preference: String,
    #[serde(default)]
    pub built_in: bool,
}

/// Payload used to create or update a saved impose preset.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetInput {
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
    #[serde(default)]
    pub output_preference: Option<String>,
}

fn default_created_bleed_amount() -> f64 {
    0.125
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleedSettings {
    pub option: BleedOption,
    pub detected_amount_per_side: f64,
    pub effective_amount_per_side: f64,
    pub source: BleedSource,
    pub source_larger_than_cut: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiecePlacement {
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutResult {
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

/// Returns one authoritative warning set for the output summary.
#[cfg(test)]
pub fn output_warnings(
    layout: Option<&LayoutResult>,
    analysis: Option<&PdfAnalysis>,
) -> Vec<ProductionWarning> {
    layout
        .map(|value| value.warnings.clone())
        .or_else(|| analysis.map(|value| value.warnings.clone()))
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewSide {
    Front,
    Back,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PreviewRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreviewImageRect {
    pub rect: PreviewRect,
    pub transform: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GutterBand {
    pub position: f64,
    pub size: f64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewViewIdentity {
    /// Opaque raster identity: server source id plus the source-bleed interpretation.
    /// Never send this value as a server source token.
    pub source_id: String,
    /// Serialized layout snapshot identity. This binds page assets to the exact geometry that
    /// requested them instead of only to a source and sheet number.
    pub layout_identity: Arc<str>,
    pub pages: Vec<usize>,
    pub retry_generation: u64,
    pub side: PreviewSide,
    pub sheet_index: usize,
    pub sheet_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreviewLifecycle {
    Idle,
    Loading {
        view: PreviewViewIdentity,
        completed: usize,
    },
    Ready {
        view: PreviewViewIdentity,
    },
    Failed {
        view: PreviewViewIdentity,
        completed: usize,
        message: String,
    },
}

/// Coalesces rapid sheet navigation without delaying the first useful preview or an explicit retry.
pub fn should_settle_preview_navigation(
    previous: &PreviewLifecycle,
    next: &PreviewViewIdentity,
    missing_pages: usize,
) -> bool {
    missing_pages > 0
        && previous.source_id() == Some(next.source_id.as_str())
        && previous.pages() != next.pages.as_slice()
}

/// Chooses the fully composed view that may be painted while a requested view loads.
///
/// An uncached target must not replace artwork already on screen with empty placements. A view
/// from another source is never retained because its object URLs no longer describe this job.
pub fn presented_preview_view(
    current: Option<&PreviewViewIdentity>,
    target: &PreviewViewIdentity,
    completed: usize,
) -> Option<PreviewViewIdentity> {
    if completed >= target.pages.len() {
        return Some(target.clone());
    }
    current
        .filter(|view| view.source_id == target.source_id)
        .cloned()
}

pub fn preview_layout_identity(layout: &LayoutResult) -> Result<Arc<str>, String> {
    serde_json::to_string(layout)
        .map(Arc::from)
        .map_err(|error| format!("Could not identify the preview layout: {error}"))
}

pub fn preview_raster_identity(source_id: &str, source_bleed_override: Option<f64>) -> String {
    match source_bleed_override {
        Some(amount) => format!("{source_id}:source-bleed-{:016x}", amount.to_bits()),
        None => source_id.to_owned(),
    }
}

pub fn preview_navigation_label(
    sheet_index: usize,
    sheet_count: usize,
    side: PreviewSide,
) -> String {
    let sheet_count = sheet_count.max(1);
    let sheet = sheet_index
        .min(sheet_count.saturating_sub(1))
        .saturating_add(1);
    let side = match side {
        PreviewSide::Front => "front",
        PreviewSide::Back => "back",
    };
    format!("Sheet {sheet} of {sheet_count}, {side} side imposition preview")
}

pub fn preview_loading_label(
    requested: &PreviewViewIdentity,
    presented: Option<&PreviewViewIdentity>,
    completed: usize,
) -> String {
    let requested_label =
        preview_navigation_label(requested.sheet_index, requested.sheet_count, requested.side);
    let progress = format!(
        "{completed} of {} artwork pages decoded",
        requested.pages.len()
    );
    match presented.filter(|current| *current != requested) {
        Some(current) => format!(
            "Loading {requested_label}: {progress}. {} remains visible.",
            preview_navigation_label(current.sheet_index, current.sheet_count, current.side)
        ),
        None => format!("Loading {requested_label}: {progress}."),
    }
}

/// A decode failure may remove only the exact object URL that emitted it.
pub fn should_remove_failed_preview_url(cached_url: Option<&str>, failed_url: &str) -> bool {
    cached_url == Some(failed_url)
}

pub fn preview_failure_availability(completed: usize, total: usize) -> String {
    let completed = completed.min(total);
    match (completed, total) {
        (0, _) => "No artwork pages are available for this sheet yet.".into(),
        (1, total) => format!("1 of {total} artwork pages is still visible."),
        (completed, total) => {
            format!("{completed} of {total} artwork pages are still visible.")
        }
    }
}

/// Decodes and validates the private preview-batch wire format at the browser boundary.
pub fn decode_preview_batch(
    payload: &[u8],
    requested_pages: &[usize],
) -> Result<Vec<(usize, Vec<u8>)>, String> {
    if payload.len() < 12 {
        return Err("Artwork preview response was truncated.".into());
    }
    if &payload[..8] != PREVIEW_BATCH_MAGIC {
        return Err("Artwork preview response had an invalid signature.".into());
    }
    let count = read_preview_u32(payload, 8)? as usize;
    let requested = requested_pages.iter().copied().collect::<BTreeSet<_>>();
    if requested.len() != requested_pages.len() || count != requested.len() {
        return Err("Artwork preview response did not match the requested pages.".into());
    }

    let mut offset = 12_usize;
    let mut decoded = Vec::with_capacity(count);
    let mut received = BTreeSet::new();
    for _ in 0..count {
        let page = read_preview_u32(payload, offset)? as usize;
        let length = read_preview_u32(payload, offset.saturating_add(4))? as usize;
        offset = offset
            .checked_add(8)
            .ok_or_else(|| "Artwork preview response was too large.".to_string())?;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| "Artwork preview response was too large.".to_string())?;
        let bytes = payload
            .get(offset..end)
            .ok_or_else(|| "Artwork preview response was truncated.".to_string())?;
        if bytes.is_empty() || !received.insert(page) {
            return Err("Artwork preview response contained an invalid page.".into());
        }
        decoded.push((page, bytes.to_vec()));
        offset = end;
    }
    if offset != payload.len() || received != requested {
        return Err("Artwork preview response did not match the requested pages.".into());
    }
    Ok(decoded)
}

fn read_preview_u32(payload: &[u8], offset: usize) -> Result<u32, String> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| "Artwork preview response was too large.".to_string())?;
    let bytes = payload
        .get(offset..end)
        .ok_or_else(|| "Artwork preview response was truncated.".to_string())?;
    let bytes: [u8; 4] = bytes
        .try_into()
        .map_err(|_| "Artwork preview response was truncated.".to_string())?;
    Ok(u32::from_be_bytes(bytes))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InitialPreparationPhase {
    Analysis { percent: f64 },
    Layout,
    Artwork { completed: usize, total: usize },
    Ready,
}

/// Returns every unique artwork page needed to reveal the first sheet complete.
pub fn initial_preview_pages(layout: &LayoutResult) -> Vec<usize> {
    preview_page_numbers(layout, PreviewSide::Front, 0)
}

/// Returns every unique artwork page used anywhere in the current sheet layout.
#[cfg(test)]
pub fn all_preview_pages(layout: &LayoutResult) -> Vec<usize> {
    let mut pages = BTreeSet::new();
    for sheet_index in 0..layout.sheets_required {
        pages.extend(preview_page_numbers(
            layout,
            PreviewSide::Front,
            sheet_index,
        ));
        if layout.duplex.is_some() {
            pages.extend(preview_page_numbers(layout, PreviewSide::Back, sheet_index));
        }
    }
    pages.into_iter().collect()
}

/// Keeps background preview work bounded to the visible sheet and one likely navigation target.
///
/// Rendering an entire long document in the background competes with foreground navigation and
/// makes cancelled sheet changes appear to load forever. The adjacent sheet is enough to make the
/// common Next/Previous path immediate without creating an unbounded render queue.
pub fn preview_cache_window(
    layout: &LayoutResult,
    side: PreviewSide,
    sheet_index: usize,
) -> Vec<usize> {
    let current = sheet_index.min(layout.sheets_required.saturating_sub(1));
    let neighbor = if current + 1 < layout.sheets_required {
        Some(current + 1)
    } else {
        current.checked_sub(1)
    };
    let mut pages = BTreeSet::from_iter(preview_page_numbers(layout, side, current));
    if let Some(neighbor) = neighbor {
        pages.extend(preview_page_numbers(layout, side, neighbor));
    }
    pages.into_iter().collect()
}

/// Protects both the requested view and the complete view currently on screen from LRU eviction.
pub fn preview_protected_pages(
    target: &PreviewViewIdentity,
    presented: Option<&PreviewViewIdentity>,
) -> Vec<usize> {
    let mut pages = BTreeSet::from_iter(target.pages.iter().copied());
    if let Some(presented) = presented.filter(|view| view.source_id == target.source_id) {
        pages.extend(presented.pages.iter().copied());
    }
    pages.into_iter().collect()
}

pub fn initial_preparation_percent(phase: InitialPreparationPhase) -> u8 {
    fn scale(value: f64, total: f64, start: f64, end: f64) -> u8 {
        if !value.is_finite() || !total.is_finite() || total <= 0.0 {
            return start as u8;
        }
        (start + (value / total).clamp(0.0, 1.0) * (end - start)).round() as u8
    }
    match phase {
        InitialPreparationPhase::Analysis { percent } => scale(percent, 100.0, 0.0, 45.0),
        InitialPreparationPhase::Layout => 50,
        InitialPreparationPhase::Artwork { completed, total } => {
            scale(completed as f64, total as f64, 55.0, 99.0)
        }
        InitialPreparationPhase::Ready => 100,
    }
}

impl PreviewLifecycle {
    pub fn loading(view: PreviewViewIdentity, completed: usize) -> Self {
        let completed = completed.min(view.pages.len());
        if completed == view.pages.len() {
            Self::Ready { view }
        } else {
            Self::Loading { view, completed }
        }
    }

    pub fn source_id(&self) -> Option<&str> {
        match self {
            Self::Loading { view, .. } | Self::Ready { view } | Self::Failed { view, .. } => {
                Some(&view.source_id)
            }
            Self::Idle => None,
        }
    }

    pub fn pages(&self) -> &[usize] {
        match self {
            Self::Loading { view, .. } | Self::Ready { view } | Self::Failed { view, .. } => {
                &view.pages
            }
            Self::Idle => &[],
        }
    }

    #[cfg(test)]
    pub fn completed(&self) -> usize {
        match self {
            Self::Loading { completed, .. } | Self::Failed { completed, .. } => *completed,
            Self::Ready { view } => view.pages.len(),
            Self::Idle => 0,
        }
    }

    #[cfg(test)]
    pub fn total(&self) -> usize {
        self.pages().len()
    }

    #[cfg(test)]
    pub fn is_usable_for(&self, view: &PreviewViewIdentity) -> bool {
        self.view() == Some(view) && self.completed() > 0
    }

    pub fn accepts_completion(&self, view: &PreviewViewIdentity) -> bool {
        matches!(
            self,
            Self::Loading { view: current, .. } | Self::Failed { view: current, .. }
                if current == view
        )
    }

    pub fn accepts_failure(&self, view: &PreviewViewIdentity) -> bool {
        self.view() == Some(view)
    }

    pub fn record_completed(&mut self, view: &PreviewViewIdentity, completed: usize) {
        let (current, current_completed) = match self {
            Self::Loading {
                view: current,
                completed,
            }
            | Self::Failed {
                view: current,
                completed,
                ..
            } => (current, completed),
            Self::Idle | Self::Ready { .. } => return,
        };
        if current != view {
            return;
        }
        *current_completed = completed.min(current.pages.len());
        if *current_completed == current.pages.len() {
            *self = Self::Ready {
                view: current.clone(),
            };
        }
    }

    pub fn record_failure(
        &mut self,
        view: &PreviewViewIdentity,
        completed: usize,
        message: String,
    ) {
        if !self.accepts_failure(view) {
            return;
        }
        let current = view.clone();
        let completed = completed.min(current.pages.len());
        *self = Self::Failed {
            view: current,
            completed,
            message,
        };
    }

    fn view(&self) -> Option<&PreviewViewIdentity> {
        match self {
            Self::Loading { view, .. } | Self::Ready { view } | Self::Failed { view, .. } => {
                Some(view)
            }
            Self::Idle => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreparationLifecycle {
    Idle,
    Preparing { request_id: u64 },
    Ready { request_id: u64, source_id: String },
    Failed { request_id: u64, message: String },
    Cancelled { request_id: u64 },
}

impl PreparationLifecycle {
    pub fn accepts(&self, request_id: u64) -> bool {
        matches!(self, Self::Preparing { request_id: current } if *current == request_id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LayoutLifecycle {
    Empty,
    /// The draft is currently invalid, while the last valid sheet remains available to inspect.
    Invalid {
        previous: Option<LayoutResult>,
    },
    Loading {
        request_id: u64,
        signature: String,
        previous: Option<LayoutResult>,
    },
    Ready {
        signature: String,
        layout: LayoutResult,
    },
    Failed {
        signature: String,
        message: String,
        previous: Option<LayoutResult>,
    },
}

impl LayoutLifecycle {
    pub fn accepts(&self, request_id: u64, signature: &str) -> bool {
        matches!(self, Self::Loading { request_id: current, signature: current_signature, .. }
            if *current == request_id && current_signature == signature)
    }

    pub fn last_good(&self) -> Option<&LayoutResult> {
        match self {
            Self::Ready { layout, .. } => Some(layout),
            Self::Invalid { previous }
            | Self::Loading { previous, .. }
            | Self::Failed { previous, .. } => previous.as_ref(),
            Self::Empty => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExportLifecycle {
    Idle,
    Exporting { request_id: u64 },
    Complete { filename: String },
    Failed { message: String },
    Cancelled,
}

pub fn initial_request() -> LayoutRequest {
    LayoutRequest {
        finished_dimensions_chosen: [false; 2],
        source_id: None,
        source_pages: None,
        finished_size_mode: FinishedSizeMode::Common,
        artwork_fit: Some(ArtworkFit::default()),
        page_overrides: Vec::new(),
        source_pdf_size: SizeInches {
            width: 3.5,
            height: 2.0,
        },
        source_trim_box: None,
        source_page_count: None,
        finished_cut_size: SizeInches {
            width: 3.5,
            height: 2.0,
        },
        parent_sheet_size: SizeInches {
            width: 12.0,
            height: 18.0,
        },
        quantity_requested: 1,
        imposition_mode: ImpositionMode::Unique,
        impression_quantities: None,
        orientation_preference: OrientationPreference::Auto,
        sides: Sides::Single,
        duplex: None,
        layout_mode: LayoutMode::MaxPieces,
        bleed_option: BleedOption::UseAsIs,
        source_bleed_override: None,
        created_bleed_amount: 0.125,
        gutter: GuttersInches {
            horizontal: 0.299,
            vertical: 0.299,
        },
        manual: None,
    }
}

pub fn request_from_analysis(mut request: LayoutRequest, analysis: &PdfAnalysis) -> LayoutRequest {
    request.source_pages =
        (!analysis.source_pages.is_empty()).then(|| analysis.source_pages.clone());
    request
        .page_overrides
        .retain(|value| value.page_number <= analysis.page_count);
    request.source_pdf_size = analysis.source_pdf_size;
    request.source_trim_box = analysis.trim_box;
    request.source_page_count = Some(analysis.page_count);
    // Finished dimensions belong to the user, not the replacement source.
    request.source_bleed_override = None;
    if request.layout_mode == LayoutMode::Auto {
        request.layout_mode = LayoutMode::MaxPieces;
        request.manual = None;
    }
    if !supports_duplex(Some(analysis.page_count)) {
        request.sides = Sides::Single;
        request.duplex = None;
    }
    if request.imposition_mode == ImpositionMode::Repeat || analysis.page_count == 1 {
        request.imposition_mode = ImpositionMode::Repeat;
        request.impression_quantities = Some(normalized_quantities(
            analysis.page_count,
            request.sides,
            request.impression_quantities.as_deref(),
            request.quantity_requested,
        ));
    } else {
        request.impression_quantities = None;
    }
    request
}

pub fn restore_request_from_analysis(
    request: LayoutRequest,
    analysis: &PdfAnalysis,
    preserve_finished_cut: bool,
) -> LayoutRequest {
    let finished = request.finished_cut_size;
    let manual_bleed = request.source_bleed_override;
    let mut restored = request_from_analysis(request, analysis);
    if preserve_finished_cut {
        restored.finished_cut_size = finished;
    }
    if let Some(bleed) = manual_bleed {
        if restored.source_id.is_some() || restored.source_pages.is_some() {
            // Source bleed is an input-artwork property, not a finished-product dimension.
            // Preserve it through retries/replacements and let the server validate the source.
            restored.source_bleed_override = Some(bleed);
            return restored;
        }
        if analysis_contains_manual_source_bleed(analysis, restored.finished_cut_size, bleed) {
            if let Some(manual) = request_with_manual_source_bleed(restored.clone(), bleed) {
                return manual;
            }
        }
    }
    restored
}

pub fn source_size_from_finished_bleed(
    finished: SizeInches,
    bleed_per_side: f64,
) -> Option<SizeInches> {
    if !bleed_per_side.is_finite() || !(0.001..=1.0).contains(&bleed_per_side) {
        return None;
    }
    let rounded = |value: f64| (value * 10_000.0).round() / 10_000.0;
    let size = SizeInches {
        width: rounded(finished.width + bleed_per_side * 2.0),
        height: rounded(finished.height + bleed_per_side * 2.0),
    };
    (size.width <= MAX_DIMENSION && size.height <= MAX_DIMENSION).then_some(size)
}

pub fn request_with_manual_source_bleed(
    mut request: LayoutRequest,
    bleed_per_side: f64,
) -> Option<LayoutRequest> {
    if request.source_id.is_some() || request.source_pages.is_some() {
        if !bleed_per_side.is_finite() || !(0.001..=1.0).contains(&bleed_per_side) {
            return None;
        }
        request.source_bleed_override = Some(bleed_per_side);
        return Some(request);
    }
    let source = source_size_from_finished_bleed(request.finished_cut_size, bleed_per_side)?;
    request.source_pdf_size = source;
    request.source_trim_box = Some(PdfBox {
        left: bleed_per_side,
        bottom: bleed_per_side,
        right: bleed_per_side + request.finished_cut_size.width,
        top: bleed_per_side + request.finished_cut_size.height,
        width: request.finished_cut_size.width,
        height: request.finished_cut_size.height,
    });
    request.source_bleed_override = Some(bleed_per_side);
    Some(request)
}

pub fn analysis_contains_manual_source_bleed(
    analysis: &PdfAnalysis,
    finished: SizeInches,
    bleed_per_side: f64,
) -> bool {
    const TOLERANCE: f64 = 0.001;
    let Some(expected_outer) = source_size_from_finished_bleed(finished, bleed_per_side) else {
        return false;
    };
    let close = |left: f64, right: f64| (left - right).abs() <= TOLERANCE;
    let source_matches = close(analysis.source_pdf_size.width, expected_outer.width)
        && close(analysis.source_pdf_size.height, expected_outer.height);
    let media = analysis.media_box.unwrap_or(PdfBox {
        left: 0.0,
        bottom: 0.0,
        right: analysis.source_pdf_size.width,
        top: analysis.source_pdf_size.height,
        width: analysis.source_pdf_size.width,
        height: analysis.source_pdf_size.height,
    });
    let embedded = [analysis.trim_box, analysis.crop_box]
        .into_iter()
        .flatten()
        .find(|value| close(value.width, finished.width) && close(value.height, finished.height));
    let Some(embedded) = embedded else {
        return source_matches;
    };
    media.left <= embedded.left - bleed_per_side + TOLERANCE
        && media.bottom <= embedded.bottom - bleed_per_side + TOLERANCE
        && media.right + TOLERANCE >= embedded.right + bleed_per_side
        && media.top + TOLERANCE >= embedded.top + bleed_per_side
}

pub fn supports_duplex(page_count: Option<usize>) -> bool {
    page_count.is_none_or(|count| count >= 2 && count % 2 == 0)
}

pub fn impression_count(page_count: Option<usize>, sides: Sides) -> usize {
    let count = page_count.unwrap_or(0);
    if sides == Sides::Double {
        count / 2
    } else {
        count
    }
}

pub fn normalized_quantities(
    page_count: usize,
    sides: Sides,
    current: Option<&[usize]>,
    default_quantity: usize,
) -> Vec<usize> {
    let count = impression_count(Some(page_count), sides);
    (0..count)
        .map(|index| {
            current
                .and_then(|values| values.get(index))
                .copied()
                .unwrap_or(if count == 1 { default_quantity } else { 1 })
        })
        .collect()
}

#[cfg(test)]
pub fn selection_after_replacement_failure(
    visible_ids: &[u64],
    failed_ids: &[u64],
    active_ids: &[u64],
) -> Vec<u64> {
    if visible_ids == failed_ids && !active_ids.is_empty() {
        active_ids.to_vec()
    } else {
        visible_ids.to_vec()
    }
}

pub fn request_issue(request: &LayoutRequest) -> Option<String> {
    if request.sides == Sides::Single && request.duplex.is_some() {
        return Some("Single-sided imposition cannot include duplex settings.".into());
    }
    if request.sides == Sides::Double && request.duplex.is_none() {
        return Some("Double-sided imposition requires duplex settings.".into());
    }
    if request.imposition_mode == ImpositionMode::Unique && request.impression_quantities.is_some()
    {
        return Some("Unique imposition cannot include repeat quantities.".into());
    }
    if request.layout_mode != LayoutMode::Manual && request.manual.is_some() {
        return Some("Manual layout settings require custom layout mode.".into());
    }
    if !valid_size(request.finished_cut_size) {
        return Some("Finished size must be between 0.01 and 100 inches in each direction.".into());
    }
    if !valid_size(request.parent_sheet_size) {
        return Some(
            "Parent sheet size must be between 0.01 and 100 inches in each direction.".into(),
        );
    }
    if request.imposition_mode == ImpositionMode::Repeat {
        if let Some(quantities) = &request.impression_quantities {
            if quantities.len() != impression_count(request.source_page_count, request.sides) {
                return Some("Repeat quantities do not match the PDF pages.".into());
            }
            let total = quantities.iter().sum::<usize>();
            if !(1..=MAX_IMPRESSIONS).contains(&total) {
                return Some("Total repeat quantity must be from 1 to 10,000.".into());
            }
        } else if !(1..=MAX_IMPRESSIONS).contains(&request.quantity_requested) {
            return Some("Finished quantity must be a whole number from 1 to 10,000.".into());
        }
    }
    if request.sides == Sides::Double && !supports_duplex(request.source_page_count) {
        return Some("Double-sided imposition requires an even number of PDF pages.".into());
    }
    if !valid_gutter(request.gutter.horizontal) || !valid_gutter(request.gutter.vertical) {
        return Some("Gutters must be from 0 to 100 inches.".into());
    }
    if let Some(bleed) = request.source_bleed_override {
        if !bleed.is_finite() || !(0.001..=1.0).contains(&bleed) {
            return Some("Source bleed must be from 0.001 to 1 inch per side.".into());
        }
        let expected_width = request.source_pdf_size.width - bleed * 2.0;
        let expected_height = request.source_pdf_size.height - bleed * 2.0;
        if request.source_id.is_none()
            && request.source_pages.is_none()
            && (expected_width < MIN_DIMENSION
                || expected_height < MIN_DIMENSION
                || (expected_width - request.finished_cut_size.width).abs() > 0.01
                || (expected_height - request.finished_cut_size.height).abs() > 0.01)
        {
            return Some(
                "Finished size must match the PDF size minus the source bleed on each side.".into(),
            );
        }
    }
    if request.bleed_option == BleedOption::ScaleToBleed
        && (!request.created_bleed_amount.is_finite()
            || !(0.001..=1.0).contains(&request.created_bleed_amount))
    {
        return Some("Created bleed must be from 0.001 to 1 inch.".into());
    }
    if request.layout_mode == LayoutMode::Manual {
        let Some(manual) = request.manual else {
            return Some("Custom layout requires rows and columns.".into());
        };
        if manual.rows == 0
            || manual.columns == 0
            || manual.rows > MAX_GRID_AXIS
            || manual.columns > MAX_GRID_AXIS
        {
            return Some(
                "Custom n-up columns and rows must be whole numbers from 1 to 100.".into(),
            );
        }
        if manual.rows.saturating_mul(manual.columns) > MAX_PLACEMENTS {
            return Some("Custom n-up is limited to 512 placements.".into());
        }
        if let Some(margins) = manual.margins {
            if [margins.top, margins.right, margins.bottom, margins.left]
                .into_iter()
                .any(|value| !value.is_finite() || !(0.0..=100.0).contains(&value))
            {
                return Some("Margins must be from 0 to 100 inches.".into());
            }
        }
    }
    None
}

pub fn request_issue_with_analysis(
    request: &LayoutRequest,
    analysis: Option<&PdfAnalysis>,
) -> Option<String> {
    if let (Some(bleed), Some(analysis)) = (request.source_bleed_override, analysis) {
        if request.source_id.is_none()
            && request.source_pages.is_none()
            && !analysis_contains_manual_source_bleed(analysis, request.finished_cut_size, bleed)
        {
            return Some(
                "The PDF does not contain the specified bleed outside the finished boundary."
                    .into(),
            );
        }
    }
    request_issue(request)
}

fn valid_size(size: SizeInches) -> bool {
    size.width.is_finite()
        && size.height.is_finite()
        && (MIN_DIMENSION..=MAX_DIMENSION).contains(&size.width)
        && (MIN_DIMENSION..=MAX_DIMENSION).contains(&size.height)
}

fn valid_gutter(value: f64) -> bool {
    value.is_finite() && (0.0..=100.0).contains(&value)
}

pub fn request_signature(request: &LayoutRequest) -> Result<String, String> {
    serde_json::to_string(request)
        .map_err(|error| format!("Could not encode the layout request: {error}"))
}

#[cfg(test)]
pub fn page_usage(layout: &LayoutResult) -> &'static str {
    if layout.imposition_mode == ImpositionMode::Unique {
        "One of each, in order"
    } else if layout.impression_quantities.is_some() {
        if layout.duplex.is_some() {
            "Copies by page pair"
        } else {
            "Copies by page"
        }
    } else if layout.duplex.is_some() {
        "Page pair repeated"
    } else {
        "Page 1 repeated"
    }
}

pub fn preview_page_number(
    layout: &LayoutResult,
    placement_index: usize,
    side: PreviewSide,
    sheet_index: usize,
) -> Option<usize> {
    if layout.imposition_mode == ImpositionMode::Repeat && layout.impression_quantities.is_none() {
        return Some(if side == PreviewSide::Back && layout.duplex.is_some() {
            2
        } else {
            1
        });
    }
    let impression_index = sheet_index
        .saturating_mul(layout.pieces_per_sheet)
        .saturating_add(placement_index);
    if impression_index >= layout.impressions_requested {
        return None;
    }
    let source_index = if layout.imposition_mode == ImpositionMode::Repeat {
        repeated_source(
            layout.impression_quantities.as_deref().unwrap_or_default(),
            impression_index,
        )?
    } else {
        impression_index
    };
    let page = if layout.duplex.is_some() {
        source_index
            .saturating_mul(2)
            .saturating_add(usize::from(side == PreviewSide::Back))
            .saturating_add(1)
    } else {
        source_index.saturating_add(1)
    };
    (page <= layout.source_page_count.unwrap_or(usize::MAX)).then_some(page)
}

pub fn preview_page_numbers(
    layout: &LayoutResult,
    side: PreviewSide,
    sheet_index: usize,
) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    layout
        .placements
        .iter()
        .filter_map(|placement| preview_page_number(layout, placement.index, side, sheet_index))
        .filter(|page| seen.insert(*page))
        .collect()
}

/// Stable identity for a rendered placement. Every value captured by a keyed Leptos row is
/// represented so changing sheets, sides, source pages, or geometry remounts that row.
pub fn preview_placement_key(
    source_id: &str,
    layout: &LayoutResult,
    placement: &PiecePlacement,
    side: PreviewSide,
    sheet_index: usize,
    page: Option<usize>,
) -> String {
    let artwork = preview_artwork_rect(layout, placement);
    let clip = preview_clip_rect(layout, placement);
    let image = preview_artwork_image(layout, artwork, side);
    let bits = |value: f64| value.to_bits();
    format!(
        "{source_id}:{side:?}:{sheet_index}:{page:?}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{:?}:{:?}",
        placement.index,
        placement.row,
        placement.column,
        bits(placement.finished_x),
        bits(placement.finished_y),
        bits(placement.finished_width),
        bits(placement.finished_height),
        bits(artwork.x),
        bits(artwork.y),
        bits(artwork.width),
        bits(artwork.height),
        bits(clip.x),
        bits(clip.y),
        bits(clip.width),
        bits(clip.height),
        bits(image.rect.x),
        bits(image.rect.y),
        bits(image.rect.width),
        bits(image.rect.height),
        bits(layout.parent_sheet_size.width),
        bits(layout.parent_sheet_size.height),
        layout.rotation_degrees,
        image.transform,
        layout
            .duplex
            .as_ref()
            .map(|duplex| (duplex.flip_edge, duplex.rotate_back_180)),
    )
}

fn repeated_source(quantities: &[usize], impression_index: usize) -> Option<usize> {
    let mut remaining = impression_index;
    for (index, quantity) in quantities.iter().copied().enumerate() {
        if remaining < quantity {
            return Some(index);
        }
        remaining = remaining.saturating_sub(quantity);
    }
    None
}

pub fn duplex_mirror_axes(layout: &LayoutResult) -> (bool, bool) {
    let Some(duplex) = &layout.duplex else {
        return (false, false);
    };
    let landscape = layout.parent_sheet_size.width > layout.parent_sheet_size.height;
    let base_x = if duplex.flip_edge == DuplexFlipEdge::LongEdge {
        !landscape
    } else {
        landscape
    };
    (
        base_x != duplex.rotate_back_180,
        base_x == duplex.rotate_back_180,
    )
}

pub fn preview_artwork_rect(layout: &LayoutResult, placement: &PiecePlacement) -> PreviewRect {
    if layout.bleed.option == BleedOption::UseAsIs {
        return PreviewRect {
            x: placement.x,
            y: placement.y,
            width: placement.width,
            height: placement.height,
        };
    }
    let finished = PreviewRect {
        x: placement.finished_x,
        y: placement.finished_y,
        width: placement.finished_width,
        height: placement.finished_height,
    };
    let source = if layout.rotation_degrees == 90 {
        SizeInches {
            width: layout.source_pdf_size.height,
            height: layout.source_pdf_size.width,
        }
    } else {
        layout.source_pdf_size
    };
    if layout.bleed.option == BleedOption::FitInside {
        let scale = (finished.width / source.width).min(finished.height / source.height);
        return centered(finished, source.width * scale, source.height * scale);
    }
    let bleed = if layout.bleed.effective_amount_per_side > 0.0 {
        layout.bleed.effective_amount_per_side
    } else if layout.created_bleed_amount > 0.0 {
        layout.created_bleed_amount
    } else {
        0.125
    };
    let trim = layout
        .source_trim_box
        .map(|value| rotate_box(value, layout.source_pdf_size, layout.rotation_degrees));
    let basis = trim.unwrap_or(PdfBox {
        left: 0.0,
        bottom: 0.0,
        right: source.width,
        top: source.height,
        width: source.width,
        height: source.height,
    });
    let target = if trim.is_some() && layout.bleed.effective_amount_per_side > 0.0 {
        finished
    } else {
        PreviewRect {
            width: finished.width + bleed * 2.0,
            height: finished.height + bleed * 2.0,
            ..finished
        }
    };
    let scale_x = target.width / basis.width;
    let scale_y = target.height / basis.height;
    let trim_center_x = basis.left + basis.width / 2.0;
    let trim_center_top = source.height - basis.bottom - basis.height / 2.0;
    PreviewRect {
        x: finished.x + finished.width / 2.0 - trim_center_x * scale_x,
        y: finished.y + finished.height / 2.0 - trim_center_top * scale_y,
        width: source.width * scale_x,
        height: source.height * scale_y,
    }
}

pub fn preview_artwork_image(
    layout: &LayoutResult,
    artwork: PreviewRect,
    side: PreviewSide,
) -> PreviewImageRect {
    let rotate_back = side == PreviewSide::Back
        && layout
            .duplex
            .as_ref()
            .is_some_and(|duplex| duplex.rotate_back_180);
    let rotation = (layout.rotation_degrees + if rotate_back { 180 } else { 0 }) % 360;
    let center_x = artwork.x + artwork.width / 2.0;
    let center_y = artwork.y + artwork.height / 2.0;
    let swaps_axes = rotation == 90 || rotation == 270;
    let rect = if swaps_axes {
        PreviewRect {
            x: center_x - artwork.height / 2.0,
            y: center_y - artwork.width / 2.0,
            width: artwork.height,
            height: artwork.width,
        }
    } else {
        artwork
    };
    PreviewImageRect {
        rect,
        transform: (rotation != 0).then(|| format!("rotate({rotation} {center_x} {center_y})")),
    }
}

pub fn preview_source_image_rect(
    selected_artwork: PreviewImageRect,
    raster_box: PdfBox,
    selected_box: PdfBox,
) -> PreviewImageRect {
    if raster_box.width <= 0.0
        || raster_box.height <= 0.0
        || selected_box.width <= 0.0
        || selected_box.height <= 0.0
    {
        return selected_artwork;
    }
    let scale_x = selected_artwork.rect.width / selected_box.width;
    let scale_y = selected_artwork.rect.height / selected_box.height;
    PreviewImageRect {
        rect: PreviewRect {
            x: selected_artwork.rect.x - (selected_box.left - raster_box.left) * scale_x,
            y: selected_artwork.rect.y - (raster_box.top - selected_box.top) * scale_y,
            width: raster_box.width * scale_x,
            height: raster_box.height * scale_y,
        },
        transform: selected_artwork.transform,
    }
}

pub fn raster_artwork_box(analysis: &PdfAnalysis) -> PdfBox {
    analysis.media_box.unwrap_or(PdfBox {
        left: 0.0,
        bottom: 0.0,
        right: analysis.source_pdf_size.width,
        top: analysis.source_pdf_size.height,
        width: analysis.source_pdf_size.width,
        height: analysis.source_pdf_size.height,
    })
}

pub fn desired_artwork_box(layout: &LayoutResult, analysis: &PdfAnalysis) -> PdfBox {
    if layout.bleed.source == BleedSource::Manual {
        let finished = [analysis.trim_box, analysis.crop_box]
            .into_iter()
            .flatten()
            .find(|value| {
                (value.width - layout.finished_cut_size.width).abs() <= 0.01
                    && (value.height - layout.finished_cut_size.height).abs() <= 0.01
            });
        if let Some(finished) = finished {
            let amount = layout.bleed.effective_amount_per_side;
            return PdfBox {
                left: finished.left - amount,
                bottom: finished.bottom - amount,
                right: finished.right + amount,
                top: finished.top + amount,
                width: finished.width + amount * 2.0,
                height: finished.height + amount * 2.0,
            };
        }
    }
    PdfBox {
        left: 0.0,
        bottom: 0.0,
        right: analysis.source_pdf_size.width,
        top: analysis.source_pdf_size.height,
        width: analysis.source_pdf_size.width,
        height: analysis.source_pdf_size.height,
    }
}

pub fn preview_clip_rect(layout: &LayoutResult, placement: &PiecePlacement) -> PreviewRect {
    let left = if placement.column == 0 {
        0.0
    } else {
        placement.finished_x - layout.gutters.horizontal / 2.0
    };
    let right = if placement.column + 1 == layout.columns {
        layout.parent_sheet_size.width
    } else {
        placement.finished_x + placement.finished_width + layout.gutters.horizontal / 2.0
    };
    let top = if placement.row == 0 {
        0.0
    } else {
        placement.finished_y - layout.gutters.vertical / 2.0
    };
    let bottom = if placement.row + 1 == layout.rows {
        layout.parent_sheet_size.height
    } else {
        placement.finished_y + placement.finished_height + layout.gutters.vertical / 2.0
    };
    PreviewRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

pub fn unique_positions(values: impl IntoIterator<Item = f64>) -> Vec<f64> {
    let mut positions = values.into_iter().collect::<Vec<_>>();
    positions.sort_by(f64::total_cmp);
    positions.dedup_by(|left, right| (*left - *right).abs() < 0.0001);
    positions
}

pub fn gutter_bands(edges: impl IntoIterator<Item = f64>, expected: f64) -> Vec<GutterBand> {
    if expected <= 0.0 || !expected.is_finite() {
        return Vec::new();
    }
    unique_positions(edges)
        .windows(2)
        .filter_map(|pair| {
            let size = pair[1] - pair[0];
            ((size - expected).abs() <= 0.01).then_some(GutterBand {
                position: pair[0],
                size,
            })
        })
        .collect()
}

pub fn validate_prepared_source(prepared: &PreparedPdfSource) -> Result<(), String> {
    if prepared.source_id.is_empty() {
        return Err("The prepared PDF source did not include a source id.".into());
    }
    if prepared.analysis.page_count == 0 || !valid_size(prepared.analysis.source_pdf_size) {
        return Err("The prepared PDF source contained invalid page geometry.".into());
    }
    Ok(())
}

pub fn validate_layout_result(layout: &LayoutResult) -> Result<(), String> {
    if !layout.page_plans.is_empty() {
        let pages = layout
            .page_plans
            .iter()
            .map(|plan| plan.page_number)
            .collect::<BTreeSet<_>>();
        let expected = layout.source_page_count.unwrap_or(layout.page_plans.len());
        if pages != (1..=expected).collect() || pages.len() != layout.page_plans.len() {
            return Err(
                "The layout result did not include one authoritative plan for every artwork page."
                    .into(),
            );
        }
    }
    for plan in &layout.page_plans {
        if plan.page_number == 0
            || !valid_size(plan.source_pdf_size)
            || !valid_size(plan.finished_cut_size)
            || ![
                plan.artwork.x,
                plan.artwork.y,
                plan.artwork.width,
                plan.artwork.height,
                plan.bleed_amount,
            ]
            .into_iter()
            .all(f64::is_finite)
            || plan.artwork.width <= 0.0
            || plan.artwork.height <= 0.0
            || plan.bleed_amount < 0.0
        {
            return Err("The layout result contained an invalid artwork plan.".into());
        }
    }
    if !valid_size(layout.source_pdf_size)
        || !valid_size(layout.finished_cut_size)
        || !valid_size(layout.parent_sheet_size)
        || layout.rows == 0
        || layout.columns == 0
        || layout.pieces_per_sheet == 0
        || layout.rows.saturating_mul(layout.columns) != layout.pieces_per_sheet
        || layout.placements.len() != layout.pieces_per_sheet
        || layout.placements.len() > MAX_PLACEMENTS
        || layout.rotation_degrees != 0 && layout.rotation_degrees != 90
    {
        return Err("The layout result contained invalid sheet geometry.".into());
    }
    let finite_rect = |placement: &PiecePlacement| {
        [
            placement.x,
            placement.y,
            placement.width,
            placement.height,
            placement.finished_x,
            placement.finished_y,
            placement.finished_width,
            placement.finished_height,
        ]
        .into_iter()
        .all(f64::is_finite)
            && placement.width > 0.0
            && placement.height > 0.0
            && placement.finished_width > 0.0
            && placement.finished_height > 0.0
    };
    if layout
        .placements
        .iter()
        .any(|placement| !finite_rect(placement))
    {
        return Err("The layout result contained an invalid placement.".into());
    }
    let mut indexes = BTreeSet::new();
    if layout.placements.iter().any(|placement| {
        placement.row >= layout.rows
            || placement.column >= layout.columns
            || placement.index >= layout.pieces_per_sheet
            || !indexes.insert(placement.index)
    }) {
        return Err("The layout result contained inconsistent placement indexes.".into());
    }
    Ok(())
}

/// The backend supplies the fit and anchor. Only sheet rotation and registration are applied here.
pub fn planned_preview_geometry(
    layout: &LayoutResult,
    placement: &PiecePlacement,
    plan: &PagePlan,
    side: PreviewSide,
) -> (PreviewRect, PreviewRect, PreviewImageRect) {
    let size = plan.finished_cut_size;
    let rotated = layout.rotation_degrees == 90;
    let (width, height) = if rotated {
        (size.height, size.width)
    } else {
        (size.width, size.height)
    };
    let mut cut = centered(
        PreviewRect {
            x: placement.finished_x,
            y: placement.finished_y,
            width: placement.finished_width,
            height: placement.finished_height,
        },
        width,
        height,
    );
    let rotate_back = side == PreviewSide::Back
        && layout
            .duplex
            .as_ref()
            .is_some_and(|value| value.rotate_back_180);
    if side == PreviewSide::Back {
        let (mx, my) = duplex_mirror_axes(layout);
        if mx {
            cut.x = layout.parent_sheet_size.width - cut.x - cut.width;
        }
        if my {
            cut.y = layout.parent_sheet_size.height - cut.y - cut.height;
        }
    }
    let center_x = cut.x + cut.width / 2.0;
    let center_y = cut.y + cut.height / 2.0;
    let mut image = PreviewRect {
        x: center_x - size.width / 2.0 + plan.artwork.x,
        y: center_y - size.height / 2.0 + plan.artwork.y,
        width: plan.artwork.width,
        height: plan.artwork.height,
    };
    if let Some(bounds) = plan.preview_box {
        let sx = image.width / plan.source_pdf_size.width;
        let sy = image.height / plan.source_pdf_size.height;
        image.x += bounds.left * sx;
        image.y += (plan.source_pdf_size.height - bounds.top) * sy;
        image.width = bounds.width * sx;
        image.height = bounds.height * sy;
    }
    let rotation = (layout.rotation_degrees + if rotate_back { 180 } else { 0 }) % 360;
    let bleed = plan.bleed_amount;
    let clip = PreviewRect {
        x: cut.x - bleed,
        y: cut.y - bleed,
        width: cut.width + 2.0 * bleed,
        height: cut.height + 2.0 * bleed,
    };
    (
        cut,
        clip,
        PreviewImageRect {
            rect: image,
            transform: (rotation != 0).then(|| format!("rotate({rotation} {center_x} {center_y})")),
        },
    )
}

pub fn effective_artwork_fit(request: &LayoutRequest, page: Option<usize>) -> ArtworkFit {
    page.and_then(|page| {
        request
            .page_overrides
            .iter()
            .find(|value| value.page_number == page)
    })
    .and_then(|value| value.artwork_fit)
    .or(request.artwork_fit)
    .unwrap_or_default()
}

pub fn wire_layout_request(request: &LayoutRequest) -> Result<String, serde_json::Error> {
    let mut wire = request.clone();
    if wire.source_id.is_some() {
        wire.source_pages = None;
    }
    serde_json::to_string(&wire)
}

pub fn update_finished_override(
    request: &mut LayoutRequest,
    page_number: usize,
    size: Option<SizeInches>,
) {
    if let Some(value) = request
        .page_overrides
        .iter_mut()
        .find(|value| value.page_number == page_number)
    {
        value.finished_cut_size = size;
    } else if size.is_some() {
        request.page_overrides.push(PageOverride {
            page_number,
            finished_cut_size: size,
            artwork_fit: None,
        });
    }
    request
        .page_overrides
        .retain(|value| value.finished_cut_size.is_some() || value.artwork_fit.is_some());
}

pub fn drag_crop_position(
    start: ArtworkPosition,
    delta: ArtworkPosition,
    travel: ArtworkPosition,
) -> ArtworkPosition {
    let axis = |start: f64, delta: f64, travel: f64| {
        if travel.is_finite() && delta.is_finite() && travel.abs() > 0.001 {
            (start + delta / travel).clamp(0.0, 1.0)
        } else {
            start
        }
    };
    ArtworkPosition {
        x: axis(start.x, delta.x, travel.x),
        y: axis(start.y, delta.y, travel.y),
    }
}

pub fn update_artwork_fit(request: &mut LayoutRequest, page: Option<usize>, fit: ArtworkFit) {
    if let Some(page_number) = page {
        if let Some(value) = request
            .page_overrides
            .iter_mut()
            .find(|value| value.page_number == page_number)
        {
            value.artwork_fit = Some(fit);
        } else {
            request.page_overrides.push(PageOverride {
                page_number,
                finished_cut_size: None,
                artwork_fit: Some(fit),
            });
        }
    } else {
        request.artwork_fit = Some(fit);
    }
}

fn centered(container: PreviewRect, width: f64, height: f64) -> PreviewRect {
    PreviewRect {
        x: container.x + (container.width - width) / 2.0,
        y: container.y + (container.height - height) / 2.0,
        width,
        height,
    }
}

fn rotate_box(value: PdfBox, source: SizeInches, degrees: u16) -> PdfBox {
    if degrees != 90 {
        return value;
    }
    PdfBox {
        left: value.bottom,
        bottom: source.width - value.right,
        right: value.top,
        top: source.width - value.left,
        width: value.height,
        height: value.width,
    }
}

pub fn size_label(size: SizeInches) -> String {
    format!(
        "{} × {} in",
        trim_number(size.width),
        trim_number(size.height)
    )
}

#[cfg(test)]
pub fn print_ready_facts(layout: &LayoutResult) -> String {
    let pages = if layout.duplex.is_some() {
        layout.sheets_required.saturating_mul(2)
    } else {
        layout.sheets_required
    };
    let sides = if layout.duplex.is_some() {
        "double-sided"
    } else {
        "single-sided"
    };
    format!(
        "{pages} PDF {} · {} {} · {} per sheet · {} finished {} · {} · {sides}",
        plural_noun(pages, "page", "pages"),
        layout.sheets_required,
        plural_noun(layout.sheets_required, "sheet", "sheets"),
        layout.pieces_per_sheet,
        layout.total_pieces_produced,
        plural_noun(layout.total_pieces_produced, "piece", "pieces"),
        size_label(layout.parent_sheet_size),
    )
}

#[cfg(test)]
fn plural_noun(count: usize, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

fn trim_number(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value:.4}").trim_end_matches('0').to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_input_accepts_decimal_drafts_and_rejects_invalid_values() {
        assert_eq!(parse_numeric_input("4.", 0.01, 100.0, 0.01), Some(4.0));
        assert_eq!(parse_numeric_input("4.25", 0.01, 100.0, 0.01), Some(4.25));
        assert_eq!(parse_numeric_input("0", 0.01, 100.0, 0.01), None);
        assert_eq!(parse_numeric_input("100.01", 0.01, 100.0, 0.01), None);
        assert_eq!(parse_numeric_input("not a number", 0.01, 100.0, 0.01), None);
        assert_eq!(parse_numeric_input("1.5", 0.0, 100.0, 1.0), None);
    }

    #[test]
    fn numeric_draft_sync_preserves_partial_decimal_input() {
        assert!(numeric_draft_matches_value("4.", 4.0, 0.01, 100.0, 0.01));
        assert!(numeric_draft_matches_value(
            "4.250", 4.25, 0.01, 100.0, 0.01
        ));
        assert!(!numeric_draft_matches_value("4.2e", 4.0, 0.01, 100.0, 0.01));
        assert!(!numeric_draft_matches_value("4", 4.25, 0.01, 100.0, 0.01));
    }

    #[test]
    fn decimal_finished_size_survives_validation_and_request_signatures() {
        let mut request = initial_request();
        request.finished_dimensions_chosen = [true; 2];
        request.finished_cut_size = SizeInches {
            width: 4.25,
            height: 6.25,
        };

        assert_eq!(request_issue(&request), None);
        let signature = request_signature(&request).unwrap_or_else(|error| unreachable!("{error}"));
        assert!(signature.contains("\"width\":4.25"));
        assert!(signature.contains("\"height\":6.25"));
    }

    fn result() -> LayoutResult {
        serde_json::from_value(serde_json::json!({
            "sourcePdfSize":{"width":3.5,"height":2.0},"sourceTrimBox":null,"sourcePageCount":4,"finishedCutSize":{"width":3.5,"height":2.0},"parentSheetSize":{"width":12.0,"height":18.0},"quantityRequested":4,"impositionMode":"unique","impressionQuantities":null,"orientationPreference":"auto","impressionsRequested":4,"piecesPerSheet":2,"sheetsRequired":2,"totalPiecesProduced":4,"extraPiecesProduced":0,"unusedPositions":0,"wastePercent":0.0,"rotationDegrees":0,"rows":1,"columns":2,"margins":{"top":1.0,"right":1.0,"bottom":1.0,"left":1.0},"gutters":{"horizontal":0.25,"vertical":0.25},"placements":[{"index":0,"row":0,"column":0,"x":1.0,"y":1.0,"width":3.5,"height":2.0,"finishedX":1.0,"finishedY":1.0,"finishedWidth":3.5,"finishedHeight":2.0},{"index":1,"row":0,"column":1,"x":4.75,"y":1.0,"width":3.5,"height":2.0,"finishedX":4.75,"finishedY":1.0,"finishedWidth":3.5,"finishedHeight":2.0}],"duplex":null,"bleed":{"option":"useAsIs","detectedAmountPerSide":0.0,"effectiveAmountPerSide":0.0,"source":"none","sourceLargerThanCut":false},"createdBleedAmount":0.125,"warnings":[]
        })).unwrap_or_else(|error| unreachable!("valid fixture: {error}"))
    }

    fn anchored_plan() -> PagePlan {
        PagePlan {
            page_number: 1,
            source_pdf_size: SizeInches {
                width: 20.0,
                height: 10.0,
            },
            finished_cut_size: SizeInches {
                width: 2.0,
                height: 1.5,
            },
            bleed_amount: 0.125,
            artwork: ArtworkRect {
                x: -0.7,
                y: -0.125,
                width: 3.5,
                height: 1.75,
            },
            preview_box: None,
            position_travel: Some(ArtworkPosition { x: -1.5, y: -0.25 }),
        }
    }

    #[test]
    fn manual_bleed_and_clear_select_distinct_rasters_without_reusing_old_views() {
        let default_key = preview_raster_identity("source", None);
        let manual_key = preview_raster_identity("source", Some(0.125));
        assert_eq!(default_key, "source");
        assert_ne!(default_key, manual_key);
        assert_ne!(manual_key, preview_raster_identity("source", Some(0.25)));
        assert_eq!(default_key, preview_raster_identity("source", None));
        let default_view = preview_view(&default_key, vec![1], 0, PreviewSide::Front, 0);
        let manual_view = preview_view(&manual_key, vec![1], 0, PreviewSide::Front, 0);
        assert_eq!(
            presented_preview_view(Some(&default_view), &manual_view, 0),
            None
        );
        assert_eq!(
            presented_preview_view(Some(&manual_view), &default_view, 0),
            None
        );
        let pending = PreviewLifecycle::loading(manual_view.clone(), 0);
        assert!(!pending.accepts_completion(&default_view));
        let retry = preview_view(&manual_key, vec![1], 1, PreviewSide::Front, 0);
        assert!(!pending.accepts_completion(&retry));
        let mut layout = result();
        let initial_identity =
            preview_layout_identity(&layout).unwrap_or_else(|error| unreachable!("{error}"));
        layout.source_bleed_override = Some(0.125);
        assert_ne!(
            initial_identity,
            preview_layout_identity(&layout).unwrap_or_else(|error| unreachable!("{error}"))
        );
    }

    #[test]
    fn new_requests_default_to_contain_and_keep_source_metadata_off_the_wire() {
        let mut request = initial_request();
        assert_eq!(request.artwork_fit, Some(ArtworkFit::default()));
        request.source_id = Some("prepared-source".into());
        request.source_pages = Some(vec![
            SourcePage {
                source_pdf_size: request.source_pdf_size,
                source_trim_box: None,
                physical_size_assumed: true,
                preview_box: None,
                filename: None,
                original_page_number: None,
            };
            2000
        ]);
        let wire = wire_layout_request(&request).unwrap_or_else(|error| unreachable!("{error}"));
        assert!(wire.len() < 2000);
        assert!(!wire.contains("sourcePages"));
        assert!(wire.contains("prepared-source"));
        assert_eq!(request.source_pages.as_ref().map(Vec::len), Some(2000));
    }

    #[test]
    fn drag_uses_signed_backend_travel_and_does_not_move_a_locked_axis() {
        let next = drag_crop_position(
            ArtworkPosition::default(),
            ArtworkPosition { x: 25.0, y: 200.0 },
            ArtworkPosition { x: -100.0, y: 0.0 },
        );
        assert_eq!(next, ArtworkPosition { x: 0.25, y: 0.5 });
        let clamped = drag_crop_position(
            next,
            ArtworkPosition { x: -500.0, y: 1.0 },
            ArtworkPosition {
                x: -100.0,
                y: f64::NAN,
            },
        );
        assert_eq!(clamped, ArtworkPosition { x: 1.0, y: 0.5 });
    }

    #[test]
    fn image_analysis_never_chooses_dimensions_and_preserves_explicit_replacement_settings() {
        let analysis: PdfAnalysis = serde_json::from_value(serde_json::json!({
            "filename":"image.png","pageCount":1,"sourcePdfSize":{"width":9.0,"height":3.0},"orientation":"landscape",
            "mediaBox":null,"cropBox":null,"bleedBox":null,"trimBox":null,
            "likelyBleed":{"detected":false,"amountPerSide":0.0,"horizontal":0.0,"vertical":0.0,"notes":[]},
            "matchedPresetId":null,"suggestedFinishedCutSize":null,"appearsDuplex":false,"warnings":[],
            "sourcePages":[{"sourcePdfSize":{"width":9.0,"height":3.0},"sourceTrimBox":null,"physicalSizeAssumed":true,"previewBox":null,"filename":"image.png","originalPageNumber":1}]
        })).unwrap_or_else(|error| unreachable!("{error}"));
        let mut request = request_from_analysis(initial_request(), &analysis);
        assert_eq!(
            request.finished_cut_size,
            initial_request().finished_cut_size
        );
        assert_eq!(request.finished_dimensions_chosen, [false; 2]);
        assert!(request
            .source_pages
            .as_ref()
            .is_some_and(|pages| pages[0].physical_size_assumed));
        request.finished_cut_size = SizeInches {
            width: 4.0,
            height: 6.0,
        };
        request.finished_dimensions_chosen = [true; 2];
        let fit = ArtworkFit {
            mode: ArtworkFitMode::Cover,
            position: ArtworkPosition { x: 0.25, y: 0.8 },
        };
        request.artwork_fit = Some(fit);
        let request = request_with_manual_source_bleed(request, 0.125)
            .unwrap_or_else(|| unreachable!("valid manual bleed"));
        assert_eq!(request.source_pdf_size, analysis.source_pdf_size);
        assert_eq!(request_issue_with_analysis(&request, Some(&analysis)), None);
        let replaced = request_from_analysis(request.clone(), &analysis);
        assert_eq!(replaced.finished_cut_size, request.finished_cut_size);
        assert_eq!(replaced.finished_dimensions_chosen, [true; 2]);
        let restored = restore_request_from_analysis(request, &analysis, false);
        assert_eq!(restored.finished_dimensions_chosen, [true; 2]);
        assert_eq!(
            restored.finished_cut_size,
            SizeInches {
                width: 4.0,
                height: 6.0
            }
        );
        assert_eq!(restored.artwork_fit, Some(fit));
        assert_eq!(restored.source_bleed_override, Some(0.125));
    }

    #[test]
    fn pdf_analysis_does_not_accept_source_geometry_as_finished_intent() {
        let mut analysis: PdfAnalysis = serde_json::from_value(serde_json::json!({"filename":"cards.pdf","pageCount":3,"sourcePdfSize":{"width":3.75,"height":2.25},"orientation":"landscape","mediaBox":null,"cropBox":null,"bleedBox":null,"trimBox":null,"likelyBleed":{"detected":false,"amountPerSide":0.0,"horizontal":0.0,"vertical":0.0,"notes":[]},"matchedPresetId":null,"suggestedFinishedCutSize":null,"appearsDuplex":false,"warnings":[]})).unwrap_or_else(|error| unreachable!("valid fixture: {error}"));
        analysis.suggested_finished_cut_size = Some(SizeInches {
            width: 8.5,
            height: 11.0,
        });
        let mut request = request_from_analysis(initial_request(), &analysis);
        assert_eq!(request.finished_dimensions_chosen, [false; 2]);
        assert_eq!(
            request.finished_cut_size,
            initial_request().finished_cut_size
        );
        request.finished_dimensions_chosen = [true, false];
        request.finished_cut_size.width = 4.0;
        let restored = restore_request_from_analysis(request, &analysis, false);
        assert_eq!(restored.finished_dimensions_chosen, [true, false]);
        assert_eq!(restored.finished_cut_size.width, 4.0);
    }

    #[test]
    fn stretch_serializes_for_shared_and_per_page_fitting_without_changing_crop_position() {
        let mut request = initial_request();
        let fit = ArtworkFit {
            mode: ArtworkFitMode::Stretch,
            position: ArtworkPosition { x: 0.2, y: 0.8 },
        };
        update_artwork_fit(&mut request, None, fit);
        update_artwork_fit(&mut request, Some(1), fit);
        request.finished_dimensions_chosen = [true; 2];
        let wire = serde_json::to_value(&request).unwrap_or_else(|error| unreachable!("{error}"));
        assert_eq!(wire["artworkFit"]["mode"], "stretch");
        assert_eq!(wire["pageOverrides"][0]["artworkFit"]["mode"], "stretch");
        assert!(wire.get("finishedDimensionsChosen").is_none());
        assert!(wire.get("finished_dimensions_chosen").is_none());
        assert_eq!(effective_artwork_fit(&request, Some(1)), fit);
        assert_eq!(
            serde_json::from_value::<ArtworkFitMode>(serde_json::json!("stretch")).ok(),
            Some(ArtworkFitMode::Stretch)
        );
        assert_eq!(
            serde_json::from_value::<FinishedSizeMode>(serde_json::json!("original")).ok(),
            Some(FinishedSizeMode::Original)
        );
    }

    #[test]
    fn preview_uses_exact_backend_anchor_without_refitting_odd_artwork() {
        let layout = result();
        let plan = anchored_plan();
        let (cut, clip, image) =
            planned_preview_geometry(&layout, &layout.placements[0], &plan, PreviewSide::Front);
        assert_eq!(
            cut,
            PreviewRect {
                x: 1.75,
                y: 1.25,
                width: 2.0,
                height: 1.5
            }
        );
        assert_eq!(
            clip,
            PreviewRect {
                x: 1.625,
                y: 1.125,
                width: 2.25,
                height: 1.75
            }
        );
        assert!((image.rect.x - 1.05).abs() < 1e-9);
        assert_eq!(image.rect.y, 1.125);
        assert_eq!(image.rect.width, plan.artwork.width);
        assert_eq!(image.rect.height, plan.artwork.height);
        assert_eq!(image.transform, None);
        assert_eq!(
            image.rect.width / image.rect.height,
            plan.source_pdf_size.width / plan.source_pdf_size.height
        );
    }

    #[test]
    fn preview_raster_crop_uses_backend_preview_box_top_origin() {
        let layout = result();
        let mut plan = anchored_plan();
        plan.preview_box = Some(PdfBox {
            left: 2.0,
            bottom: 1.0,
            right: 18.0,
            top: 8.0,
            width: 16.0,
            height: 7.0,
        });
        let (_, _, image) =
            planned_preview_geometry(&layout, &layout.placements[0], &plan, PreviewSide::Front);
        assert!((image.rect.x - 1.4).abs() < 1e-9);
        assert!((image.rect.y - 1.475).abs() < 1e-9);
        assert!((image.rect.width - 2.8).abs() < 1e-9);
        assert!((image.rect.height - 1.225).abs() < 1e-9);
    }

    #[test]
    fn quarter_turn_rotates_whole_anchored_composition_about_cut_center() {
        let mut layout = result();
        layout.rotation_degrees = 90;
        let plan = anchored_plan();
        let (cut, _, image) =
            planned_preview_geometry(&layout, &layout.placements[0], &plan, PreviewSide::Front);
        assert_eq!(cut.width, plan.finished_cut_size.height);
        assert_eq!(cut.height, plan.finished_cut_size.width);
        assert_eq!(image.transform, Some("rotate(90 2.75 2)".into()));
        assert_eq!(image.rect.width, plan.artwork.width);
        assert_eq!(image.rect.height, plan.artwork.height);
    }

    #[test]
    fn duplex_registers_each_cut_without_reversing_asymmetric_crop_anchor() {
        let mut layout = result();
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });
        let plan = anchored_plan();
        let (front, _, front_image) =
            planned_preview_geometry(&layout, &layout.placements[0], &plan, PreviewSide::Front);
        let (back, _, back_image) =
            planned_preview_geometry(&layout, &layout.placements[0], &plan, PreviewSide::Back);
        assert_eq!(
            back.x,
            layout.parent_sheet_size.width - front.x - front.width
        );
        assert_eq!(back.y, front.y);
        assert!((front_image.rect.x - front.x - (back_image.rect.x - back.x)).abs() < 1e-9);
        assert_eq!(front_image.rect.y - front.y, back_image.rect.y - back.y);
        if let Some(value) = layout.duplex.as_mut() {
            value.rotate_back_180 = true;
        }
        let (rotated_cut, _, rotated_image) =
            planned_preview_geometry(&layout, &layout.placements[0], &plan, PreviewSide::Back);
        assert_eq!(rotated_cut.x, front.x);
        assert_eq!(
            rotated_cut.y,
            layout.parent_sheet_size.height - front.y - front.height
        );
        assert_eq!(rotated_image.transform, Some("rotate(180 2.75 16)".into()));
    }

    #[test]
    fn fitting_and_per_piece_size_changes_preserve_quantities_order_and_other_overrides() {
        let mut request = initial_request();
        request.source_page_count = Some(3);
        request.imposition_mode = ImpositionMode::Repeat;
        request.impression_quantities = Some(vec![4, 1, 7]);
        request.quantity_requested = 12;
        let fit = ArtworkFit {
            mode: ArtworkFitMode::Cover,
            position: ArtworkPosition { x: 0.1, y: 0.9 },
        };
        update_artwork_fit(&mut request, Some(2), fit);
        update_finished_override(
            &mut request,
            2,
            Some(SizeInches {
                width: 8.5,
                height: 11.0,
            }),
        );
        update_artwork_fit(&mut request, None, ArtworkFit::default());
        assert_eq!(effective_artwork_fit(&request, Some(2)), fit);
        assert_eq!(request.impression_quantities, Some(vec![4, 1, 7]));
        assert_eq!(request.quantity_requested, 12);
        update_finished_override(&mut request, 2, None);
        assert_eq!(effective_artwork_fit(&request, Some(2)), fit);
        assert_eq!(request.page_overrides.len(), 1);
    }

    #[test]
    fn incomplete_or_nonfinite_backend_page_plans_are_rejected() {
        let mut layout = result();
        layout.page_plans = vec![anchored_plan()];
        assert!(validate_layout_result(&layout).is_err());
        layout.source_page_count = Some(1);
        assert!(validate_layout_result(&layout).is_ok());
        layout.page_plans[0].artwork.x = f64::NAN;
        assert!(validate_layout_result(&layout).is_err());
    }

    fn preview_view(
        source_id: &str,
        pages: Vec<usize>,
        retry_generation: u64,
        side: PreviewSide,
        sheet_index: usize,
    ) -> PreviewViewIdentity {
        PreviewViewIdentity {
            source_id: source_id.into(),
            layout_identity: "layout-a".into(),
            pages,
            retry_generation,
            side,
            sheet_index,
            sheet_count: 2,
        }
    }

    #[test]
    fn preview_identity_rejects_completions_from_an_old_layout_epoch() {
        let current = preview_view("source", vec![1, 2], 0, PreviewSide::Front, 0);
        let mut stale = current.clone();
        stale.layout_identity = "layout-before-resize".into();
        let lifecycle = PreviewLifecycle::Loading {
            view: current,
            completed: 0,
        };

        assert!(!lifecycle.accepts_completion(&stale));
        assert!(!lifecycle.accepts_failure(&stale));
    }

    #[test]
    fn navigation_label_names_the_exact_sheet_and_side() {
        assert_eq!(
            preview_navigation_label(1, 4, PreviewSide::Back),
            "Sheet 2 of 4, back side imposition preview"
        );
        assert_eq!(
            preview_navigation_label(99, 0, PreviewSide::Front),
            "Sheet 1 of 1, front side imposition preview"
        );

        let requested = preview_view("source", vec![3, 4], 0, PreviewSide::Back, 1);
        let presented = preview_view("source", vec![1, 2], 0, PreviewSide::Front, 0);
        assert_eq!(
            preview_loading_label(&requested, Some(&presented), 1),
            "Loading Sheet 2 of 2, back side imposition preview: 1 of 2 artwork pages decoded. Sheet 1 of 2, front side imposition preview remains visible."
        );
    }

    #[test]
    fn analysis_rebases_page_and_cut_rules() {
        let analysis: PdfAnalysis = serde_json::from_value(serde_json::json!({"filename":"cards.pdf","pageCount":3,"sourcePdfSize":{"width":3.75,"height":2.25},"orientation":"landscape","mediaBox":null,"cropBox":null,"bleedBox":null,"trimBox":null,"likelyBleed":{"detected":false,"amountPerSide":0.0,"horizontal":0.0,"vertical":0.0,"notes":[]},"matchedPresetId":null,"suggestedFinishedCutSize":{"width":3.5,"height":2.0},"appearsDuplex":false,"warnings":[]})).unwrap_or_else(|error| unreachable!("valid fixture: {error}"));
        let mut request = initial_request();
        request.sides = Sides::Double;
        request.impression_quantities = Some(vec![6]);
        request.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });
        let rebased = request_from_analysis(request, &analysis);
        assert_eq!(
            rebased.finished_cut_size,
            SizeInches {
                width: 3.5,
                height: 2.0
            }
        );
        assert_eq!(rebased.sides, Sides::Single);
        assert!(rebased.duplex.is_none());
    }

    #[test]
    fn validation_covers_exact_quantities_grid_and_duplex() {
        let mut request = initial_request();
        request.source_page_count = Some(3);
        request.imposition_mode = ImpositionMode::Repeat;
        request.impression_quantities = Some(vec![1, 2]);
        assert_eq!(
            request_issue(&request).as_deref(),
            Some("Repeat quantities do not match the PDF pages.")
        );
        request.impression_quantities = Some(vec![1, 2, 3]);
        request.layout_mode = LayoutMode::Manual;
        request.manual = Some(ManualLayout {
            rows: 23,
            columns: 23,
            rotation_degrees: 0,
            margins: None,
        });
        assert_eq!(
            request_issue(&request).as_deref(),
            Some("Custom n-up is limited to 512 placements.")
        );
        request.manual = Some(ManualLayout {
            rows: 2,
            columns: 2,
            rotation_degrees: 0,
            margins: None,
        });
        request.sides = Sides::Double;
        request.impression_quantities = Some(vec![6]);
        request.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });
        assert_eq!(
            request_issue(&request).as_deref(),
            Some("Double-sided imposition requires an even number of PDF pages.")
        );
    }

    #[test]
    fn stale_lifecycle_results_are_rejected_and_last_good_is_retained() {
        let loading = PreparationLifecycle::Preparing { request_id: 8 };
        assert!(loading.accepts(8));
        assert!(!loading.accepts(7));
        let previous = result();
        let failed = LayoutLifecycle::Failed {
            signature: "next".into(),
            message: "offline".into(),
            previous: Some(previous.clone()),
        };
        assert_eq!(failed.last_good(), Some(&previous));
        let invalid = LayoutLifecycle::Invalid {
            previous: Some(previous.clone()),
        };
        assert_eq!(invalid.last_good(), Some(&previous));
        let current = LayoutLifecycle::Loading {
            request_id: 4,
            signature: "current".into(),
            previous: None,
        };
        assert!(current.accepts(4, "current"));
        assert!(!current.accepts(3, "current"));
    }

    #[test]
    fn print_ready_facts_pluralize_job_counts() {
        let mut layout = result();
        layout.sheets_required = 1;
        layout.total_pieces_produced = 1;
        assert_eq!(
            print_ready_facts(&layout),
            "1 PDF page · 1 sheet · 2 per sheet · 1 finished piece · 12 × 18 in · single-sided"
        );

        layout.sheets_required = 2;
        layout.total_pieces_produced = 4;
        assert_eq!(
            print_ready_facts(&layout),
            "2 PDF pages · 2 sheets · 2 per sheet · 4 finished pieces · 12 × 18 in · single-sided"
        );
    }

    #[test]
    fn transient_layout_failure_can_retry_same_request_and_accept_success() {
        let previous = result();
        let failed = LayoutLifecycle::Failed {
            signature: "same-request".into(),
            message: "temporary network failure".into(),
            previous: Some(previous.clone()),
        };
        assert_eq!(failed.last_good(), Some(&previous));

        let retry = LayoutLifecycle::Loading {
            request_id: 2,
            signature: "same-request".into(),
            previous: failed.last_good().cloned(),
        };
        assert!(retry.accepts(2, "same-request"));
        let succeeded = LayoutLifecycle::Ready {
            signature: "same-request".into(),
            layout: previous.clone(),
        };
        assert_eq!(succeeded.last_good(), Some(&previous));
    }

    #[test]
    fn preview_page_assignment_spans_sheets_and_duplex_pairs() {
        let mut layout = result();
        assert_eq!(
            preview_page_number(&layout, 1, PreviewSide::Front, 1),
            Some(4)
        );
        layout.imposition_mode = ImpositionMode::Repeat;
        layout.impression_quantities = Some(vec![2, 1]);
        layout.impressions_requested = 3;
        layout.source_page_count = Some(4);
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });
        assert_eq!(
            preview_page_number(&layout, 0, PreviewSide::Front, 1),
            Some(3)
        );
        assert_eq!(
            preview_page_number(&layout, 0, PreviewSide::Back, 1),
            Some(4)
        );
    }

    #[test]
    fn preview_geometry_clips_at_gutter_midpoints_and_handles_fit_inside() {
        let mut layout = result();
        let clip = preview_clip_rect(&layout, &layout.placements[0]);
        assert_eq!(clip.x, 0.0);
        assert_eq!(clip.width, 4.625);
        layout.bleed.option = BleedOption::FitInside;
        layout.source_pdf_size = SizeInches {
            width: 4.0,
            height: 4.0,
        };
        let artwork = preview_artwork_rect(&layout, &layout.placements[0]);
        assert_eq!(artwork.width, 2.0);
        assert_eq!(artwork.x, 1.75);
    }

    #[test]
    fn scale_to_bleed_preview_resizes_odd_source_without_cropping() {
        let mut layout = result();
        layout.bleed.option = BleedOption::ScaleToBleed;
        layout.bleed.effective_amount_per_side = 0.0;
        layout.created_bleed_amount = 0.125;
        layout.source_pdf_size = SizeInches {
            width: 4.0,
            height: 3.0,
        };
        let placement = &layout.placements[0];
        let artwork = preview_artwork_rect(&layout, placement);

        assert_eq!(artwork.x, placement.finished_x - 0.125);
        assert_eq!(artwork.y, placement.finished_y - 0.125);
        assert_eq!(artwork.width, placement.finished_width + 0.25);
        assert_eq!(artwork.height, placement.finished_height + 0.25);
    }

    #[test]
    fn server_shapes_are_typed_and_request_names_match_backend() {
        let encoded = serde_json::to_value(initial_request())
            .unwrap_or_else(|error| unreachable!("serializable fixture: {error}"));
        assert_eq!(encoded["layoutMode"], "maxPieces");
        assert_eq!(encoded["bleedOption"], "useAsIs");
        assert!(serde_json::from_value::<PreparedPdfSource>(
            serde_json::json!({"sourceId":7,"analysis":{}})
        )
        .is_err());
    }

    #[test]
    fn preview_maps_media_raster_and_preserves_rotation_transform() {
        let selected = PreviewImageRect {
            rect: PreviewRect {
                x: 10.0,
                y: 20.0,
                width: 3.5,
                height: 2.0,
            },
            transform: Some("rotate(90 2.5 3)".into()),
        };
        let mapped = preview_source_image_rect(
            selected,
            PdfBox {
                left: -0.25,
                bottom: -0.125,
                right: 3.75,
                top: 2.125,
                width: 4.0,
                height: 2.25,
            },
            PdfBox {
                left: 0.0,
                bottom: 0.0,
                right: 3.5,
                top: 2.0,
                width: 3.5,
                height: 2.0,
            },
        );
        assert_eq!(mapped.rect.x, 9.75);
        assert_eq!(mapped.rect.y, 19.875);
        assert_eq!(mapped.rect.width, 4.0);
        assert_eq!(mapped.rect.height, 2.25);
        assert_eq!(mapped.transform.as_deref(), Some("rotate(90 2.5 3)"));
    }

    #[test]
    fn preview_rotates_ninety_degree_and_back_content_around_artwork_center() {
        let mut layout = result();
        layout.rotation_degrees = 90;
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: true,
            back_alignment: String::new(),
        });
        let image = preview_artwork_image(
            &layout,
            PreviewRect {
                x: 1.0,
                y: 2.0,
                width: 3.0,
                height: 2.0,
            },
            PreviewSide::Back,
        );
        assert_eq!(image.rect.x, 1.5);
        assert_eq!(image.rect.y, 1.5);
        assert_eq!(image.rect.width, 2.0);
        assert_eq!(image.rect.height, 3.0);
        assert_eq!(image.transform.as_deref(), Some("rotate(270 2.5 3)"));
    }

    #[test]
    fn asymmetric_trim_box_keeps_detected_bleed_offset() {
        let mut layout = result();
        layout.source_pdf_size = SizeInches {
            width: 3.75,
            height: 2.25,
        };
        layout.source_trim_box = Some(PdfBox {
            left: 0.2,
            bottom: 0.1,
            right: 3.7,
            top: 2.1,
            width: 3.5,
            height: 2.0,
        });
        layout.bleed.option = BleedOption::ScaleToBleed;
        layout.bleed.effective_amount_per_side = 0.1;
        layout.bleed.source = BleedSource::Detected;
        let placement = PiecePlacement {
            x: -0.2,
            y: -0.15,
            width: 3.75,
            height: 2.25,
            finished_x: 0.0,
            finished_y: 0.0,
            finished_width: 3.5,
            ..layout.placements[0].clone()
        };
        let artwork = preview_artwork_rect(&layout, &placement);
        assert!((artwork.x + 0.2).abs() < 1e-10);
        assert!((artwork.y + 0.15).abs() < 1e-10);
        assert!((artwork.width - 3.75).abs() < 1e-10);
        assert!((artwork.height - 2.25).abs() < 1e-10);
    }

    #[test]
    fn preview_page_sets_cover_unique_repeat_duplex_and_unused_slots() {
        let mut layout = result();
        layout.source_page_count = Some(6);
        layout.impressions_requested = 3;
        assert_eq!(
            preview_page_numbers(&layout, PreviewSide::Front, 0),
            vec![1, 2]
        );
        assert_eq!(
            preview_page_numbers(&layout, PreviewSide::Front, 1),
            vec![3]
        );
        assert_eq!(preview_page_number(&layout, 1, PreviewSide::Front, 1), None);

        layout.imposition_mode = ImpositionMode::Repeat;
        layout.impression_quantities = Some(vec![2, 0, 1]);
        assert_eq!(
            preview_page_numbers(&layout, PreviewSide::Front, 0),
            vec![1]
        );
        assert_eq!(
            preview_page_number(&layout, 0, PreviewSide::Front, 1),
            Some(3)
        );
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });
        layout.source_page_count = Some(6);
        assert_eq!(
            preview_page_number(&layout, 0, PreviewSide::Back, 1),
            Some(6)
        );
    }

    #[test]
    fn duplex_mirroring_follows_physical_sheet_edge_and_back_rotation() {
        let mut layout = result();
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });
        assert_eq!(duplex_mirror_axes(&layout), (true, false));
        layout.parent_sheet_size = SizeInches {
            width: 18.0,
            height: 12.0,
        };
        assert_eq!(duplex_mirror_axes(&layout), (false, true));
        if let Some(duplex) = layout.duplex.as_mut() {
            duplex.rotate_back_180 = true;
        }
        assert_eq!(duplex_mirror_axes(&layout), (true, false));
    }

    #[test]
    fn gutter_bands_only_cover_real_adjacent_gaps() {
        assert_eq!(
            gutter_bands([0.0, 3.0, 3.25, 6.25, 9.0], 0.25),
            vec![GutterBand {
                position: 3.0,
                size: 0.25
            }]
        );
        assert!(gutter_bands([0.0, 3.0, 3.25], 0.0).is_empty());
        assert_eq!(unique_positions([3.0, 1.0, 1.00001]), vec![1.0, 3.0]);
    }

    #[test]
    fn preparation_progress_is_monotonic_and_bounds_malformed_values() {
        let values = [
            initial_preparation_percent(InitialPreparationPhase::Analysis { percent: 0.0 }),
            initial_preparation_percent(InitialPreparationPhase::Analysis { percent: 50.0 }),
            initial_preparation_percent(InitialPreparationPhase::Analysis { percent: 100.0 }),
            initial_preparation_percent(InitialPreparationPhase::Layout),
            initial_preparation_percent(InitialPreparationPhase::Artwork {
                completed: 0,
                total: 10,
            }),
            initial_preparation_percent(InitialPreparationPhase::Artwork {
                completed: 4,
                total: 10,
            }),
            initial_preparation_percent(InitialPreparationPhase::Artwork {
                completed: 10,
                total: 10,
            }),
            initial_preparation_percent(InitialPreparationPhase::Ready),
        ];
        assert_eq!(values, [0, 23, 45, 50, 55, 73, 99, 100]);
        assert_eq!(
            initial_preparation_percent(InitialPreparationPhase::Analysis { percent: f64::NAN }),
            0
        );
        assert_eq!(
            initial_preparation_percent(InitialPreparationPhase::Artwork {
                completed: 5,
                total: 0
            }),
            55
        );
    }

    #[test]
    fn initial_preview_gates_on_every_page_used_by_the_first_sheet() {
        let mut layout = result();
        layout.source_page_count = Some(6);
        layout.impressions_requested = 3;
        assert_eq!(initial_preview_pages(&layout), vec![1, 2]);

        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });
        layout.source_page_count = Some(12);
        assert_eq!(initial_preview_pages(&layout), vec![1, 3]);
        assert_eq!(all_preview_pages(&layout), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn preview_failure_retains_completed_pages_and_retry_can_finish() {
        let current = preview_view("current", vec![1, 2, 3], 0, PreviewSide::Front, 0);
        let mut state = PreviewLifecycle::Loading {
            view: current.clone(),
            completed: 0,
        };
        let old = preview_view("old", vec![1, 2, 3], 0, PreviewSide::Front, 0);
        state.record_completed(&old, 2);
        assert_eq!(state.completed(), 0);
        state.record_completed(&current, 1);
        state.record_failure(&current, 1, "render failed".into());
        assert!(state.is_usable_for(&current));
        assert!(!state.is_usable_for(&old));
        assert!(matches!(
            state,
            PreviewLifecycle::Failed { completed: 1, .. }
        ));
        state.record_completed(&current, 2);
        assert!(matches!(
            state,
            PreviewLifecycle::Failed { completed: 2, .. }
        ));

        let mut retry = PreviewLifecycle::Loading {
            view: current.clone(),
            completed: 1,
        };
        retry.record_completed(&current, 3);
        assert!(matches!(retry, PreviewLifecycle::Ready { .. }));

        assert!(retry.accepts_failure(&current));
        retry.record_failure(&current, 2, "decode failed".into());
        assert!(matches!(
            retry,
            PreviewLifecycle::Failed { completed: 2, .. }
        ));
    }

    #[test]
    fn stale_decode_failure_cannot_remove_a_replacement_preview_url() {
        assert!(!should_remove_failed_preview_url(
            Some("blob:replacement"),
            "blob:stale"
        ));
        assert!(should_remove_failed_preview_url(
            Some("blob:current"),
            "blob:current"
        ));
        assert!(!should_remove_failed_preview_url(None, "blob:removed"));
    }

    #[test]
    fn preview_navigation_settles_only_between_uncached_views() {
        let first = preview_view("source", vec![1, 2], 0, PreviewSide::Front, 0);
        let next = preview_view("source", vec![3, 4], 0, PreviewSide::Front, 1);
        let other_source = preview_view("other", vec![3, 4], 0, PreviewSide::Front, 1);
        let ready = PreviewLifecycle::Ready {
            view: first.clone(),
        };
        let failed = PreviewLifecycle::Failed {
            view: next.clone(),
            completed: 0,
            message: "offline".into(),
        };

        assert!(should_settle_preview_navigation(&ready, &next, 2));
        assert!(!should_settle_preview_navigation(&ready, &next, 0));
        assert!(!should_settle_preview_navigation(&ready, &other_source, 2));
        assert!(!should_settle_preview_navigation(&failed, &next, 2));
        assert!(!should_settle_preview_navigation(
            &PreviewLifecycle::Idle,
            &first,
            2
        ));
    }

    #[test]
    fn preview_cache_window_stays_local_and_protects_the_presented_sheet() {
        let mut layout = result();
        layout.source_page_count = Some(24);
        layout.impressions_requested = 12;
        layout.sheets_required = 3;
        let current = preview_page_numbers(&layout, PreviewSide::Front, 1);
        let next = preview_page_numbers(&layout, PreviewSide::Front, 2);
        let expected = current
            .iter()
            .chain(&next)
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(
            preview_cache_window(&layout, PreviewSide::Front, 1),
            expected
        );
        assert!(preview_cache_window(&layout, PreviewSide::Front, 1).len() < 24);

        let presented = preview_view("source", vec![1, 2], 0, PreviewSide::Front, 0);
        let target = preview_view("source", vec![3, 4], 0, PreviewSide::Front, 1);
        let stale = preview_view("stale", vec![9], 0, PreviewSide::Front, 0);
        assert_eq!(
            preview_protected_pages(&target, Some(&presented)),
            vec![1, 2, 3, 4]
        );
        assert_eq!(preview_protected_pages(&target, Some(&stale)), vec![3, 4]);
    }

    #[test]
    fn uncached_navigation_keeps_the_composed_view_until_target_is_complete() {
        let current = preview_view("source", vec![1, 2], 0, PreviewSide::Front, 0);
        let target = preview_view("source", vec![3, 4], 0, PreviewSide::Front, 1);

        assert_eq!(
            presented_preview_view(Some(&current), &target, 0),
            Some(current.clone())
        );
        assert_eq!(
            presented_preview_view(Some(&current), &target, 1),
            Some(current)
        );
        assert_eq!(
            presented_preview_view(None, &target, 1),
            None,
            "a partial initial target is not presented as a complete sheet"
        );
        assert_eq!(presented_preview_view(None, &target, 2), Some(target));
    }

    #[test]
    fn presentation_switches_cached_targets_immediately_and_rejects_stale_sources() {
        let current = preview_view("source", vec![1], 0, PreviewSide::Front, 0);
        let cached = preview_view("source", vec![2], 0, PreviewSide::Back, 0);
        let replacement = preview_view("replacement", vec![1], 0, PreviewSide::Front, 0);

        assert_eq!(
            presented_preview_view(Some(&current), &cached, 1),
            Some(cached)
        );
        assert_eq!(
            presented_preview_view(Some(&current), &replacement, 0),
            None,
            "a previous source must never leak into the replacement job"
        );
    }

    #[test]
    fn preview_failure_availability_is_specific_and_human_readable() {
        assert_eq!(
            preview_failure_availability(0, 4),
            "No artwork pages are available for this sheet yet."
        );
        assert_eq!(
            preview_failure_availability(1, 4),
            "1 of 4 artwork pages is still visible."
        );
        assert_eq!(
            preview_failure_availability(8, 4),
            "4 of 4 artwork pages are still visible."
        );
    }

    #[test]
    fn preview_batch_decoder_requires_the_exact_requested_pages() {
        let mut payload = PREVIEW_BATCH_MAGIC.to_vec();
        payload.extend_from_slice(&2_u32.to_be_bytes());
        payload.extend_from_slice(&7_u32.to_be_bytes());
        payload.extend_from_slice(&3_u32.to_be_bytes());
        payload.extend_from_slice(&[1, 2, 3]);
        payload.extend_from_slice(&4_u32.to_be_bytes());
        payload.extend_from_slice(&2_u32.to_be_bytes());
        payload.extend_from_slice(&[8, 9]);

        assert_eq!(
            decode_preview_batch(&payload, &[4, 7]),
            Ok(vec![(7, vec![1, 2, 3]), (4, vec![8, 9])])
        );
        assert!(decode_preview_batch(&payload, &[4, 8]).is_err());
        assert!(decode_preview_batch(&payload[..payload.len() - 1], &[4, 7]).is_err());
        assert!(decode_preview_batch(&payload, &[4, 4]).is_err());
    }

    #[test]
    fn stale_preview_events_cannot_mutate_a_new_view() {
        let current = preview_view("source", vec![3, 4], 2, PreviewSide::Back, 1);
        let mut state = PreviewLifecycle::Loading {
            view: current.clone(),
            completed: 0,
        };
        for stale in [
            preview_view("old-source", vec![3, 4], 2, PreviewSide::Back, 1),
            preview_view("source", vec![1, 2], 2, PreviewSide::Back, 1),
            preview_view("source", vec![3, 4], 1, PreviewSide::Back, 1),
            preview_view("source", vec![3, 4], 2, PreviewSide::Front, 1),
            preview_view("source", vec![3, 4], 2, PreviewSide::Back, 0),
        ] {
            assert!(!state.accepts_completion(&stale));
            assert!(!state.accepts_failure(&stale));
            state.record_completed(&stale, 2);
            state.record_failure(&stale, 0, "stale failure".into());
            assert_eq!(state.completed(), 0);
        }

        assert!(state.accepts_completion(&current));
        assert!(state.accepts_failure(&current));
        state.record_completed(&current, 1);
        assert_eq!(state.completed(), 1);
    }

    #[test]
    fn manual_bleed_derives_outer_size_and_requires_available_artwork() {
        let finished = SizeInches {
            width: 2.0,
            height: 3.5,
        };
        assert_eq!(
            source_size_from_finished_bleed(finished, 0.125),
            Some(SizeInches {
                width: 2.25,
                height: 3.75
            })
        );
        let analysis: PdfAnalysis = serde_json::from_value(serde_json::json!({"filename":"job.pdf","pageCount":2,"sourcePdfSize":{"width":2.25,"height":3.75},"orientation":"portrait","mediaBox":{"left":0.0,"bottom":0.0,"right":2.25,"top":3.75,"width":2.25,"height":3.75},"cropBox":null,"bleedBox":null,"trimBox":null,"likelyBleed":{"detected":false,"amountPerSide":0.0,"horizontal":0.0,"vertical":0.0,"notes":[]},"matchedPresetId":null,"suggestedFinishedCutSize":null,"appearsDuplex":true,"warnings":[]})).unwrap_or_else(|error| unreachable!("valid fixture: {error}"));
        assert!(analysis_contains_manual_source_bleed(
            &analysis, finished, 0.125
        ));
        assert!(!analysis_contains_manual_source_bleed(
            &analysis,
            SizeInches {
                width: 2.25,
                height: 3.75
            },
            0.125
        ));
    }

    #[test]
    fn restore_preserves_cut_and_manual_bleed_when_new_analysis_contains_it() {
        let mut saved = initial_request();
        saved.finished_cut_size = SizeInches {
            width: 4.0,
            height: 6.0,
        };
        saved.source_bleed_override = Some(0.125);
        let analysis: PdfAnalysis = serde_json::from_value(serde_json::json!({"filename":"job.pdf","pageCount":2,"sourcePdfSize":{"width":4.25,"height":6.25},"orientation":"portrait","mediaBox":null,"cropBox":null,"bleedBox":null,"trimBox":null,"likelyBleed":{"detected":false,"amountPerSide":0.0,"horizontal":0.0,"vertical":0.0,"notes":[]},"matchedPresetId":null,"suggestedFinishedCutSize":{"width":4.25,"height":6.25},"appearsDuplex":true,"warnings":[]})).unwrap_or_else(|error| unreachable!("valid fixture: {error}"));
        let restored = restore_request_from_analysis(saved, &analysis, true);
        assert_eq!(
            restored.finished_cut_size,
            SizeInches {
                width: 4.0,
                height: 6.0
            }
        );
        assert_eq!(restored.source_pdf_size, analysis.source_pdf_size);
        assert_eq!(restored.source_bleed_override, Some(0.125));
    }

    #[test]
    fn workflow_normalizes_modes_quantities_and_page_pair_language() {
        assert!(!supports_duplex(Some(3)));
        assert!(supports_duplex(Some(4)));
        assert_eq!(impression_count(Some(8), Sides::Double), 4);
        assert_eq!(
            normalized_quantities(3, Sides::Single, Some(&[2]), 9),
            vec![2, 1, 1]
        );
        let mut layout = result();
        layout.imposition_mode = ImpositionMode::Repeat;
        layout.impression_quantities = Some(vec![2, 1]);
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });
        assert_eq!(page_usage(&layout), "Copies by page pair");
    }

    #[test]
    fn repeat_quantity_drafts_round_trip_between_simplex_and_duplex() {
        let mut request = initial_request();
        request.source_page_count = Some(4);
        request.imposition_mode = ImpositionMode::Repeat;
        request.impression_quantities = Some(vec![2, 3, 4, 5]);
        request.quantity_requested = 14;
        let mut drafts = RepeatQuantityDrafts::from_request(&request);

        set_sides_preserving_repeat_drafts(&mut request, &mut drafts, Sides::Double);
        assert_eq!(request.impression_quantities, Some(vec![1, 1]));
        request.impression_quantities = Some(vec![7, 8]);
        set_sides_preserving_repeat_drafts(&mut request, &mut drafts, Sides::Single);
        assert_eq!(request.impression_quantities, Some(vec![2, 3, 4, 5]));
        assert_eq!(request.quantity_requested, 14);

        set_sides_preserving_repeat_drafts(&mut request, &mut drafts, Sides::Double);
        assert_eq!(request.impression_quantities, Some(vec![7, 8]));
        assert_eq!(request.quantity_requested, 15);

        let before_unrelated_request_edit = drafts.clone();
        drafts.rebase(request.source_page_count);
        assert_eq!(drafts, before_unrelated_request_edit);
    }

    #[test]
    fn repeat_mode_selection_restores_quantities_and_exposes_duplex_pair_draft() {
        let mut request = initial_request();
        request.source_page_count = Some(4);
        request.imposition_mode = ImpositionMode::Repeat;
        request.impression_quantities = Some(vec![2, 3, 4, 5]);
        request.quantity_requested = 14;
        let mut drafts = RepeatQuantityDrafts::from_request(&request);

        set_imposition_mode_preserving_repeat_drafts(
            &mut request,
            &mut drafts,
            ImpositionMode::Unique,
        );
        assert_eq!(request.imposition_mode, ImpositionMode::Unique);
        assert_eq!(request.impression_quantities, None);

        set_imposition_mode_preserving_repeat_drafts(
            &mut request,
            &mut drafts,
            ImpositionMode::Repeat,
        );
        assert_eq!(request.imposition_mode, ImpositionMode::Repeat);
        assert_eq!(request.impression_quantities, Some(vec![2, 3, 4, 5]));
        assert_eq!(request.quantity_requested, 14);

        set_sides_preserving_repeat_drafts(&mut request, &mut drafts, Sides::Double);
        assert_eq!(request.impression_quantities, Some(vec![1, 1]));
        assert!(request.duplex.is_some());
    }

    #[test]
    fn repeat_quantity_edits_keep_independent_simplex_and_duplex_drafts() {
        let mut request = initial_request();
        request.source_page_count = Some(2);
        request.imposition_mode = ImpositionMode::Repeat;
        request.impression_quantities = Some(vec![1, 1]);
        request.quantity_requested = 2;
        let mut drafts = RepeatQuantityDrafts::from_request(&request);

        update_repeat_quantity(&mut request, &mut drafts, 0, 3);
        update_repeat_quantity(&mut request, &mut drafts, 1, 5);
        set_sides_preserving_repeat_drafts(&mut request, &mut drafts, Sides::Double);
        update_repeat_quantity(&mut request, &mut drafts, 0, 7);
        set_sides_preserving_repeat_drafts(&mut request, &mut drafts, Sides::Single);
        assert_eq!(request.impression_quantities, Some(vec![3, 5]));

        set_sides_preserving_repeat_drafts(&mut request, &mut drafts, Sides::Double);
        assert_eq!(request.impression_quantities, Some(vec![7]));
    }

    #[test]
    fn bulk_repeat_quantity_updates_total_and_active_side_draft() {
        let mut request = initial_request();
        request.source_page_count = Some(4);
        request.imposition_mode = ImpositionMode::Repeat;
        request.sides = Sides::Single;
        request.impression_quantities = Some(vec![1, 2, 3, 4]);
        let mut drafts = RepeatQuantityDrafts::from_request(&request);

        set_all_repeat_quantities(&mut request, &mut drafts, 7);

        assert_eq!(request.impression_quantities, Some(vec![7, 7, 7, 7]));
        assert_eq!(request.quantity_requested, 28);
        assert_eq!(drafts.values(Sides::Single), vec![7, 7, 7, 7]);
        assert_eq!(drafts.values(Sides::Double), vec![1, 1]);
        set_all_repeat_quantities(&mut request, &mut drafts, 0);
        assert_eq!(request.impression_quantities, Some(vec![0; 4]));
        assert_eq!(request.quantity_requested, 0);
        assert_eq!(drafts.values(Sides::Single), vec![0; 4]);
        set_all_repeat_quantities(&mut request, &mut drafts, 7);
        assert_eq!(request.quantity_requested, 28);
        assert_eq!(drafts.values(Sides::Single), vec![7; 4]);
    }

    #[test]
    fn layout_mode_selection_builds_and_clears_manual_settings() {
        let mut request = initial_request();
        let mut seed = result();
        seed.rows = 2;
        seed.columns = 7;
        seed.rotation_degrees = 90;

        select_layout_mode(&mut request, LayoutMode::Manual, Some(&seed));
        assert_eq!(request.layout_mode, LayoutMode::Manual);
        assert_eq!(
            request.manual,
            Some(ManualLayout {
                rows: 2,
                columns: 7,
                rotation_degrees: 90,
                margins: None,
            })
        );

        select_layout_mode(&mut request, LayoutMode::MaxPieces, None);
        assert_eq!(request.layout_mode, LayoutMode::MaxPieces);
        assert!(request.manual.is_none());
    }

    #[test]
    fn fully_loaded_preview_starts_ready_instead_of_reverting_to_loading() {
        let view = preview_view("source", vec![1, 2], 0, PreviewSide::Front, 0);
        assert!(matches!(
            PreviewLifecycle::loading(view.clone(), 2),
            PreviewLifecycle::Ready { view: current } if current == view
        ));
        assert!(matches!(
            PreviewLifecycle::loading(view, 1),
            PreviewLifecycle::Loading { completed: 1, .. }
        ));
    }

    #[test]
    fn preview_becomes_ready_only_after_every_required_asset_is_decoded() {
        let view = preview_view("source", vec![1, 2], 0, PreviewSide::Front, 0);
        let mut lifecycle = PreviewLifecycle::loading(view.clone(), 0);

        lifecycle.record_completed(&view, 1);
        assert!(matches!(
            lifecycle,
            PreviewLifecycle::Loading { completed: 1, .. }
        ));
        lifecycle.record_completed(&view, 2);
        assert!(matches!(lifecycle, PreviewLifecycle::Ready { .. }));
    }

    #[test]
    fn output_uses_one_authoritative_warning_set() {
        let mut layout = result();
        layout.warnings = vec![ProductionWarning {
            problem: "Layout warning".into(),
            impact: "Current output".into(),
            fix: "Adjust layout".into(),
        }];
        let mut analysis: PdfAnalysis = serde_json::from_value(serde_json::json!({
            "filename":"cards.pdf","pageCount":2,"sourcePdfSize":{"width":3.75,"height":2.25},
            "orientation":"landscape","mediaBox":null,"cropBox":null,"bleedBox":null,
            "trimBox":null,"likelyBleed":{"detected":false,"amountPerSide":0.0,"horizontal":0.0,"vertical":0.0,"notes":[]},
            "matchedPresetId":null,"suggestedFinishedCutSize":null,"appearsDuplex":false,"warnings":[]
        }))
        .unwrap_or_else(|error| unreachable!("valid fixture: {error}"));
        analysis.warnings = vec![ProductionWarning {
            problem: "Source warning".into(),
            impact: "Original source".into(),
            fix: "Replace source".into(),
        }];

        assert_eq!(
            output_warnings(Some(&layout), Some(&analysis)),
            layout.warnings
        );
        assert_eq!(output_warnings(None, Some(&analysis)), analysis.warnings);
        assert!(output_warnings(None, None).is_empty());
    }

    #[test]
    fn preview_identity_changes_for_navigation_page_geometry_clipping_and_rotation() {
        let mut layout = result();
        layout.source_page_count = Some(8);
        layout.impressions_requested = 8;
        layout.sheets_required = 4;
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });
        let placement = layout.placements[0].clone();
        let front_page = preview_page_number(&layout, 0, PreviewSide::Front, 1);
        let front = preview_placement_key(
            "source",
            &layout,
            &placement,
            PreviewSide::Front,
            1,
            front_page,
        );
        let back_page = preview_page_number(&layout, 0, PreviewSide::Back, 1);
        let back = preview_placement_key(
            "source",
            &layout,
            &placement,
            PreviewSide::Back,
            1,
            back_page,
        );
        assert_eq!(front_page, Some(5));
        assert_eq!(back_page, Some(6));
        assert_ne!(front, back);

        layout.rotation_degrees = 90;
        layout.placements[0].finished_x += 0.25;
        layout.gutters.horizontal = 0.5;
        let changed = preview_placement_key(
            "source",
            &layout,
            &layout.placements[0],
            PreviewSide::Front,
            1,
            front_page,
        );
        assert_ne!(front, changed);
        assert_ne!(
            preview_clip_rect(&layout, &placement),
            preview_clip_rect(&layout, &layout.placements[0])
        );
        assert_eq!(
            preview_artwork_image(
                &layout,
                preview_artwork_rect(&layout, &layout.placements[0]),
                PreviewSide::Front,
            )
            .transform
            .as_deref(),
            Some("rotate(90 2.75 2)")
        );

        let back_before_mirror_change = preview_placement_key(
            "source",
            &layout,
            &layout.placements[0],
            PreviewSide::Back,
            1,
            back_page,
        );
        layout.parent_sheet_size = SizeInches {
            width: 18.0,
            height: 12.0,
        };
        let back_after_sheet_change = preview_placement_key(
            "source",
            &layout,
            &layout.placements[0],
            PreviewSide::Back,
            1,
            back_page,
        );
        assert_ne!(back_before_mirror_change, back_after_sheet_change);
        if let Some(duplex) = layout.duplex.as_mut() {
            duplex.flip_edge = DuplexFlipEdge::ShortEdge;
        }
        let back_after_flip_change = preview_placement_key(
            "source",
            &layout,
            &layout.placements[0],
            PreviewSide::Back,
            1,
            back_page,
        );
        assert_ne!(back_after_sheet_change, back_after_flip_change);
    }

    #[test]
    fn replacement_failure_only_rolls_back_the_matching_selection() {
        assert_eq!(
            selection_after_replacement_failure(&[3, 4], &[3, 4], &[1, 2]),
            vec![1, 2]
        );
        assert_eq!(
            selection_after_replacement_failure(&[5], &[3, 4], &[1, 2]),
            vec![5]
        );
        assert_eq!(
            selection_after_replacement_failure(&[3], &[3], &[]),
            vec![3]
        );
    }

    #[test]
    fn validation_rejects_impossible_variant_combinations_and_bad_results() {
        let mut request = initial_request();
        request.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });
        assert_eq!(
            request_issue(&request).as_deref(),
            Some("Single-sided imposition cannot include duplex settings.")
        );
        request.duplex = None;
        request.impression_quantities = Some(vec![1]);
        assert_eq!(
            request_issue(&request).as_deref(),
            Some("Unique imposition cannot include repeat quantities.")
        );
        let mut invalid = result();
        invalid.placements[0].width = f64::NAN;
        assert_eq!(
            validate_layout_result(&invalid),
            Err("The layout result contained an invalid placement.".into())
        );
    }
}
