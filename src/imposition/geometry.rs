use std::collections::HashSet;

use lopdf::{Document as LoDocument, Object, ObjectId};

use super::model::{PdfBox, SizeInches};
use crate::error::{AppError, AppResult};

const PT_PER_IN: f64 = 72.0;
const BOX_TOLERANCE_POINTS: f64 = 0.01;
const COMMON_BLEEDS_INCHES: [f64; 3] = [0.0625, 0.125, 0.25];

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourcePageGeometry {
    pub(crate) size: SizeInches,
    pub(crate) page_box: [f64; 4],
    pub(crate) normalization: [f64; 6],
    pub(crate) media_box: Option<PdfBox>,
    pub(crate) crop_box: PdfBox,
    pub(crate) bleed_box: Option<PdfBox>,
    pub(crate) trim_box: Option<PdfBox>,
}

#[derive(Clone, Copy)]
pub(crate) struct PageGeometryParts {
    pub(crate) media: Option<[f64; 4]>,
    pub(crate) crop: Option<[f64; 4]>,
    pub(crate) bleed: Option<[f64; 4]>,
    pub(crate) trim: Option<[f64; 4]>,
    pub(crate) rotation: i64,
    pub(crate) user_unit: f64,
}

#[cfg(test)]
pub(crate) fn same_imposition_geometry(
    left: &SourcePageGeometry,
    right: &SourcePageGeometry,
    tolerance: f64,
) -> bool {
    same_size(left.size, right.size, tolerance)
        && same_box(
            left.trim_box.as_ref().unwrap_or(&left.crop_box),
            right.trim_box.as_ref().unwrap_or(&right.crop_box),
            tolerance,
        )
}

#[cfg(test)]
fn same_size(left: SizeInches, right: SizeInches, tolerance: f64) -> bool {
    (left.width - right.width).abs() <= tolerance && (left.height - right.height).abs() <= tolerance
}

#[cfg(test)]
fn same_box(left: &PdfBox, right: &PdfBox, tolerance: f64) -> bool {
    (left.left - right.left).abs() <= tolerance
        && (left.bottom - right.bottom).abs() <= tolerance
        && (left.right - right.right).abs() <= tolerance
        && (left.top - right.top).abs() <= tolerance
}

#[cfg(test)]
pub(crate) fn source_page_geometry(
    document: &LoDocument,
    page_id: ObjectId,
) -> AppResult<SourcePageGeometry> {
    source_page_geometry_with_bleed(document, page_id, None)
}

pub(crate) fn source_page_geometry_with_bleed(
    document: &LoDocument,
    page_id: ObjectId,
    source_bleed_override: Option<f64>,
) -> AppResult<SourcePageGeometry> {
    let media_object = inherited_page_attribute(document, page_id, b"MediaBox")?;
    let crop_object = inherited_page_attribute(document, page_id, b"CropBox")?;
    let bleed_object = inherited_page_attribute(document, page_id, b"BleedBox")?;
    let trim_object = inherited_page_attribute(document, page_id, b"TrimBox")?;
    let media_points = media_object
        .as_ref()
        .map(|object| page_box_points(document, object))
        .transpose()?;
    let crop_points = crop_object
        .as_ref()
        .map(|object| page_box_points(document, object))
        .transpose()?;
    let bleed_points = bleed_object
        .as_ref()
        .map(|object| page_box_points(document, object))
        .transpose()?;
    let trim_points = trim_object
        .as_ref()
        .map(|object| page_box_points(document, object))
        .transpose()?;
    let rotation = optional_page_integer(document, page_id, b"Rotate", 0)?.rem_euclid(360);
    let user_unit = optional_page_number(document, page_id, b"UserUnit", 1.0)?;
    source_page_geometry_from_parts(
        PageGeometryParts {
            media: media_points,
            crop: crop_points,
            bleed: bleed_points,
            trim: trim_points,
            rotation,
            user_unit,
        },
        source_bleed_override,
    )
}

fn optional_page_integer(
    document: &LoDocument,
    page_id: ObjectId,
    key: &[u8],
    default: i64,
) -> AppResult<i64> {
    let Some(value) = inherited_page_attribute(document, page_id, key)? else {
        return Ok(default);
    };
    dereference_object(document, &value)
        .and_then(Object::as_i64)
        .map_err(|error| {
            AppError::bad_request(format!(
                "source PDF has an invalid {} value: {error}",
                String::from_utf8_lossy(key)
            ))
        })
}

