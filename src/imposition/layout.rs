use std::cmp::Ordering;

use super::model::{
    BleedOption, BleedSettings, BleedSource, DuplexFlipEdge, DuplexSettings, GuttersInches,
    ImpositionMode, LayoutMode, LayoutRequest, LayoutResult, ManualLayout, MarginsInches,
    OrientationPreference, PdfBox, PiecePlacement, ProductionWarning, Sides, SizeInches,
};
use crate::error::{AppError, AppResult};

const SIZE_TOLERANCE: f64 = 0.01;
const MIN_DIMENSION_INCHES: f64 = 0.01;
const MAX_DIMENSION_INCHES: f64 = 100.0;
const MAX_IMPRESSIONS: usize = 10_000;
const MAX_GRID_AXIS: usize = 100;
const MAX_LAYOUT_PLACEMENTS: usize = 512;
pub(crate) const MAX_OUTPUT_PAGE_SIDES: usize = 1_000;
#[cfg(test)]
pub(crate) const DEFAULT_CREATED_BLEED_IN: f64 = 0.125;

pub(crate) fn generate_layout(request: LayoutRequest) -> AppResult<LayoutResult> {
    if super::mixed::enabled(&request) {
        return super::mixed::generate(request);
    }
    let request = ValidatedLayoutRequest::try_from(request)?;

    match request.0.layout_mode {
        LayoutMode::Manual => manual_layout(request),
        LayoutMode::Auto | LayoutMode::MaxPieces => auto_layout(request),
    }
}

struct ValidatedLayoutRequest(LayoutRequest);

impl TryFrom<LayoutRequest> for ValidatedLayoutRequest {
    type Error = AppError;

    fn try_from(request: LayoutRequest) -> Result<Self, Self::Error> {
        validate_request(&request)?;
        Ok(Self(request))
    }
}

pub(crate) fn validate_request(request: &LayoutRequest) -> AppResult<()> {
    if request.source_pages.len() > crate::MAX_SOURCE_PDF_PAGES
        || (!request.source_pages.is_empty()
            && request
                .source_page_count
                .is_some_and(|n| n != request.source_pages.len()))
    {
        return Err(AppError::bad_request(
            "sourcePages must match a bounded source page count",
        ));
    }
    for page in &request.source_pages {
        validate_size(page.source_pdf_size, "source page size")?;
        validate_source_trim_box(page.source_trim_box, page.source_pdf_size)?;
    }
    if super::mixed::enabled(request) {
        return super::mixed::validate_request(request);
    }
    validate_size(request.source_pdf_size, "source PDF size")?;
    validate_source_trim_box(request.source_trim_box, request.source_pdf_size)?;
    validate_size(request.finished_cut_size, "finished cut size")?;
    validate_size(request.parent_sheet_size, "parent sheet size")?;
    validate_source_bleed_override(request)?;
    match (request.sides, request.duplex.as_ref()) {
        (Sides::Single, None) | (Sides::Double, Some(_)) => {}
        (Sides::Single, Some(_)) => {
            return Err(AppError::bad_request(
                "single-sided layouts cannot include duplex settings",
            ));
        }
        (Sides::Double, None) => {
            return Err(AppError::bad_request(
                "double-sided layouts require duplex settings",
            ));
        }
    }
    match (request.layout_mode, request.manual) {
        (LayoutMode::Manual, Some(manual)) => validate_manual_layout(manual)?,
        (LayoutMode::Manual, None) => {
            return Err(AppError::bad_request(
                "manual layouts require custom grid settings",
            ));
        }
        (LayoutMode::Auto | LayoutMode::MaxPieces, None) => {}
        (LayoutMode::Auto | LayoutMode::MaxPieces, Some(_)) => {
            return Err(AppError::bad_request(
                "custom grid settings require manual layout mode",
            ));
        }
    }

    impressions_requested(request)?;

    if !request.gutter.horizontal.is_finite() || !request.gutter.vertical.is_finite() {
        return Err(AppError::bad_request("gutters must contain finite values"));
    }
    if !(0.0..=100.0).contains(&request.gutter.horizontal)
        || !(0.0..=100.0).contains(&request.gutter.vertical)
    {
        return Err(AppError::bad_request(
            "gutters must be from 0 to 100 inches",
        ));
    }
    if matches!(request.bleed_option, BleedOption::ScaleToBleed)
        && (!request.created_bleed_amount.is_finite()
            || !(0.001..=1.0).contains(&request.created_bleed_amount))
    {
        return Err(AppError::bad_request(
            "created bleed amount must be from 0.001 to 1 inch",
        ));
    }

    Ok(())
}

fn validate_source_bleed_override(request: &LayoutRequest) -> AppResult<()> {
    let Some(bleed) = request.source_bleed_override else {
        return Ok(());
    };
    if !bleed.is_finite() || !(0.001..=1.0).contains(&bleed) {
        return Err(AppError::bad_request(
            "source bleed override must be from 0.001 to 1 inch per side",
        ));
    }
    let derived = SizeInches {
        width: request.source_pdf_size.width - bleed * 2.0,
        height: request.source_pdf_size.height - bleed * 2.0,
    };
    if derived.width < MIN_DIMENSION_INCHES || derived.height < MIN_DIMENSION_INCHES {
        return Err(AppError::bad_request(
            "source bleed override leaves no valid finished size",
        ));
    }
    if !same_size(derived, request.finished_cut_size) {
        return Err(AppError::bad_request(
            "finished cut size must equal the source PDF size minus twice the source bleed override",
        ));
    }
    Ok(())
}

pub(super) fn validate_source_trim_box(trim: Option<PdfBox>, source: SizeInches) -> AppResult<()> {
    let Some(trim) = trim else {
        return Ok(());
    };
    let values = [
        trim.left,
        trim.bottom,
        trim.right,
        trim.top,
        trim.width,
        trim.height,
    ];
    if values.iter().any(|value| !value.is_finite()) {
        return Err(AppError::bad_request(
            "source trim box must contain finite values",
        ));
    }
    if trim.width <= 0.0
        || trim.height <= 0.0
        || (trim.right - trim.left - trim.width).abs() > SIZE_TOLERANCE
        || (trim.top - trim.bottom - trim.height).abs() > SIZE_TOLERANCE
        || trim.left < -SIZE_TOLERANCE
        || trim.bottom < -SIZE_TOLERANCE
        || trim.right > source.width + SIZE_TOLERANCE
        || trim.top > source.height + SIZE_TOLERANCE
    {
        return Err(AppError::bad_request(
            "source trim box must be a positive rectangle within the source PDF",
        ));
    }
    Ok(())
}

pub(super) fn validate_size(size: SizeInches, label: &str) -> AppResult<()> {
    if !size.width.is_finite() || !size.height.is_finite() {
        return Err(AppError::bad_request(format!(
            "{label} must contain finite dimensions"
        )));
    }
    if size.width < MIN_DIMENSION_INCHES || size.height < MIN_DIMENSION_INCHES {
        return Err(AppError::bad_request(format!(
            "{label} must be at least {MIN_DIMENSION_INCHES} inches in each direction"
        )));
    }
    if size.width > MAX_DIMENSION_INCHES || size.height > MAX_DIMENSION_INCHES {
        return Err(AppError::bad_request(format!(
            "{label} cannot exceed {MAX_DIMENSION_INCHES} inches in either direction"
        )));
    }
    Ok(())
}

