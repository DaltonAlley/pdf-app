//! Page-specific finished-size planning. All rectangles use top-left inches.
use super::model::*;
use crate::error::{AppError, AppResult};

pub(crate) fn enabled(request: &LayoutRequest) -> bool {
    request.artwork_fit.is_some()
        || request.finished_size_mode == FinishedSizeMode::Original
        || !request.page_overrides.is_empty()
        || request.source_pages.windows(2).any(|pair| {
            pair[0].source_pdf_size != pair[1].source_pdf_size
                || pair[0].source_trim_box != pair[1].source_trim_box
        })
}

fn prepare(mut request: LayoutRequest) -> AppResult<(LayoutRequest, Vec<PagePlan>)> {
    if request.source_pages.is_empty() {
        let count = request.source_page_count.unwrap_or(1);
        if count == 0 || count > crate::MAX_SOURCE_PDF_PAGES {
            return Err(AppError::bad_request("invalid source page count"));
        }
        request.source_pages = vec![
            SourcePage {
                source_pdf_size: request.source_pdf_size,
                source_trim_box: request.source_trim_box,
                preview_box: None,
                physical_size_assumed: false,
                filename: None,
                original_page_number: None,
            };
            count
        ];
    }
    if request.source_pages.len() > crate::MAX_SOURCE_PDF_PAGES
        || request
            .source_page_count
            .is_some_and(|count| count != request.source_pages.len())
    {
        return Err(AppError::bad_request(
            "sourcePages must match source page count",
        ));
    }
    request.source_page_count = Some(request.source_pages.len());
    let mut scalar_request = request.clone();
    scalar_request.source_pages.clear();
    scalar_request.page_overrides.clear();
    scalar_request.artwork_fit = None;
    scalar_request.finished_size_mode = FinishedSizeMode::Common;
    scalar_request.source_bleed_override = None;
    super::layout::validate_request(&scalar_request)?;
    if request
        .source_bleed_override
        .is_some_and(|b| !b.is_finite() || !(0.001..=1.0).contains(&b))
    {
        return Err(AppError::bad_request(
            "source bleed override must be from 0.001 to 1 inch",
        ));
    }
    if request.sides == Sides::Double && !request.source_pages.len().is_multiple_of(2) {
        return Err(AppError::bad_request(
            "double-sided imposition requires an even source page count",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for entry in &request.page_overrides {
        if entry.page_number == 0
            || entry.page_number > request.source_pages.len()
            || !seen.insert(entry.page_number)
        {
            return Err(AppError::bad_request(
                "page overrides must name distinct existing pages",
            ));
        }
    }
    let mut plans = Vec::with_capacity(request.source_pages.len());
    for (index, page) in request.source_pages.iter().enumerate() {
        super::layout::validate_size(page.source_pdf_size, "source page size")?;
        super::layout::validate_source_trim_box(page.source_trim_box, page.source_pdf_size)?;
        // Preview crop boxes can extend past an explicit BleedBox, but must be valid rectangles.
        if let Some(rect) = page.preview_box {
            if [
                rect.left,
                rect.bottom,
                rect.right,
                rect.top,
                rect.width,
                rect.height,
            ]
            .iter()
            .any(|v| !v.is_finite())
                || rect.width <= 0.0
                || rect.height <= 0.0
                || (rect.right - rect.left - rect.width).abs() > 0.01
                || (rect.top - rect.bottom - rect.height).abs() > 0.01
            {
                return Err(AppError::bad_request("invalid preview box"));
            }
        }
        let override_ = request
            .page_overrides
            .iter()
            .find(|entry| entry.page_number == index + 1);
        if request.finished_size_mode == FinishedSizeMode::Original
            && page.physical_size_assumed
            && override_
                .and_then(|entry| entry.finished_cut_size)
                .is_none()
        {
            return Err(AppError::bad_request("image physical size is assumed at 300 DPI; choose a common or explicit finished size"));
        }
        let basis = page.source_trim_box.unwrap_or(PdfBox {
            left: 0.0,
            bottom: 0.0,
            right: page.source_pdf_size.width,
            top: page.source_pdf_size.height,
            width: page.source_pdf_size.width,
            height: page.source_pdf_size.height,
        });
        let finished = override_
            .and_then(|entry| entry.finished_cut_size)
            .unwrap_or_else(|| {
                if request.finished_size_mode == FinishedSizeMode::Original {
                    SizeInches {
                        width: basis.width,
                        height: basis.height,
                    }
                } else {
                    request.finished_cut_size
                }
            });
        super::layout::validate_size(finished, "finished cut size")?;
        let fit = override_
            .and_then(|entry| entry.artwork_fit)
            .or(request.artwork_fit)
            .unwrap_or(ArtworkFit {
                mode: ArtworkFitMode::Contain,
                position: CropPosition::default(),
            });
        if !fit.position.x.is_finite()
            || !fit.position.y.is_finite()
            || !(0.0..=1.0).contains(&fit.position.x)
            || !(0.0..=1.0).contains(&fit.position.y)
        {
            return Err(AppError::bad_request(
                "crop position x and y must be from 0 to 1",
            ));
        }
        let scale_x = finished.width / basis.width;
        let scale_y = finished.height / basis.height;
        let (mut scale_x, mut scale_y) = match fit.mode {
            ArtworkFitMode::Contain => (scale_x.min(scale_y), scale_x.min(scale_y)),
            ArtworkFitMode::Cover => (scale_x.max(scale_y), scale_x.max(scale_y)),
            ArtworkFitMode::Stretch => (scale_x, scale_y),
        };
        let mut bleed = 0.0;
        if request.bleed_option == BleedOption::ScaleToBleed {
            if !request.created_bleed_amount.is_finite()
                || !(0.001..=1.0).contains(&request.created_bleed_amount)
            {
                return Err(AppError::bad_request(
                    "created bleed amount must be from 0.001 to 1 inch",
                ));
            }
            bleed = request.created_bleed_amount;
            // Enlarge the fitted composition uniformly. Preserve normalized crop
            // position relative to the expanded bleed frame, not a cut-edge pixel.
            // A cut landmark can shift by up to the bleed amount at edge anchors.
            let enlargement = ((finished.width + 2.0 * bleed) / finished.width)
                .max((finished.height + 2.0 * bleed) / finished.height);
            scale_x *= enlargement;
            scale_y *= enlargement;
        } else if request.bleed_option == BleedOption::UseAsIs {
            // Stretch changes the two axes independently. Only the smallest
            // transformed edge is available as uniform bleed on all four sides.
            let supplied = [
                basis.left * scale_x,
                basis.bottom * scale_y,
                (page.source_pdf_size.width - basis.right) * scale_x,
                (page.source_pdf_size.height - basis.top) * scale_y,
            ]
            .into_iter()
            .fold(f64::INFINITY, f64::min)
            .max(0.0);
            bleed = supplied;
        }
        let added = if request.bleed_option == BleedOption::ScaleToBleed {
            bleed
        } else {
            0.0
        };
        let artwork = PlanRect {
            x: -added + (finished.width + 2.0 * added - basis.width * scale_x) * fit.position.x
                - basis.left * scale_x,
            y: -added + (finished.height + 2.0 * added - basis.height * scale_y) * fit.position.y
                - (page.source_pdf_size.height - basis.top) * scale_y,
            width: page.source_pdf_size.width * scale_x,
            height: page.source_pdf_size.height * scale_y,
        };
        plans.push(PagePlan {
            position_travel: CropPosition {
                x: finished.width + 2.0 * added - basis.width * scale_x,
                y: finished.height + 2.0 * added - basis.height * scale_y,
            },
            page_number: index + 1,
            source_pdf_size: page.source_pdf_size,
            preview_box: page.preview_box,
            finished_cut_size: finished,
            bleed_amount: bleed,
            artwork,
        });
    }
    if request.sides == Sides::Double {
        for pair in plans.chunks_exact(2) {
            if (pair[0].finished_cut_size.width - pair[1].finished_cut_size.width).abs() > 0.0001
                || (pair[0].finished_cut_size.height - pair[1].finished_cut_size.height).abs()
                    > 0.0001
            {
                return Err(AppError::bad_request("duplex front and back finished sizes must match; choose a common finished size or matching page overrides"));
            }
        }
    }
    Ok((request, plans))
}

pub(crate) fn validate_request(request: &LayoutRequest) -> AppResult<()> {
    prepare(request.clone()).map(|_| ())
}

pub(crate) fn generate(request: LayoutRequest) -> AppResult<LayoutResult> {
    let (request, plans) = prepare(request)?;
    let slot = plans.iter().fold(
        SizeInches {
            width: 0.0,
            height: 0.0,
        },
        |largest, plan| SizeInches {
            width: largest
                .width
                .max(plan.finished_cut_size.width + 2.0 * plan.bleed_amount),
            height: largest
                .height
                .max(plan.finished_cut_size.height + 2.0 * plan.bleed_amount),
        },
    );
    let mut grid = request.clone();
    grid.source_pages.clear();
    grid.page_overrides.clear();
    grid.finished_size_mode = FinishedSizeMode::Common;
    grid.artwork_fit = None;
    grid.source_pdf_size = slot;
    grid.finished_cut_size = slot;
    grid.source_trim_box = None;
    grid.source_bleed_override = None;
    grid.bleed_option = BleedOption::UseAsIs;
    let mut result = super::layout::generate_layout(grid)?;
    result.source_pdf_size = request.source_pages[0].source_pdf_size;
    result.source_trim_box = request.source_pages[0].source_trim_box;
    result.source_bleed_override = request.source_bleed_override;
    result.finished_cut_size = request.finished_cut_size;
    result.bleed.option = request.bleed_option;
    result.bleed.effective_amount_per_side =
        plans.iter().map(|p| p.bleed_amount).fold(0.0, f64::max);
    result.page_plans = plans;
    result.warnings.clear();
    if request.bleed_option == BleedOption::ScaleToBleed {
        result.warnings.push(ProductionWarning::new("Artwork enlarged to add bleed.",
            "Artwork is enlarged uniformly using the same normalized crop position in the bleed frame. More edge content is cut away, and edge landmarks can shift by up to the bleed amount. Contain may retain white borders.",
            "Review the cut and bleed preview, especially text near the edges."));
    }
    Ok(result)
}

#[cfg(test)]
#[path = "mixed_tests.rs"]
mod tests;

/// Resolve an individual cut and artwork into a regular grid slot, before duplex.
pub(crate) fn placed_rects(
    plan: &PagePlan,
    slot: &PiecePlacement,
    rotation: u16,
) -> (PlanRect, PlanRect) {
    let cut = plan.finished_cut_size;
    let (width, height) = if rotation == 90 {
        (cut.height, cut.width)
    } else {
        (cut.width, cut.height)
    };
    let x = slot.finished_x + (slot.finished_width - width) / 2.0;
    let y = slot.finished_y + (slot.finished_height - height) / 2.0;
    let a = plan.artwork;
    let artwork = if rotation == 90 {
        PlanRect {
            x: x + cut.height - a.y - a.height,
            y: y + a.x,
            width: a.height,
            height: a.width,
        }
    } else {
        PlanRect {
            x: x + a.x,
            y: y + a.y,
            ..a
        }
    };
    let b = plan.bleed_amount;
    (
        artwork,
        PlanRect {
            x: x - b,
            y: y - b,
            width: width + 2.0 * b,
            height: height + 2.0 * b,
        },
    )
}