fn optional_page_number(
    document: &LoDocument,
    page_id: ObjectId,
    key: &[u8],
    default: f64,
) -> AppResult<f64> {
    let Some(value) = inherited_page_attribute(document, page_id, key)? else {
        return Ok(default);
    };
    dereference_object(document, &value)
        .and_then(Object::as_float)
        .map(f64::from)
        .map_err(|error| {
            AppError::bad_request(format!(
                "source PDF has an invalid {} value: {error}",
                String::from_utf8_lossy(key)
            ))
        })
}

pub(crate) fn source_page_geometry_from_parts(
    parts: PageGeometryParts,
    source_bleed_override: Option<f64>,
) -> AppResult<SourcePageGeometry> {
    let PageGeometryParts {
        media,
        crop,
        bleed,
        trim,
        rotation,
        user_unit,
    } = parts;
    if !user_unit.is_finite() || user_unit <= 0.0 {
        return Err(AppError::bad_request("source PDF has an invalid user unit"));
    }
    let fallback_points = crop
        .or(media)
        .ok_or_else(|| AppError::bad_request("source PDF page has no media box"))?;
    let finished_points = trim.or(crop).unwrap_or(fallback_points);
    let (page_box, effective_finished_points) = resolved_artwork_points(
        &parts,
        fallback_points,
        finished_points,
        source_bleed_override,
    )?;
    let [left, bottom, right, top] = page_box;

    let width = (right - left) * user_unit;
    let height = (top - bottom) * user_unit;
    if width <= 0.0 || height <= 0.0 {
        return Err(AppError::bad_request(
            "source PDF page has an invalid page box",
        ));
    }
    let (form_width, form_height, normalization) = match rotation {
        0 => (
            width,
            height,
            [
                user_unit,
                0.0,
                0.0,
                user_unit,
                -left * user_unit,
                -bottom * user_unit,
            ],
        ),
        90 => (
            height,
            width,
            [
                0.0,
                -user_unit,
                user_unit,
                0.0,
                -bottom * user_unit,
                right * user_unit,
            ],
        ),
        180 => (
            width,
            height,
            [
                -user_unit,
                0.0,
                0.0,
                -user_unit,
                right * user_unit,
                top * user_unit,
            ],
        ),
        270 => (
            height,
            width,
            [
                0.0,
                user_unit,
                -user_unit,
                0.0,
                top * user_unit,
                -left * user_unit,
            ],
        ),
        _ => {
            return Err(AppError::bad_request(
                "source PDF page rotation must be a multiple of 90 degrees",
            ))
        }
    };

    let normalized_box = |points: [f64; 4]| transformed_box(points, normalization);
    let crop_box = normalized_box(crop.unwrap_or(page_box))?;
    let media_box = media.map(normalized_box).transpose()?;
    let bleed_box = bleed.map(normalized_box).transpose()?;
    let effective_finished_box = normalized_box(effective_finished_points)?;
    let trim_box = if trim.is_some() || !same_points(effective_finished_points, page_box) {
        Some(effective_finished_box)
    } else {
        None
    };

    Ok(SourcePageGeometry {
        size: SizeInches {
            width: round4(form_width / PT_PER_IN),
            height: round4(form_height / PT_PER_IN),
        },
        page_box,
        normalization,
        media_box,
        crop_box,
        bleed_box,
        trim_box,
    })
}