pub(crate) fn impressions_requested(request: &LayoutRequest) -> AppResult<usize> {
    let impressions = match request.imposition_mode {
        ImpositionMode::Repeat => match &request.impression_quantities {
            Some(quantities) => {
                let expected = source_impression_count(request)?;
                if quantities.len() != expected {
                    return Err(AppError::bad_request(format!(
                        "repeat quantities must contain one value for each of the {expected} source impressions"
                    )));
                }
                quantities.iter().try_fold(0usize, |total, quantity| {
                    total
                        .checked_add(*quantity)
                        .ok_or_else(|| AppError::bad_request("repeat quantity total is too large"))
                })?
            }
            None => request.quantity_requested,
        },
        ImpositionMode::Unique => source_impression_count(request)?,
    };

    if impressions == 0 {
        return Err(AppError::bad_request(
            "imposition quantity must be at least 1",
        ));
    }

    if impressions > MAX_IMPRESSIONS {
        return Err(AppError::bad_request(format!(
            "imposition is limited to {MAX_IMPRESSIONS} requested pieces"
        )));
    }

    Ok(impressions)
}

fn source_impression_count(request: &LayoutRequest) -> AppResult<usize> {
    let page_count = request
        .source_page_count
        .ok_or_else(|| AppError::bad_request("imposition requires source page count"))?;
    if page_count == 0 {
        return Err(AppError::bad_request(
            "imposition requires at least one source page",
        ));
    }
    if matches!(request.sides, Sides::Double) {
        if page_count < 2 || page_count % 2 != 0 {
            return Err(AppError::bad_request(
                "double-sided imposition requires an even source page count",
            ));
        }
        Ok(page_count / 2)
    } else {
        Ok(page_count)
    }
}

fn auto_layout(request: ValidatedLayoutRequest) -> AppResult<LayoutResult> {
    let raw_request = &request.0;
    let mut best: Option<LayoutResult> = None;
    for rotation in allowed_rotations(
        raw_request.finished_cut_size,
        raw_request.orientation_preference,
    ) {
        let cut = rotate_size(raw_request.finished_cut_size, rotation);
        let max_rows = fit_count(
            raw_request.parent_sheet_size.height,
            cut.height,
            raw_request.gutter.vertical,
        );
        let max_columns = fit_count(
            raw_request.parent_sheet_size.width,
            cut.width,
            raw_request.gutter.horizontal,
        );
        for (rows, columns) in bounded_auto_grids(max_rows, max_columns)? {
            if let Ok(candidate) = candidate_for(&request, rotation, Some((rows, columns))) {
                let replace = best.as_ref().is_none_or(|current| {
                    compare_candidates(&candidate, current) == Ordering::Greater
                });
                if replace {
                    best = Some(candidate);
                }
            }
        }
    }

    let result = best.ok_or_else(|| {
        AppError::bad_request(
            "finished cut size and selected bleed do not fit the parent sheet without clipping",
        )
    })?;
    validate_output_page_sides(&result)?;
    Ok(result)
}

fn bounded_auto_grids(rows: usize, columns: usize) -> AppResult<Vec<(usize, usize)>> {
    if rows == 0 || columns == 0 {
        return Ok(vec![(rows, columns)]);
    }
    let _grid_capacity = rows
        .checked_mul(columns)
        .ok_or_else(|| AppError::bad_request("layout grid is too large to calculate safely"))?;
    let mut grids = Vec::new();
    for candidate_rows in 1..=rows.min(MAX_LAYOUT_PLACEMENTS) {
        let max_columns = columns.min(MAX_LAYOUT_PLACEMENTS / candidate_rows);
        for candidate_columns in 1..=max_columns {
            grids.push((candidate_rows, candidate_columns));
        }
    }
    Ok(grids)
}

fn manual_layout(request: ValidatedLayoutRequest) -> AppResult<LayoutResult> {
    let manual = request
        .0
        .manual
        .ok_or_else(|| AppError::bad_request("custom n-up grid settings are required"))?;
    validate_manual_layout(manual)?;

    let rotation = requested_rotation(
        request.0.finished_cut_size,
        request.0.orientation_preference,
        manual.rotation_degrees,
    );
    let result =
        candidate_for(&request, rotation, Some((manual.rows, manual.columns))).map_err(|_| {
            AppError::bad_request(
                "custom n-up grid does not fit the selected parent sheet with the current gutters",
            )
        })?;
    validate_output_page_sides(&result)?;
    Ok(result)
}

pub(crate) fn validate_manual_layout(manual: ManualLayout) -> AppResult<()> {
    if manual.rows == 0 || manual.columns == 0 {
        return Err(AppError::bad_request(
            "custom n-up grid rows and columns must be greater than zero",
        ));
    }
    if manual.rows > MAX_GRID_AXIS || manual.columns > MAX_GRID_AXIS {
        return Err(AppError::bad_request(format!(
            "custom n-up grid rows and columns cannot exceed {MAX_GRID_AXIS}"
        )));
    }
    if manual
        .rows
        .checked_mul(manual.columns)
        .is_none_or(|placements| placements > MAX_LAYOUT_PLACEMENTS)
    {
        return Err(AppError::bad_request(format!(
            "custom n-up grid cannot exceed {MAX_LAYOUT_PLACEMENTS} placements"
        )));
    }
    if manual.rotation_degrees != 0 && manual.rotation_degrees != 90 {
        return Err(AppError::bad_request(
            "custom n-up rotation must be 0 or 90",
        ));
    }
    if let Some(margins) = manual.margins {
        let values = [margins.top, margins.right, margins.bottom, margins.left];
        if values.iter().any(|value| !value.is_finite()) {
            return Err(AppError::bad_request(
                "custom n-up margins must contain finite values",
            ));
        }
        if values.iter().any(|value| *value < 0.0) {
            return Err(AppError::bad_request(
                "custom n-up margins cannot be negative",
            ));
        }
    }
    Ok(())
}

fn validate_output_page_sides(layout: &LayoutResult) -> AppResult<()> {
    let output_page_sides = layout
        .sheets_required
        .checked_mul(if layout.duplex.is_some() { 2 } else { 1 })
        .ok_or_else(|| AppError::bad_request("layout output page count is too large"))?;
    if output_page_sides > MAX_OUTPUT_PAGE_SIDES {
        return Err(AppError::bad_request(format!(
            "layout would create {output_page_sides} sheet sides; the limit is {MAX_OUTPUT_PAGE_SIDES}"
        )));
    }
    Ok(())
}

fn compare_candidates(left: &LayoutResult, right: &LayoutResult) -> Ordering {
    left.sheets_required
        .cmp(&right.sheets_required)
        .reverse()
        .then_with(|| left.pieces_per_sheet.cmp(&right.pieces_per_sheet))
        .then_with(|| {
            left.waste_percent
                .partial_cmp(&right.waste_percent)
                .unwrap_or(Ordering::Equal)
                .reverse()
        })
        .then_with(|| cut_position_count(right).cmp(&cut_position_count(left)))
        .then_with(|| {
            balance_score(left)
                .partial_cmp(&balance_score(right))
                .unwrap_or(Ordering::Equal)
                .reverse()
        })
        .then_with(|| right.rotation_degrees.cmp(&left.rotation_degrees))
}