fn resolved_artwork_points(
    parts: &PageGeometryParts,
    fallback: [f64; 4],
    finished: [f64; 4],
    source_bleed_override: Option<f64>,
) -> AppResult<([f64; 4], [f64; 4])> {
    let PageGeometryParts {
        media,
        crop,
        bleed,
        trim,
        user_unit,
        rotation: _,
    } = *parts;
    if let Some(amount) = source_bleed_override {
        let inset_points = amount * PT_PER_IN / user_unit;
        if trim.is_some()
            || crop
                .zip(media)
                .is_some_and(|(crop, media)| box_has_uniform_margin(media, crop, amount, user_unit))
        {
            let requested = expanded_box(finished, inset_points);
            let Some(media) = media else {
                return Err(AppError::bad_request(
                    "source PDF has no media box containing the requested bleed",
                ));
            };
            if !box_contains(media, requested) {
                return Err(AppError::bad_request(format!(
                    "source PDF does not contain {amount:.4} in of artwork outside every finished edge"
                )));
            }
            return Ok((requested, finished));
        }

        let inferred_finished = inset_box(fallback, inset_points)?;
        return Ok((fallback, inferred_finished));
    }

    if let Some(bleed) = bleed {
        if !box_contains(bleed, finished) {
            return Err(AppError::bad_request(
                "source PDF bleed box does not contain the finished boundary",
            ));
        }
        return Ok((bleed, finished));
    }

    if trim.is_some() {
        if let Some(crop) =
            crop.filter(|crop| box_contains(*crop, finished) && !same_points(*crop, finished))
        {
            return Ok((crop, finished));
        }
    }

    if let Some(media) = media {
        if uniform_common_bleed(media, finished, user_unit) {
            return Ok((media, finished));
        }
    }

    Ok((fallback, finished))
}

fn uniform_common_bleed(outer: [f64; 4], finished: [f64; 4], user_unit: f64) -> bool {
    if !box_contains(outer, finished) {
        return false;
    }
    let amounts = [
        (finished[0] - outer[0]) * user_unit / PT_PER_IN,
        (finished[1] - outer[1]) * user_unit / PT_PER_IN,
        (outer[2] - finished[2]) * user_unit / PT_PER_IN,
        (outer[3] - finished[3]) * user_unit / PT_PER_IN,
    ];
    COMMON_BLEEDS_INCHES.into_iter().any(|common| {
        amounts
            .iter()
            .all(|amount| (*amount - common).abs() <= 0.01)
    })
}

fn box_has_uniform_margin(outer: [f64; 4], inner: [f64; 4], amount: f64, user_unit: f64) -> bool {
    if !box_contains(outer, inner) {
        return false;
    }
    [
        inner[0] - outer[0],
        inner[1] - outer[1],
        outer[2] - inner[2],
        outer[3] - inner[3],
    ]
    .into_iter()
    .all(|points| (points * user_unit / PT_PER_IN - amount).abs() <= 0.01)
}

fn expanded_box(inner: [f64; 4], amount: f64) -> [f64; 4] {
    [
        inner[0] - amount,
        inner[1] - amount,
        inner[2] + amount,
        inner[3] + amount,
    ]
}

fn inset_box(outer: [f64; 4], amount: f64) -> AppResult<[f64; 4]> {
    let inner = [
        outer[0] + amount,
        outer[1] + amount,
        outer[2] - amount,
        outer[3] - amount,
    ];
    if inner[2] <= inner[0] || inner[3] <= inner[1] {
        return Err(AppError::bad_request(
            "source bleed override leaves no valid finished size",
        ));
    }
    Ok(inner)
}

fn box_contains(outer: [f64; 4], inner: [f64; 4]) -> bool {
    outer[0] <= inner[0] + BOX_TOLERANCE_POINTS
        && outer[1] <= inner[1] + BOX_TOLERANCE_POINTS
        && outer[2] + BOX_TOLERANCE_POINTS >= inner[2]
        && outer[3] + BOX_TOLERANCE_POINTS >= inner[3]
}

fn same_points(left: [f64; 4], right: [f64; 4]) -> bool {
    left.iter()
        .zip(right)
        .all(|(left, right)| (*left - right).abs() <= BOX_TOLERANCE_POINTS)
}

pub(super) fn transformed_box(points: [f64; 4], matrix: [f64; 6]) -> AppResult<PdfBox> {
    let [left, bottom, right, top] = points;
    if right <= left || top <= bottom {
        return Err(AppError::bad_request(
            "source PDF page has an invalid page box",
        ));
    }
    let corners = [
        transform_point(matrix, left, bottom),
        transform_point(matrix, left, top),
        transform_point(matrix, right, bottom),
        transform_point(matrix, right, top),
    ];
    let min_x = corners
        .iter()
        .map(|(x, _)| *x)
        .fold(f64::INFINITY, f64::min);
    let max_x = corners
        .iter()
        .map(|(x, _)| *x)
        .fold(f64::NEG_INFINITY, f64::max);
    let min_y = corners
        .iter()
        .map(|(_, y)| *y)
        .fold(f64::INFINITY, f64::min);
    let max_y = corners
        .iter()
        .map(|(_, y)| *y)
        .fold(f64::NEG_INFINITY, f64::max);
    Ok(PdfBox {
        left: round4(min_x / PT_PER_IN),
        bottom: round4(min_y / PT_PER_IN),
        right: round4(max_x / PT_PER_IN),
        top: round4(max_y / PT_PER_IN),
        width: round4((max_x - min_x) / PT_PER_IN),
        height: round4((max_y - min_y) / PT_PER_IN),
    })
}

fn transform_point(matrix: [f64; 6], x: f64, y: f64) -> (f64, f64) {
    (
        matrix[0] * x + matrix[2] * y + matrix[4],
        matrix[1] * x + matrix[3] * y + matrix[5],
    )
}

pub(crate) fn inherited_page_attribute(
    document: &LoDocument,
    page_id: ObjectId,
    key: &[u8],
) -> AppResult<Option<Object>> {
    let mut current_id = page_id;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current_id) {
            return Err(AppError::bad_request(
                "source PDF page tree contains a cycle",
            ));
        }
        let node = document.get_dictionary(current_id).map_err(|err| {
            AppError::bad_request(format!("could not read source PDF page tree: {err}"))
        })?;
        if let Ok(value) = node.get(key) {
            return Ok(Some(value.clone()));
        }
        let Ok(parent_id) = node.get(b"Parent").and_then(Object::as_reference) else {
            return Ok(None);
        };
        current_id = parent_id;
    }
}

fn dereference_object<'a>(
    document: &'a LoDocument,
    object: &'a Object,
) -> lopdf::Result<&'a Object> {
    match object {
        Object::Reference(id) => document.get_object(*id),
        value => Ok(value),
    }
}

fn page_box_points(document: &LoDocument, object: &Object) -> AppResult<[f64; 4]> {
    let array = dereference_object(document, object)
        .and_then(Object::as_array)
        .map_err(|err| {
            AppError::bad_request(format!("source PDF has an invalid page box: {err}"))
        })?;
    if array.len() != 4 {
        return Err(AppError::bad_request(
            "source PDF page box must contain four numbers",
        ));
    }
    let mut points = [0.0; 4];
    for (point, value) in points.iter_mut().zip(array) {
        *point = f64::from(
            dereference_object(document, value)
                .and_then(Object::as_float)
                .map_err(|err| {
                    AppError::bad_request(format!("source PDF has an invalid page box: {err}"))
                })?,
        );
    }
    if points.iter().any(|point| !point.is_finite()) {
        return Err(AppError::bad_request(
            "source PDF page box must contain finite numbers",
        ));
    }
    Ok(points)
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 0.001,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn normalizes_rotated_crop_and_asymmetric_trim_boxes() {
        let mut document = LoDocument::with_version("1.7");
        let page = document.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 200.into()],
            "CropBox" => vec![20.into(), 10.into(), 280.into(), 190.into()],
            "TrimBox" => vec![30.into(), 25.into(), 260.into(), 175.into()],
            "Rotate" => 90,
        });

        let geometry = source_page_geometry(&document, page).unwrap();

        assert_close(geometry.size.width, 180.0 / PT_PER_IN);
        assert_close(geometry.size.height, 260.0 / PT_PER_IN);
        let trim = geometry.trim_box.unwrap();
        assert_close(trim.left, 15.0 / PT_PER_IN);
        assert_close(trim.bottom, 20.0 / PT_PER_IN);
        assert_close(trim.width, 150.0 / PT_PER_IN);
        assert_close(trim.height, 230.0 / PT_PER_IN);
    }

    #[test]
    fn malformed_rotation_and_user_unit_values_are_rejected() {
        let mut invalid_rotation = LoDocument::with_version("1.7");
        let page = invalid_rotation.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 200.into()],
            "Rotate" => Object::Reference((999, 0)),
        });
        assert!(source_page_geometry(&invalid_rotation, page)
            .unwrap_err()
            .to_string()
            .contains("Rotate"));

        let mut invalid_user_unit = LoDocument::with_version("1.7");
        let page = invalid_user_unit.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 200.into()],
            "UserUnit" => "large",
        });
        assert!(source_page_geometry(&invalid_user_unit, page)
            .unwrap_err()
            .to_string()
            .contains("UserUnit"));
    }

    #[test]
    fn omitted_trim_box_matches_an_explicit_crop_sized_trim_box() {
        let mut document = LoDocument::with_version("1.7");
        let rotated = document.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 378.into(), 522.into()],
            "Rotate" => 270,
        });
        let unrotated = document.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 522.into(), 378.into()],
            "TrimBox" => vec![0.into(), 0.into(), 522.into(), 378.into()],
        });

        let rotated = source_page_geometry(&document, rotated).unwrap();
        let unrotated = source_page_geometry(&document, unrotated).unwrap();

        assert!(same_imposition_geometry(&rotated, &unrotated, 0.01));
    }

    #[test]
    fn omitted_trim_box_does_not_match_a_genuinely_inset_trim_box() {
        let mut document = LoDocument::with_version("1.7");
        let full_page = document.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 522.into(), 378.into()],
        });
        let inset_trim = document.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 522.into(), 378.into()],
            "TrimBox" => vec![9.into(), 9.into(), 513.into(), 369.into()],
        });

        let full_page = source_page_geometry(&document, full_page).unwrap();
        let inset_trim = source_page_geometry(&document, inset_trim).unwrap();

        assert!(!same_imposition_geometry(&full_page, &inset_trim, 0.01));
    }

    #[test]
    fn crop_is_finished_and_common_media_margin_is_supplied_bleed() {
        let mut document = LoDocument::with_version("1.7");
        let page = document.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 162.into(), 270.into()],
            "CropBox" => vec![9.into(), 9.into(), 153.into(), 261.into()],
        });

        let geometry = source_page_geometry(&document, page).unwrap();

        assert_close(geometry.size.width, 2.25);
        assert_close(geometry.size.height, 3.75);
        let trim = geometry.trim_box.unwrap();
        assert_close(trim.left, 0.125);
        assert_close(trim.bottom, 0.125);
        assert_close(trim.width, 2.0);
        assert_close(trim.height, 3.5);
    }

    #[test]
    fn explicit_bleed_box_defines_artwork_bounds_inside_the_media_box() {
        let mut document = LoDocument::with_version("1.7");
        let page = document.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![
                Object::Integer(-18),
                Object::Integer(-18),
                180.into(),
                288.into(),
            ],
            "BleedBox" => vec![0.into(), 0.into(), 162.into(), 270.into()],
            "CropBox" => vec![9.into(), 9.into(), 153.into(), 261.into()],
            "TrimBox" => vec![9.into(), 9.into(), 153.into(), 261.into()],
        });

        let geometry = source_page_geometry(&document, page).unwrap();

        assert_close(geometry.size.width, 2.25);
        assert_close(geometry.size.height, 3.75);
        assert_close(geometry.crop_box.left, 0.125);
        assert_close(geometry.crop_box.bottom, 0.125);
        let media = geometry.media_box.unwrap();
        assert_close(media.left, -0.25);
        assert_close(media.bottom, -0.25);
        assert_close(media.right, 2.5);
        assert_close(media.top, 4.0);
    }

    #[test]
    fn manual_bleed_on_an_outer_only_page_infers_an_inset_finished_box() {
        let mut document = LoDocument::with_version("1.7");
        let page = document.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 144.into(), 252.into()],
            "CropBox" => vec![0.into(), 0.into(), 144.into(), 252.into()],
        });

        let geometry = source_page_geometry_with_bleed(&document, page, Some(0.125)).unwrap();

        assert_close(geometry.size.width, 2.0);
        assert_close(geometry.size.height, 3.5);
        let trim = geometry.trim_box.unwrap();
        assert_close(trim.width, 1.75);
        assert_close(trim.height, 3.25);
    }

    #[test]
    fn manual_bleed_on_an_outer_only_page_preserves_the_outer_artwork() {
        let mut document = LoDocument::with_version("1.7");
        let page = document.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 162.into(), 270.into()],
        });

        let geometry = source_page_geometry_with_bleed(&document, page, Some(0.125)).unwrap();

        assert_close(geometry.size.width, 2.25);
        assert_close(geometry.size.height, 3.75);
        let trim = geometry.trim_box.unwrap();
        assert_close(trim.left, 0.125);
        assert_close(trim.bottom, 0.125);
        assert_close(trim.width, 2.0);
        assert_close(trim.height, 3.5);
    }
}