fn cut_position_count(result: &LayoutResult) -> usize {
    let mut vertical = result
        .placements
        .iter()
        .flat_map(|placement| {
            [
                placement.finished_x,
                placement.finished_x + placement.finished_width,
            ]
        })
        .filter(|position| {
            *position > SIZE_TOLERANCE
                && *position < result.parent_sheet_size.width - SIZE_TOLERANCE
        })
        .map(|position| (position * 10_000.0).round() as i64)
        .collect::<Vec<_>>();
    let mut horizontal = result
        .placements
        .iter()
        .flat_map(|placement| {
            [
                placement.finished_y,
                placement.finished_y + placement.finished_height,
            ]
        })
        .filter(|position| {
            *position > SIZE_TOLERANCE
                && *position < result.parent_sheet_size.height - SIZE_TOLERANCE
        })
        .map(|position| (position * 10_000.0).round() as i64)
        .collect::<Vec<_>>();
    vertical.sort_unstable();
    vertical.dedup();
    horizontal.sort_unstable();
    horizontal.dedup();
    vertical.len() + horizontal.len()
}

fn balance_score(result: &LayoutResult) -> f64 {
    (result.margins.left - result.margins.right).abs()
        + (result.margins.top - result.margins.bottom).abs()
}

fn allowed_rotations(size: SizeInches, preference: OrientationPreference) -> Vec<u16> {
    match preference {
        OrientationPreference::Auto => vec![0, 90],
        _ => vec![requested_rotation(size, preference, 0)],
    }
}

fn requested_rotation(size: SizeInches, preference: OrientationPreference, fallback: u16) -> u16 {
    if preference == OrientationPreference::Upright {
        return 0;
    }
    if preference == OrientationPreference::QuarterTurn {
        return 90;
    }
    if matches!(preference, OrientationPreference::Auto)
        || (size.width - size.height).abs() <= SIZE_TOLERANCE
    {
        return fallback;
    }

    let starts_landscape = size.width > size.height;
    let wants_landscape = matches!(preference, OrientationPreference::Landscape);
    if starts_landscape == wants_landscape {
        0
    } else {
        90
    }
}

fn candidate_for(
    request: &ValidatedLayoutRequest,
    rotation_degrees: u16,
    manual_grid: Option<(usize, usize)>,
) -> AppResult<LayoutResult> {
    let request = &request.0;
    let cut = rotate_size(request.finished_cut_size, rotation_degrees);
    let source = rotate_size(request.source_pdf_size, rotation_degrees);
    let (rows, columns) = manual_grid.unwrap_or_else(|| {
        (
            fit_count(
                request.parent_sheet_size.height,
                cut.height,
                request.gutter.vertical,
            ),
            fit_count(
                request.parent_sheet_size.width,
                cut.width,
                request.gutter.horizontal,
            ),
        )
    });

    if rows == 0 || columns == 0 {
        return Err(AppError::bad_request("layout has no pieces"));
    }

    let grid_width = grid_extent(columns, cut.width, request.gutter.horizontal);
    let grid_height = grid_extent(rows, cut.height, request.gutter.vertical);
    if grid_width > request.parent_sheet_size.width + SIZE_TOLERANCE
        || grid_height > request.parent_sheet_size.height + SIZE_TOLERANCE
    {
        return Err(AppError::bad_request("layout does not fit parent sheet"));
    }

    let margins = if matches!(request.layout_mode, LayoutMode::Manual) {
        request
            .manual
            .and_then(|manual| manual.margins)
            .unwrap_or_else(|| centered_margins(request.parent_sheet_size, grid_width, grid_height))
    } else {
        bleed_safe_centered_margins(
            request.parent_sheet_size,
            grid_width,
            grid_height,
            artwork_clearances_for_request(request, rotation_degrees),
        )
    };

    if margins.left < 0.0
        || margins.right < 0.0
        || margins.top < 0.0
        || margins.bottom < 0.0
        || margins.left + grid_width + margins.right
            > request.parent_sheet_size.width + SIZE_TOLERANCE
        || margins.top + grid_height + margins.bottom
            > request.parent_sheet_size.height + SIZE_TOLERANCE
        || (margins.left + grid_width + margins.right - request.parent_sheet_size.width).abs()
            > SIZE_TOLERANCE
        || (margins.top + grid_height + margins.bottom - request.parent_sheet_size.height).abs()
            > SIZE_TOLERANCE
    {
        return Err(AppError::bad_request(
            "layout margins do not fit parent sheet",
        ));
    }

    let pieces_per_sheet = rows
        .checked_mul(columns)
        .filter(|pieces| *pieces <= MAX_LAYOUT_PLACEMENTS)
        .ok_or_else(|| {
            AppError::bad_request(format!(
                "layout grid exceeds the {MAX_LAYOUT_PLACEMENTS}-placement safety limit"
            ))
        })?;
    let impressions_requested = impressions_requested(request)?;
    let sheets_required = impressions_requested.div_ceil(pieces_per_sheet);
    let available_positions = sheets_required
        .checked_mul(pieces_per_sheet)
        .ok_or_else(|| AppError::bad_request("layout piece count is too large"))?;
    let exact_quantities = matches!(request.imposition_mode, ImpositionMode::Unique)
        || request.impression_quantities.is_some();
    let total_pieces_produced = if exact_quantities {
        impressions_requested
    } else {
        available_positions
    };
    let extra_pieces_produced = if exact_quantities {
        0
    } else {
        available_positions.saturating_sub(impressions_requested)
    };
    let unused_positions = if exact_quantities {
        available_positions.saturating_sub(impressions_requested)
    } else {
        0
    };
    let waste_percent = waste_percent(
        request.parent_sheet_size,
        request.finished_cut_size,
        pieces_per_sheet,
    );
    let bleed = bleed_settings(
        request.source_pdf_size,
        request.finished_cut_size,
        request.bleed_option,
        request.source_bleed_override,
    );
    let source_trim_box = effective_source_trim_box(request);
    let placements = placements(
        rows,
        columns,
        margins,
        request.gutter,
        cut,
        source,
        source_trim_box,
        rotation_degrees,
    );
    let mut result = LayoutResult {
        source_bleed_override: request.source_bleed_override,
        page_plans: Vec::new(),
        source_pdf_size: request.source_pdf_size,
        source_trim_box,
        source_page_count: request.source_page_count,
        finished_cut_size: request.finished_cut_size,
        parent_sheet_size: request.parent_sheet_size,
        quantity_requested: request.quantity_requested,
        imposition_mode: request.imposition_mode,
        impression_quantities: request.impression_quantities.clone(),
        orientation_preference: request.orientation_preference,
        impressions_requested,
        pieces_per_sheet,
        sheets_required,
        total_pieces_produced,
        extra_pieces_produced,
        unused_positions,
        waste_percent,
        rotation_degrees,
        rows,
        columns,
        margins,
        gutters: request.gutter,
        placements,
        duplex: duplex_settings(request.sides, request.duplex.clone()),
        bleed,
        created_bleed_amount: request.created_bleed_amount,
        warnings: Vec::new(),
    };
    result.warnings = layout_warnings(request, &result)?;
    Ok(result)
}

fn fit_count(parent: f64, piece: f64, gutter: f64) -> usize {
    if piece <= 0.0 || parent + SIZE_TOLERANCE < piece {
        return 0;
    }

    let mut count = 0_usize;
    loop {
        let Some(next) = count.checked_add(1) else {
            return count;
        };
        if grid_extent(next, piece, gutter) <= parent + SIZE_TOLERANCE {
            count = next;
        } else {
            return count;
        }
    }
}

fn grid_extent(count: usize, piece: f64, gutter: f64) -> f64 {
    if count == 0 {
        0.0
    } else {
        count as f64 * piece + count.saturating_sub(1) as f64 * gutter
    }
}

fn rotate_size(size: SizeInches, rotation_degrees: u16) -> SizeInches {
    if rotation_degrees == 90 {
        SizeInches {
            width: size.height,
            height: size.width,
        }
    } else {
        size
    }
}

fn centered_margins(parent: SizeInches, grid_width: f64, grid_height: f64) -> MarginsInches {
    MarginsInches {
        left: round4((parent.width - grid_width) / 2.0),
        right: round4((parent.width - grid_width) / 2.0),
        top: round4((parent.height - grid_height) / 2.0),
        bottom: round4((parent.height - grid_height) / 2.0),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct ArtworkClearances {
    top: f64,
    right: f64,
    bottom: f64,
    left: f64,
}

fn artwork_clearances_for_request(
    request: &LayoutRequest,
    rotation_degrees: u16,
) -> ArtworkClearances {
    let source = rotate_size(request.source_pdf_size, rotation_degrees);
    let cut = rotate_size(request.finished_cut_size, rotation_degrees);
    let finished = ArtworkRect {
        x: 0.0,
        y: 0.0,
        width: cut.width,
        height: cut.height,
    };
    let artwork = match request.bleed_option {
        BleedOption::FitInside => contain_rect(source, finished),
        BleedOption::UseAsIs => {
            let trim = effective_source_trim_box(request)
                .map(|trim| rotate_box(trim, request.source_pdf_size, rotation_degrees))
                .unwrap_or_else(|| centered_box(source, cut));
            let trim_center_x = trim.left + trim.width / 2.0;
            let trim_center_from_top = source.height - trim.bottom - trim.height / 2.0;
            ArtworkRect {
                x: cut.width / 2.0 - trim_center_x,
                y: cut.height / 2.0 - trim_center_from_top,
                width: source.width,
                height: source.height,
            }
        }
        BleedOption::ScaleToBleed => {
            let detected = bleed_settings(
                request.source_pdf_size,
                request.finished_cut_size,
                request.bleed_option,
                request.source_bleed_override,
            )
            .effective_amount_per_side;
            let bleed = if detected > 0.0 {
                detected
            } else {
                request.created_bleed_amount
            };
            let trim = effective_source_trim_box(request)
                .map(|trim| rotate_box(trim, request.source_pdf_size, rotation_degrees));
            let basis = trim.unwrap_or(PdfBox {
                left: 0.0,
                bottom: 0.0,
                right: source.width,
                top: source.height,
                width: source.width,
                height: source.height,
            });
            let (target_width, target_height) = if trim.is_some() && detected > 0.0 {
                (cut.width, cut.height)
            } else {
                (cut.width + bleed * 2.0, cut.height + bleed * 2.0)
            };
            let scale = (target_width / basis.width).max(target_height / basis.height);
            let trim_center_x = basis.left + basis.width / 2.0;
            let trim_center_from_top = source.height - basis.bottom - basis.height / 2.0;
            ArtworkRect {
                x: cut.width / 2.0 - trim_center_x * scale,
                y: cut.height / 2.0 - trim_center_from_top * scale,
                width: source.width * scale,
                height: source.height * scale,
            }
        }
    };
    ArtworkClearances {
        left: (-artwork.x).max(0.0),
        right: (artwork.x + artwork.width - cut.width).max(0.0),
        top: (-artwork.y).max(0.0),
        bottom: (artwork.y + artwork.height - cut.height).max(0.0),
    }
}

fn bleed_safe_centered_margins(
    parent: SizeInches,
    grid_width: f64,
    grid_height: f64,
    clearance: ArtworkClearances,
) -> MarginsInches {
    let free_width = (parent.width - grid_width).max(0.0);
    let free_height = (parent.height - grid_height).max(0.0);
    let left = if clearance.left <= free_width - clearance.right {
        (free_width / 2.0).clamp(clearance.left, free_width - clearance.right)
    } else {
        free_width / 2.0
    };
    let top = if clearance.top <= free_height - clearance.bottom {
        (free_height / 2.0).clamp(clearance.top, free_height - clearance.bottom)
    } else {
        free_height / 2.0
    };
    MarginsInches {
        left: round4(left),
        right: round4(free_width - left),
        top: round4(top),
        bottom: round4(free_height - top),
    }
}

#[allow(clippy::too_many_arguments)]
fn placements(
    rows: usize,
    columns: usize,
    margins: MarginsInches,
    gutters: GuttersInches,
    cut: SizeInches,
    source: SizeInches,
    source_trim_box: Option<PdfBox>,
    rotation_degrees: u16,
) -> Vec<PiecePlacement> {
    let trim = source_trim_box
        .map(|trim| {
            rotate_box(
                trim,
                request_source_size(source, rotation_degrees),
                rotation_degrees,
            )
        })
        .unwrap_or_else(|| centered_box(source, cut));
    let trim_center_x = trim.left + trim.width / 2.0;
    let trim_center_from_top = source.height - trim.bottom - trim.height / 2.0;
    let mut result = Vec::with_capacity(rows * columns);

    for row in 0..rows {
        for column in 0..columns {
            let finished_x = margins.left + column as f64 * (cut.width + gutters.horizontal);
            let finished_y = margins.top + row as f64 * (cut.height + gutters.vertical);
            result.push(PiecePlacement {
                index: result.len(),
                row,
                column,
                x: round4(finished_x + cut.width / 2.0 - trim_center_x),
                y: round4(finished_y + cut.height / 2.0 - trim_center_from_top),
                width: round4(source.width),
                height: round4(source.height),
                finished_x: round4(finished_x),
                finished_y: round4(finished_y),
                finished_width: round4(cut.width),
                finished_height: round4(cut.height),
            });
        }
    }

    result
}

fn request_source_size(rotated_source: SizeInches, rotation_degrees: u16) -> SizeInches {
    rotate_size(rotated_source, rotation_degrees)
}

fn centered_box(source: SizeInches, cut: SizeInches) -> PdfBox {
    let left = (source.width - cut.width) / 2.0;
    let bottom = (source.height - cut.height) / 2.0;
    PdfBox {
        left,
        bottom,
        right: left + cut.width,
        top: bottom + cut.height,
        width: cut.width,
        height: cut.height,
    }
}

fn rotate_box(rect: PdfBox, source: SizeInches, rotation_degrees: u16) -> PdfBox {
    if rotation_degrees != 90 {
        return rect;
    }
    PdfBox {
        left: rect.bottom,
        bottom: source.width - rect.right,
        right: rect.top,
        top: source.width - rect.left,
        width: rect.height,
        height: rect.width,
    }
}

pub(crate) fn bleed_settings(
    source: SizeInches,
    finished: SizeInches,
    option: BleedOption,
    source_bleed_override: Option<f64>,
) -> BleedSettings {
    let horizontal = ((source.width - finished.width) / 2.0).max(0.0);
    let vertical = ((source.height - finished.height) / 2.0).max(0.0);
    let detected = if horizontal > SIZE_TOLERANCE
        && vertical > SIZE_TOLERANCE
        && (horizontal - vertical).abs() <= SIZE_TOLERANCE
    {
        horizontal.min(vertical)
    } else {
        0.0
    };

    let effective = source_bleed_override.unwrap_or(detected);
    BleedSettings {
        option,
        detected_amount_per_side: round4(detected),
        effective_amount_per_side: round4(effective),
        source: if source_bleed_override.is_some() {
            BleedSource::Manual
        } else if detected > 0.0 {
            BleedSource::Detected
        } else {
            BleedSource::None
        },
        source_larger_than_cut: source.width > finished.width + SIZE_TOLERANCE
            || source.height > finished.height + SIZE_TOLERANCE,
    }
}

fn effective_source_trim_box(request: &LayoutRequest) -> Option<PdfBox> {
    request
        .source_bleed_override
        .map_or(request.source_trim_box, |bleed| {
            Some(PdfBox {
                left: bleed,
                bottom: bleed,
                right: request.source_pdf_size.width - bleed,
                top: request.source_pdf_size.height - bleed,
                width: request.source_pdf_size.width - bleed * 2.0,
                height: request.source_pdf_size.height - bleed * 2.0,
            })
        })
}

fn duplex_settings(sides: Sides, settings: Option<DuplexSettings>) -> Option<DuplexSettings> {
    match sides {
        Sides::Single => None,
        Sides::Double => {
            let mut settings = settings.unwrap_or_else(default_duplex_settings);
            settings.back_alignment = duplex_alignment_note(&settings);
            Some(settings)
        }
    }
}

fn default_duplex_settings() -> DuplexSettings {
    DuplexSettings {
        flip_edge: DuplexFlipEdge::LongEdge,
        rotate_back_180: false,
        back_alignment: String::new(),
    }
}

fn duplex_alignment_note(settings: &DuplexSettings) -> String {
    let flip = match settings.flip_edge {
        DuplexFlipEdge::LongEdge => "long-edge flip",
        DuplexFlipEdge::ShortEdge => "short-edge flip",
    };
    let rotation = if settings.rotate_back_180 {
        " with back rotated 180 degrees"
    } else {
        ""
    };
    format!("Back preview and export use the same grid with {flip}{rotation}.")
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ArtworkRect {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

pub(crate) fn resolved_artwork_rect(
    layout: &LayoutResult,
    placement: &PiecePlacement,
) -> ArtworkRect {
    let source = rotate_size(layout.source_pdf_size, layout.rotation_degrees);
    let finished = ArtworkRect {
        x: placement.finished_x,
        y: placement.finished_y,
        width: placement.finished_width,
        height: placement.finished_height,
    };

    match layout.bleed.option {
        BleedOption::UseAsIs => ArtworkRect {
            x: placement.x,
            y: placement.y,
            width: placement.width,
            height: placement.height,
        },
        BleedOption::FitInside => contain_rect(source, finished),
        BleedOption::ScaleToBleed => {
            let bleed = if layout.bleed.effective_amount_per_side > 0.0 {
                layout.bleed.effective_amount_per_side
            } else {
                layout.created_bleed_amount
            };
            let trim = layout
                .source_trim_box
                .map(|trim| rotate_box(trim, layout.source_pdf_size, layout.rotation_degrees));
            let basis = trim.unwrap_or(PdfBox {
                left: 0.0,
                bottom: 0.0,
                right: source.width,
                top: source.height,
                width: source.width,
                height: source.height,
            });
            let (target_width, target_height) =
                if trim.is_some() && layout.bleed.effective_amount_per_side > 0.0 {
                    (finished.width, finished.height)
                } else {
                    (finished.width + bleed * 2.0, finished.height + bleed * 2.0)
                };
            let scale_x = target_width / basis.width;
            let scale_y = target_height / basis.height;
            let trim_center_x = basis.left + basis.width / 2.0;
            let trim_center_from_top = source.height - basis.bottom - basis.height / 2.0;
            ArtworkRect {
                x: finished.x + finished.width / 2.0 - trim_center_x * scale_x,
                y: finished.y + finished.height / 2.0 - trim_center_from_top * scale_y,
                width: source.width * scale_x,
                height: source.height * scale_y,
            }
        }
    }
}

fn contain_rect(source: SizeInches, target: ArtworkRect) -> ArtworkRect {
    let scale = (target.width / source.width).min(target.height / source.height);
    ArtworkRect {
        x: target.x + (target.width - source.width * scale) / 2.0,
        y: target.y + (target.height - source.height * scale) / 2.0,
        width: source.width * scale,
        height: source.height * scale,
    }
}

#[cfg(test)]
fn artwork_is_preserved(layout: &LayoutResult) -> bool {
    let first = &layout.placements[0];
    let artwork = resolved_artwork_rect(layout, first);
    let left = (first.finished_x - artwork.x).max(0.0);
    let right = (artwork.x + artwork.width - first.finished_x - first.finished_width).max(0.0);
    let top = (first.finished_y - artwork.y).max(0.0);
    let bottom = (artwork.y + artwork.height - first.finished_y - first.finished_height).max(0.0);

    (layout.columns == 1 || layout.gutters.horizontal + SIZE_TOLERANCE >= left + right)
        && (layout.rows == 1 || layout.gutters.vertical + SIZE_TOLERANCE >= top + bottom)
        && layout.margins.left + SIZE_TOLERANCE >= left
        && layout.margins.right + SIZE_TOLERANCE >= right
        && layout.margins.top + SIZE_TOLERANCE >= top
        && layout.margins.bottom + SIZE_TOLERANCE >= bottom
}

fn layout_warnings(
    request: &LayoutRequest,
    layout: &LayoutResult,
) -> AppResult<Vec<ProductionWarning>> {
    let mut warnings = Vec::new();
    let bleed = bleed_settings(
        request.source_pdf_size,
        request.finished_cut_size,
        request.bleed_option,
        request.source_bleed_override,
    );

    if bleed.effective_amount_per_side <= SIZE_TOLERANCE {
        warnings.push(ProductionWarning::new(
            "No bleed detected.",
            "Cuts may show white edges.",
            "Continue only without edge-to-edge color.",
        ));
    }

    let source_has_uniform_bleed = bleed.effective_amount_per_side > SIZE_TOLERANCE
        && (((request.source_pdf_size.width - request.finished_cut_size.width) / 2.0)
            - ((request.source_pdf_size.height - request.finished_cut_size.height) / 2.0))
            .abs()
            <= SIZE_TOLERANCE;
    let trim_matches_finished = effective_source_trim_box(request).is_some_and(|trim| {
        same_size(
            SizeInches {
                width: trim.width,
                height: trim.height,
            },
            request.finished_cut_size,
        )
    });
    if !same_size(request.source_pdf_size, request.finished_cut_size)
        && !source_has_uniform_bleed
        && !trim_matches_finished
    {
        warnings.push(ProductionWarning::new(
            "Source PDF size does not match finished cut size.",
            "Wrong sizing may trim incorrectly.",
            "Use the PDF size, scale, or enter a custom cut size.",
        ));
    }

    let first = layout
        .placements
        .first()
        .ok_or_else(|| AppError::Internal("generated layout contains no placements".to_string()))?;
    let artwork = resolved_artwork_rect(layout, first);
    let left_clearance = (first.finished_x - artwork.x).max(0.0);
    let right_clearance =
        (artwork.x + artwork.width - first.finished_x - first.finished_width).max(0.0);
    let top_clearance = (first.finished_y - artwork.y).max(0.0);
    let bottom_clearance =
        (artwork.y + artwork.height - first.finished_y - first.finished_height).max(0.0);
    let required_horizontal_gutter = left_clearance + right_clearance;
    let required_vertical_gutter = top_clearance + bottom_clearance;
    if (layout.columns > 1
        && request.gutter.horizontal + SIZE_TOLERANCE < required_horizontal_gutter)
        || (layout.rows > 1 && request.gutter.vertical + SIZE_TOLERANCE < required_vertical_gutter)
    {
        warnings.push(ProductionWarning::new(
            "Bleed is wider than the available gutter.",
            "Some bleed will be clipped so neighboring artwork cannot overlap.",
            format!(
                "Use at least {:.4} in horizontal and {:.4} in vertical gutter to preserve all bleed.",
                required_horizontal_gutter, required_vertical_gutter
            ),
        ));
    }

    if layout.margins.left + SIZE_TOLERANCE < left_clearance
        || layout.margins.right + SIZE_TOLERANCE < right_clearance
        || layout.margins.top + SIZE_TOLERANCE < top_clearance
        || layout.margins.bottom + SIZE_TOLERANCE < bottom_clearance
    {
        warnings.push(ProductionWarning::new(
            "Bleed reaches beyond the parent sheet.",
            "Artwork at an outside edge will be clipped by the sheet.",
            format!(
                "Use at least {:.4} in top, {:.4} in right, {:.4} in bottom, and {:.4} in left margin.",
                top_clearance, right_clearance, bottom_clearance, left_clearance
            ),
        ));
    }

    Ok(warnings)
}

fn same_size(left: SizeInches, right: SizeInches) -> bool {
    (left.width - right.width).abs() <= SIZE_TOLERANCE
        && (left.height - right.height).abs() <= SIZE_TOLERANCE
}

fn waste_percent(parent: SizeInches, finished: SizeInches, pieces_per_sheet: usize) -> f64 {
    let parent_area = parent.width * parent.height;
    if parent_area <= 0.0 {
        return 0.0;
    }
    let used_area = finished.width * finished.height * pieces_per_sheet as f64;
    round2(((parent_area - used_area).max(0.0) / parent_area) * 100.0)
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn round4(value: f64) -> f64 {
    (value * 10000.0).round() / 10000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(finished: SizeInches, parent: SizeInches, quantity: usize) -> LayoutRequest {
        LayoutRequest {
            source_id: None,
            source_pages: Vec::new(),
            finished_size_mode: Default::default(),
            artwork_fit: None,
            page_overrides: Vec::new(),
            source_pdf_size: finished,
            source_trim_box: None,
            source_page_count: None,
            finished_cut_size: finished,
            parent_sheet_size: parent,
            quantity_requested: quantity,
            imposition_mode: ImpositionMode::Repeat,
            impression_quantities: None,
            orientation_preference: OrientationPreference::Auto,
            sides: Sides::Single,
            duplex: None,
            layout_mode: LayoutMode::Auto,
            bleed_option: BleedOption::UseAsIs,
            source_bleed_override: None,
            created_bleed_amount: DEFAULT_CREATED_BLEED_IN,
            gutter: GuttersInches {
                horizontal: 0.0,
                vertical: 0.0,
            },
            manual: None,
        }
    }

    fn size(width: f64, height: f64) -> SizeInches {
        SizeInches { width, height }
    }

    #[test]
    fn business_card_on_12_by_18_uses_best_rotation() {
        let result = generate_layout(request(size(3.5, 2.0), size(12.0, 18.0), 500)).unwrap();

        assert_eq!(result.rotation_degrees, 90);
        assert_eq!(result.rows, 5);
        assert_eq!(result.columns, 6);
        assert_eq!(result.pieces_per_sheet, 30);
        assert_eq!(result.sheets_required, 17);
        assert_eq!(result.extra_pieces_produced, 10);
    }

    #[test]
    fn gutter_adds_space_between_impressions_and_reduces_the_grid() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 24);
        input.gutter = GuttersInches {
            horizontal: 0.25,
            vertical: 0.25,
        };

        let result = generate_layout(input).unwrap();
        let first = &result.placements[0];
        let second = &result.placements[1];

        assert_eq!(result.pieces_per_sheet, 24);
        assert_eq!(result.gutters.horizontal, 0.25);
        assert_eq!(result.gutters.vertical, 0.25);
        let measured_gutter = second.finished_x - first.finished_x - first.finished_width;
        assert!((measured_gutter - 0.25).abs() < 0.001);
    }

    #[test]
    fn layout_rejects_unbounded_dimensions_and_work() {
        let mut input = request(size(0.001, 0.001), size(12.0, 18.0), 1);
        assert!(generate_layout(input.clone())
            .unwrap_err()
            .to_string()
            .contains("at least"));

        input.finished_cut_size = size(0.01, 0.01);
        input.source_pdf_size = input.finished_cut_size;
        assert!(generate_layout(input.clone()).unwrap().pieces_per_sheet <= MAX_LAYOUT_PLACEMENTS);

        input.finished_cut_size = size(3.5, 2.0);
        input.source_pdf_size = input.finished_cut_size;
        input.quantity_requested = MAX_IMPRESSIONS + 1;
        assert!(generate_layout(input)
            .unwrap_err()
            .to_string()
            .contains("requested pieces"));
    }

    #[test]
    fn layout_rejects_too_many_output_sheet_sides() {
        let mut input = request(size(3.5, 2.0), size(3.5, 2.0), MAX_OUTPUT_PAGE_SIDES + 1);
        assert!(generate_layout(input.clone())
            .unwrap_err()
            .to_string()
            .contains("sheet sides"));

        input.quantity_requested = MAX_OUTPUT_PAGE_SIDES / 2 + 1;
        input.sides = Sides::Double;
        input.duplex = Some(default_duplex_settings());
        assert!(generate_layout(input)
            .unwrap_err()
            .to_string()
            .contains("sheet sides"));
    }

    #[test]
    fn common_presets_have_deterministic_layouts() {
        let postcard = generate_layout(request(size(4.0, 6.0), size(12.0, 18.0), 100)).unwrap();
        assert_eq!(
            (postcard.rows, postcard.columns, postcard.pieces_per_sheet),
            (3, 3, 9)
        );

        let invitation = generate_layout(request(size(5.0, 7.0), size(12.0, 18.0), 50)).unwrap();
        assert_eq!(
            (
                invitation.rows,
                invitation.columns,
                invitation.pieces_per_sheet
            ),
            (2, 2, 4)
        );

        let half_sheet = generate_layout(request(size(5.5, 8.5), size(11.0, 17.0), 50)).unwrap();
        assert_eq!(
            (
                half_sheet.rows,
                half_sheet.columns,
                half_sheet.pieces_per_sheet
            ),
            (2, 2, 4)
        );

        let full_sheet = generate_layout(request(size(8.5, 11.0), size(13.0, 19.0), 20)).unwrap();
        assert_eq!(full_sheet.pieces_per_sheet, 2);
    }

    #[test]
    fn manual_layout_rejects_non_fitting_grid() {
        let mut input = request(size(8.5, 11.0), size(12.0, 18.0), 10);
        input.layout_mode = LayoutMode::Manual;
        input.manual = Some(super::super::model::ManualLayout {
            rows: 2,
            columns: 2,
            rotation_degrees: 0,
            margins: None,
        });

        assert!(generate_layout(input).is_err());
    }

    #[test]
    fn custom_n_up_uses_the_requested_columns_and_rows() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 28);
        input.layout_mode = LayoutMode::Manual;
        input.manual = Some(super::super::model::ManualLayout {
            rows: 7,
            columns: 2,
            rotation_degrees: 0,
            margins: None,
        });

        let result = generate_layout(input).unwrap();

        assert_eq!((result.columns, result.rows), (2, 7));
        assert_eq!(result.pieces_per_sheet, 14);
        assert_eq!(result.sheets_required, 2);
        assert_eq!(result.placements.len(), 14);
    }

    #[test]
    fn double_sided_layout_returns_selected_alignment_settings() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 100);
        input.sides = Sides::Double;
        input.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::ShortEdge,
            rotate_back_180: true,
            back_alignment: "client text is recalculated".to_string(),
        });

        let result = generate_layout(input).unwrap();
        let duplex = result.duplex.unwrap();

        assert_eq!(duplex.flip_edge, DuplexFlipEdge::ShortEdge);
        assert!(duplex.rotate_back_180);
        assert!(duplex.back_alignment.contains("short-edge flip"));
        assert!(duplex.back_alignment.contains("rotated 180 degrees"));
    }

    #[test]
    fn unique_single_sided_layout_uses_source_page_count_as_impressions() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 500);
        input.imposition_mode = ImpositionMode::Unique;
        input.source_page_count = Some(35);

        let result = generate_layout(input).unwrap();

        assert_eq!(result.imposition_mode, ImpositionMode::Unique);
        assert_eq!(result.impressions_requested, 35);
        assert_eq!(result.pieces_per_sheet, 30);
        assert_eq!(result.sheets_required, 2);
        assert_eq!(result.total_pieces_produced, 35);
        assert_eq!(result.extra_pieces_produced, 0);
        assert_eq!(result.unused_positions, 25);
    }

    #[test]
    fn unique_double_sided_layout_uses_source_page_pairs_as_impressions() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 500);
        input.imposition_mode = ImpositionMode::Unique;
        input.source_page_count = Some(40);
        input.sides = Sides::Double;
        input.duplex = Some(default_duplex_settings());

        let result = generate_layout(input).unwrap();

        assert_eq!(result.impressions_requested, 20);
        assert_eq!(result.sheets_required, 1);
    }

    #[test]
    fn unique_double_sided_layout_rejects_odd_source_page_count() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 500);
        input.imposition_mode = ImpositionMode::Unique;
        input.source_page_count = Some(3);
        input.sides = Sides::Double;
        input.duplex = Some(default_duplex_settings());

        assert_eq!(
            generate_layout(input).unwrap_err().to_string(),
            "double-sided imposition requires an even source page count"
        );
    }

    #[test]
    fn repeat_layout_sums_exact_per_impression_quantities() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 999);
        input.source_page_count = Some(3);
        input.impression_quantities = Some(vec![10, 0, 1]);

        let result = generate_layout(input).unwrap();

        assert_eq!(result.impressions_requested, 11);
        assert_eq!(result.total_pieces_produced, 11);
        assert_eq!(result.extra_pieces_produced, 0);
        assert_eq!(result.unused_positions, 19);
    }

    #[test]
    fn waste_percentage_is_stable() {
        let result = generate_layout(request(size(4.0, 6.0), size(12.0, 18.0), 100)).unwrap();

        assert_eq!(result.waste_percent, 0.0);
    }

    #[test]
    fn warnings_have_required_parts() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 100);
        input.source_pdf_size = size(3.75, 2.25);
        input.gutter = GuttersInches {
            horizontal: 0.25,
            vertical: 0.25,
        };
        let result = generate_layout(input).unwrap();

        assert!(result
            .warnings
            .iter()
            .all(|warning| warning.problem.ends_with('.')
                && !warning.impact.is_empty()
                && !warning.fix.is_empty()));
    }

    #[test]
    fn uniform_source_bleed_is_not_reported_as_a_size_mismatch() {
        let mut input = request(size(7.0, 5.0), size(12.0, 18.0), 4);
        input.source_pdf_size = size(7.25, 5.25);
        input.gutter = GuttersInches {
            horizontal: 0.299,
            vertical: 0.299,
        };
        input.layout_mode = LayoutMode::Manual;
        input.manual = Some(ManualLayout {
            rows: 2,
            columns: 2,
            rotation_degrees: 90,
            margins: None,
        });

        let result = generate_layout(input).unwrap();

        assert!(!result
            .warnings
            .iter()
            .any(|warning| warning.problem == "Source PDF size does not match finished cut size."));
    }

    #[test]
    fn manual_source_bleed_overrides_the_embedded_trim_box() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 1);
        input.source_pdf_size = size(3.75, 2.25);
        input.source_trim_box = Some(PdfBox {
            left: 0.2,
            bottom: 0.1,
            right: 3.7,
            top: 2.1,
            width: 3.5,
            height: 2.0,
        });
        input.source_bleed_override = Some(0.125);
        input.gutter = GuttersInches {
            horizontal: 0.25,
            vertical: 0.25,
        };

        let result = generate_layout(input).unwrap();

        assert_eq!(result.bleed.source, BleedSource::Manual);
        assert_eq!(result.bleed.effective_amount_per_side, 0.125);
        assert_eq!(result.source_trim_box.unwrap().left, 0.125);
        let placement = &result.placements[0];
        assert!((placement.finished_x - placement.x - 0.125).abs() < 0.001);
        assert!((placement.finished_y - placement.y - 0.125).abs() < 0.001);
        assert!(result
            .warnings
            .iter()
            .all(|warning| warning.problem != "No bleed detected."));
    }

    #[test]
    fn manual_source_bleed_requires_the_derived_finished_size() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 1);
        input.source_pdf_size = size(3.75, 2.25);
        input.source_bleed_override = Some(0.25);

        let error = generate_layout(input).unwrap_err();

        assert!(error
            .to_string()
            .contains("finished cut size must equal the source PDF size"));
    }

    #[test]
    fn asymmetric_source_size_is_still_reported_as_a_mismatch() {
        let mut input = request(size(7.0, 5.0), size(12.0, 18.0), 1);
        input.source_pdf_size = size(7.5, 5.25);
        input.gutter = GuttersInches {
            horizontal: 0.5,
            vertical: 0.25,
        };

        let result = generate_layout(input).unwrap();

        assert!(result
            .warnings
            .iter()
            .any(|warning| warning.problem == "Source PDF size does not match finished cut size."));
    }

    #[test]
    fn placements_center_asymmetric_and_single_axis_bleed() {
        let mut asymmetric = request(size(3.5, 2.0), size(12.0, 18.0), 1);
        asymmetric.source_pdf_size = size(3.75, 2.5);
        asymmetric.orientation_preference = OrientationPreference::Landscape;
        asymmetric.gutter = GuttersInches {
            horizontal: 0.5,
            vertical: 0.5,
        };
        let asymmetric = generate_layout(asymmetric).unwrap();
        let placement = &asymmetric.placements[0];
        assert!((placement.finished_x - placement.x - 0.125).abs() < 0.001);
        assert!((placement.finished_y - placement.y - 0.25).abs() < 0.001);

        let mut horizontal_only = request(size(3.5, 2.0), size(12.0, 18.0), 1);
        horizontal_only.source_pdf_size = size(3.75, 2.0);
        horizontal_only.orientation_preference = OrientationPreference::Landscape;
        horizontal_only.gutter = GuttersInches {
            horizontal: 0.25,
            vertical: 0.0,
        };
        let horizontal_only = generate_layout(horizontal_only).unwrap();
        let placement = &horizontal_only.placements[0];
        assert!((placement.finished_x - placement.x - 0.125).abs() < 0.001);
        assert!((placement.finished_y - placement.y).abs() < 0.001);
        assert!(horizontal_only
            .warnings
            .iter()
            .any(|warning| warning.problem == "No bleed detected."));
    }

    #[test]
    fn automatic_layout_keeps_yield_and_warns_when_gutters_clip_bleed() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 1);
        input.source_pdf_size = size(3.75, 2.25);
        input.orientation_preference = OrientationPreference::Landscape;

        let result = generate_layout(input).unwrap();

        assert!(result.pieces_per_sheet > 1);
        assert!(!artwork_is_preserved(&result));
        assert!(result
            .warnings
            .iter()
            .any(|warning| warning.problem == "Bleed is wider than the available gutter."));
    }

    #[test]
    fn placements_align_an_asymmetric_trim_box_with_the_finished_cut() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 1);
        input.source_pdf_size = size(3.75, 2.25);
        input.source_trim_box = Some(PdfBox {
            left: 0.2,
            bottom: 0.1,
            right: 3.7,
            top: 2.1,
            width: 3.5,
            height: 2.0,
        });
        input.orientation_preference = OrientationPreference::Landscape;
        input.gutter = GuttersInches {
            horizontal: 0.25,
            vertical: 0.25,
        };

        let result = generate_layout(input).unwrap();
        let placement = &result.placements[0];

        assert!((placement.finished_x - placement.x - 0.2).abs() < 0.001);
        assert!((placement.finished_y - placement.y - 0.15).abs() < 0.001);
    }

    #[test]
    fn automatic_layout_shifts_an_asymmetric_bleed_grid_to_preserve_yield() {
        let mut input = request(size(3.5, 2.0), size(11.25, 2.25), 3);
        input.source_pdf_size = size(3.75, 2.25);
        input.source_trim_box = Some(PdfBox {
            left: 0.25,
            bottom: 0.125,
            right: 3.75,
            top: 2.125,
            width: 3.5,
            height: 2.0,
        });
        input.orientation_preference = OrientationPreference::Landscape;
        input.gutter = GuttersInches {
            horizontal: 0.25,
            vertical: 0.25,
        };

        let result = generate_layout(input).unwrap();

        assert_eq!(result.columns, 3);
        assert_eq!(result.pieces_per_sheet, 3);
        assert!((result.margins.left - 0.25).abs() < 0.001);
        assert!(result.margins.right.abs() < 0.001);
        assert!(artwork_is_preserved(&result));
    }

    #[test]
    fn placement_centers_artwork_smaller_than_the_finished_cut() {
        let mut input = request(size(4.0, 2.0), size(12.0, 18.0), 1);
        input.source_pdf_size = size(3.0, 1.0);
        input.orientation_preference = OrientationPreference::Landscape;

        let result = generate_layout(input).unwrap();
        let placement = &result.placements[0];

        assert!((placement.x - placement.finished_x - 0.5).abs() < 0.001);
        assert!((placement.y - placement.finished_y - 0.5).abs() < 0.001);
    }

    #[test]
    fn layout_rejects_non_finite_gutters_and_manual_margins() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 1);
        input.gutter.horizontal = f64::NAN;
        assert!(generate_layout(input)
            .unwrap_err()
            .to_string()
            .contains("finite"));

        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 1);
        input.layout_mode = LayoutMode::Manual;
        input.manual = Some(ManualLayout {
            rows: 1,
            columns: 1,
            rotation_degrees: 0,
            margins: Some(MarginsInches {
                left: f64::NAN,
                ..MarginsInches::default()
            }),
        });
        assert!(generate_layout(input)
            .unwrap_err()
            .to_string()
            .contains("finite"));
    }

    #[test]
    fn layout_rejects_contradictory_mode_settings() {
        let mut single_with_duplex = request(size(3.5, 2.0), size(12.0, 18.0), 1);
        single_with_duplex.duplex = Some(default_duplex_settings());
        assert!(validate_request(&single_with_duplex).is_err());

        let mut double_without_duplex = request(size(3.5, 2.0), size(12.0, 18.0), 1);
        double_without_duplex.sides = Sides::Double;
        double_without_duplex.source_page_count = Some(2);
        assert!(validate_request(&double_without_duplex).is_err());

        let mut auto_with_manual = request(size(3.5, 2.0), size(12.0, 18.0), 1);
        auto_with_manual.manual = Some(ManualLayout {
            rows: 1,
            columns: 1,
            rotation_degrees: 0,
            margins: None,
        });
        assert!(validate_request(&auto_with_manual).is_err());
    }

    #[test]
    fn manual_layout_rejects_contradictory_four_sided_margins() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 1);
        input.layout_mode = LayoutMode::Manual;
        input.manual = Some(ManualLayout {
            rows: 1,
            columns: 1,
            rotation_degrees: 0,
            margins: Some(MarginsInches {
                top: 1.0,
                right: 0.0,
                bottom: 0.0,
                left: 1.0,
            }),
        });

        let validated = ValidatedLayoutRequest::try_from(input).unwrap();
        let error = candidate_for(&validated, 0, Some((1, 1))).unwrap_err();
        assert!(
            error.to_string().contains("margins do not fit"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn manual_layout_reports_the_placement_safety_limit_precisely() {
        let mut input = request(size(0.01, 0.01), size(100.0, 100.0), 1);
        input.layout_mode = LayoutMode::Manual;
        input.manual = Some(ManualLayout {
            rows: 23,
            columns: 23,
            rotation_degrees: 0,
            margins: None,
        });

        assert_eq!(
            generate_layout(input).unwrap_err().to_string(),
            "custom n-up grid cannot exceed 512 placements"
        );
    }

    #[test]
    fn orientation_preference_controls_impression_rotation() {
        let mut input = request(size(3.5, 2.0), size(12.0, 18.0), 500);
        input.orientation_preference = OrientationPreference::Landscape;
        let landscape = generate_layout(input.clone()).unwrap();
        assert_eq!(landscape.rotation_degrees, 0);
        assert!(landscape.placements[0].finished_width > landscape.placements[0].finished_height);

        input.orientation_preference = OrientationPreference::Portrait;
        let portrait = generate_layout(input).unwrap();
        assert_eq!(portrait.rotation_degrees, 90);
        assert!(portrait.placements[0].finished_width < portrait.placements[0].finished_height);
        assert!(portrait.pieces_per_sheet > landscape.pieces_per_sheet);
    }
}
