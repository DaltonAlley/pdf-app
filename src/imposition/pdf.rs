use lopdf::Document as LoDocument;
use pdfium_render::prelude::Pdfium;
use std::path::Path;

use super::{
    geometry::{source_page_geometry_with_bleed, SourcePageGeometry},
    model::{
        orientation_for, BleedDetection, GangUpPreset, PdfAnalysis, PdfBox, ProductionWarning,
        SizeInches,
    },
};
use crate::{
    adapters::UploadFile,
    documents as pdf_io,
    error::{AppError, AppResult},
    progress::{report, ProgressCallback},
    MAX_SOURCE_PDF_PAGES,
};

const SIZE_TOLERANCE: f64 = 0.01;
const COMMON_BLEEDS: [f64; 3] = [0.0625, 0.125, 0.25];

pub(super) fn page_metadata(
    document: &LoDocument,
    id: lopdf::ObjectId,
    geometry: &SourcePageGeometry,
) -> AppResult<super::model::SourcePage> {
    let page = document.get_dictionary(id).ok();
    Ok(super::model::SourcePage {
        source_pdf_size: geometry.size,
        source_trim_box: geometry.trim_box,
        preview_box: Some(super::geometry::transformed_box(
            geometry.page_box,
            geometry.normalization,
        )?),
        physical_size_assumed: page
            .and_then(|p| p.get(b"PdfToolsAssumedPhysicalSize").ok())
            .and_then(|v| v.as_bool().ok())
            .unwrap_or(false),
        filename: page
            .and_then(|p| p.get(b"PdfToolsSourceFilename").ok())
            .and_then(|v| v.as_str().ok())
            .map(|v| String::from_utf8_lossy(v).into_owned()),
        original_page_number: page
            .and_then(|p| p.get(b"PdfToolsSourcePage").ok())
            .and_then(|v| v.as_i64().ok())
            .and_then(|v| usize::try_from(v).ok()),
    })
}

/// Stage a render-only view with the same full artwork boundary as export.
/// The prepared source remains immutable. The existing renderer keeps its page,
/// pixel, duplicate-page, and cooperative deadline limits.
pub(crate) fn prepare_artwork_preview(
    path: &Path,
    staging_dir: &Path,
    page_numbers: &[usize],
    source_bleed_override: Option<f64>,
    max_bytes: Option<usize>,
) -> AppResult<crate::adapters::StagedImposeUpload> {
    if source_bleed_override
        .is_some_and(|amount| !amount.is_finite() || !(0.001..=1.0).contains(&amount))
    {
        return Err(AppError::bad_request(
            "source bleed override must be from 0.001 to 1 inch",
        ));
    }
    if page_numbers.is_empty() || page_numbers.len() > 4 {
        return Err(AppError::bad_request(
            "preview batches require from 1 to 4 pages",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    if page_numbers.iter().any(|n| *n == 0 || !seen.insert(*n)) {
        return Err(AppError::bad_request(
            "preview pages must be distinct positive page numbers",
        ));
    }
    let mut document = LoDocument::load(path)
        .map_err(|e| AppError::bad_request_cause("could not read preview source", e))?;
    let pages = document.get_pages();
    if pages.is_empty() || pages.len() > MAX_SOURCE_PDF_PAGES {
        return Err(AppError::bad_request("invalid source page count"));
    }
    for number in page_numbers {
        let key = u32::try_from(*number)
            .map_err(|_| AppError::bad_request("preview page is out of range"))?;
        let id = *pages
            .get(&key)
            .ok_or_else(|| AppError::bad_request("preview page is out of range"))?;
        let geometry = source_page_geometry_with_bleed(&document, id, source_bleed_override)?;
        let bounds = geometry
            .page_box
            .into_iter()
            .map(lopdf::Object::from)
            .collect::<Vec<_>>();
        let page = document
            .get_dictionary_mut(id)
            .map_err(|e| AppError::bad_request_cause("invalid preview page", e))?;
        page.set("MediaBox", bounds.clone());
        page.set("CropBox", bounds);
    }
    let mut bytes = Vec::new();
    document
        .save_to(&mut bytes)
        .map_err(|e| AppError::internal_cause("could not stage artwork preview", e))?;
    crate::adapters::validate_download_size(bytes.len(), max_bytes)?;
    crate::adapters::StagedImposeUpload::from_bytes(
        staging_dir,
        "artwork-preview.pdf".to_string(),
        &bytes,
    )
}
const COMMON_FINISHED_SIZES: [SizeInches; 5] = [
    SizeInches {
        width: 3.5,
        height: 2.0,
    },
    SizeInches {
        width: 4.0,
        height: 6.0,
    },
    SizeInches {
        width: 5.0,
        height: 7.0,
    },
    SizeInches {
        width: 5.5,
        height: 8.5,
    },
    SizeInches {
        width: 8.5,
        height: 11.0,
    },
];

pub(crate) fn analyze_pdf(
    pdfium: &Pdfium,
    file: UploadFile,
    presets: Vec<GangUpPreset>,
    progress: Option<ProgressCallback>,
) -> AppResult<PdfAnalysis> {
    report(&progress, 35, "Reading PDF structure")?;
    let UploadFile { filename, bytes } = file;
    let structural_document = LoDocument::load_mem(&bytes).map_err(|error| {
        AppError::bad_request_cause("could not inspect source PDF structure", error)
    })?;
    let renderable_page_count = pdf_io::page_count(pdf_io::load_pdf(pdfium, bytes)?.pages().len())?;
    analyze_loaded(
        filename,
        structural_document,
        Some(renderable_page_count),
        presets,
        progress,
    )
}

pub(crate) fn analyze_pdf_structural(
    file: UploadFile,
    presets: Vec<GangUpPreset>,
    progress: Option<ProgressCallback>,
) -> AppResult<PdfAnalysis> {
    report(&progress, 35, "Reading PDF structure")?;
    let UploadFile { filename, bytes } = file;
    let structural_document = LoDocument::load_mem(&bytes).map_err(|error| {
        AppError::bad_request_cause("could not inspect source PDF structure", error)
    })?;
    analyze_loaded(filename, structural_document, None, presets, progress)
}

pub(crate) fn analyze_pdf_path_structural(
    filename: String,
    path: &Path,
    presets: Vec<GangUpPreset>,
    progress: Option<ProgressCallback>,
) -> AppResult<PdfAnalysis> {
    report(&progress, 35, "Reading PDF structure")?;
    let structural_document = LoDocument::load(path).map_err(|error| {
        AppError::bad_request_cause("could not inspect source PDF structure", error)
    })?;
    analyze_loaded(filename, structural_document, None, presets, progress)
}

fn analyze_loaded(
    filename: String,
    structural_document: LoDocument,
    renderable_page_count: Option<usize>,
    presets: Vec<GangUpPreset>,
    progress: Option<ProgressCallback>,
) -> AppResult<PdfAnalysis> {
    let page_count = structural_document.get_pages().len();
    if renderable_page_count.is_some_and(|renderable| renderable != page_count) {
        return Err(AppError::bad_request(
            "source PDF page tree is inconsistent",
        ));
    }
    if page_count == 0 {
        return Err(AppError::bad_request("PDF has no pages to inspect"));
    }
    if page_count > MAX_SOURCE_PDF_PAGES {
        return Err(AppError::payload_too_large(format!(
            "gang-up PDFs are limited to {MAX_SOURCE_PDF_PAGES} pages"
        )));
    }

    report(
        &progress,
        inspection_percent(1, page_count),
        format!("Inspected page 1 of {page_count}"),
    )?;
    let page_ids = structural_document
        .get_pages()
        .into_values()
        .collect::<Vec<_>>();
    if page_ids.len() != page_count {
        return Err(AppError::bad_request(
            "source PDF page tree is inconsistent",
        ));
    }
    let first_page_id = page_ids
        .first()
        .copied()
        .ok_or_else(|| AppError::bad_request("PDF has no pages to inspect"))?;
    let first_geometry =
        source_page_geometry_with_bleed(&structural_document, first_page_id, None)?;
    let source_pdf_size = first_geometry.size;
    let mut source_pages = vec![page_metadata(
        &structural_document,
        first_page_id,
        &first_geometry,
    )?];
    for page_number in 2..=page_count {
        let page_id = page_ids
            .get(page_number - 1)
            .copied()
            .ok_or_else(|| AppError::bad_request("source PDF page tree is inconsistent"))?;
        let geometry = source_page_geometry_with_bleed(&structural_document, page_id, None)?;
        source_pages.push(page_metadata(&structural_document, page_id, &geometry)?);
        report(
            &progress,
            inspection_percent(page_number, page_count),
            format!("Inspected page {page_number} of {page_count}"),
        )?;
    }
    let media_box = first_geometry.media_box;
    let crop_box = Some(first_geometry.crop_box);
    let bleed_box = first_geometry.bleed_box;
    let trim_box = first_geometry.trim_box;
    report(&progress, 88, "Detecting trim size and bleed")?;
    let (matched_preset_id, suggested_finished_cut_size, likely_bleed) =
        detect_finished_size_and_bleed(source_pdf_size, trim_box.as_ref(), &presets);
    let mut warnings = Vec::new();

    if !likely_bleed.detected {
        warnings.push(ProductionWarning::new(
            "No bleed detected.",
            "Cuts may show white edges.",
            "Scale artwork, or continue only without edge-to-edge color.",
        ));
    }

    Ok(PdfAnalysis {
        source_pages,
        filename,
        page_count,
        source_pdf_size,
        orientation: orientation_for(source_pdf_size),
        media_box,
        crop_box,
        bleed_box,
        trim_box,
        likely_bleed,
        matched_preset_id,
        suggested_finished_cut_size,
        appears_duplex: page_count >= 2,
        orientation_adjusted_pages: 0,
        warnings,
    })
}

fn inspection_percent(completed: usize, total: usize) -> u8 {
    45 + ((completed * 40) / total.max(1)) as u8
}

fn detect_finished_size_and_bleed(
    source: SizeInches,
    trim_box: Option<&PdfBox>,
    presets: &[GangUpPreset],
) -> (Option<String>, Option<SizeInches>, BleedDetection) {
    if let Some(trim_box) = trim_box {
        let trim_size = SizeInches {
            width: round4(trim_box.width),
            height: round4(trim_box.height),
        };
        let matched = matching_preset(trim_size, presets);
        if (trim_size.width - source.width).abs() > SIZE_TOLERANCE
            || (trim_size.height - source.height).abs() > SIZE_TOLERANCE
        {
            let bleed = bleed_detection(source, trim_size);
            return (
                matched.map(|preset| preset.id.clone()),
                Some(trim_size),
                bleed,
            );
        }
        if let Some(preset) = matched {
            return (
                Some(preset.id.clone()),
                Some(trim_size),
                BleedDetection {
                    detected: false,
                    amount_per_side: 0.0,
                    horizontal: 0.0,
                    vertical: 0.0,
                    notes: vec!["Source PDF matches a known finished size.".to_string()],
                },
            );
        }
    }

    for preset in presets {
        for candidate in [
            preset.finished_cut_size,
            SizeInches {
                width: preset.finished_cut_size.height,
                height: preset.finished_cut_size.width,
            },
        ] {
            if same_size(source, candidate) {
                return (
                    Some(preset.id.clone()),
                    Some(candidate),
                    BleedDetection {
                        detected: false,
                        amount_per_side: 0.0,
                        horizontal: 0.0,
                        vertical: 0.0,
                        notes: vec!["Source PDF matches a known finished size.".to_string()],
                    },
                );
            }
            let bleed = bleed_detection(source, candidate);
            if bleed.detected {
                return (Some(preset.id.clone()), Some(candidate), bleed);
            }
        }
    }

    for finished_size in COMMON_FINISHED_SIZES {
        for candidate in [
            finished_size,
            SizeInches {
                width: finished_size.height,
                height: finished_size.width,
            },
        ] {
            if same_size(source, candidate) {
                return (
                    None,
                    Some(candidate),
                    BleedDetection {
                        detected: false,
                        amount_per_side: 0.0,
                        horizontal: 0.0,
                        vertical: 0.0,
                        notes: vec!["Source PDF matches a common finished size.".to_string()],
                    },
                );
            }
            let bleed = bleed_detection(source, candidate);
            if bleed.detected {
                return (None, Some(candidate), bleed);
            }
        }
    }

    (
        None,
        None,
        BleedDetection {
            detected: false,
            amount_per_side: 0.0,
            horizontal: 0.0,
            vertical: 0.0,
            notes: vec!["No trim box was available for bleed detection.".to_string()],
        },
    )
}

fn matching_preset(size: SizeInches, presets: &[GangUpPreset]) -> Option<&GangUpPreset> {
    presets.iter().find(|preset| {
        same_size(size, preset.finished_cut_size)
            || same_size(
                size,
                SizeInches {
                    width: preset.finished_cut_size.height,
                    height: preset.finished_cut_size.width,
                },
            )
    })
}

fn same_size(left: SizeInches, right: SizeInches) -> bool {
    (left.width - right.width).abs() <= SIZE_TOLERANCE
        && (left.height - right.height).abs() <= SIZE_TOLERANCE
}

fn bleed_detection(source: SizeInches, finished: SizeInches) -> BleedDetection {
    let horizontal = round4(((source.width - finished.width) / 2.0).max(0.0));
    let vertical = round4(((source.height - finished.height) / 2.0).max(0.0));
    let common = COMMON_BLEEDS
        .into_iter()
        .find(|bleed| {
            (horizontal - bleed).abs() <= SIZE_TOLERANCE
                && (vertical - bleed).abs() <= SIZE_TOLERANCE
        })
        .unwrap_or(0.0);

    if common > 0.0 {
        BleedDetection {
            detected: true,
            amount_per_side: common,
            horizontal,
            vertical,
            notes: vec![format!("Bleed likely included at {common:.4} in per side.")],
        }
    } else {
        BleedDetection {
            detected: false,
            amount_per_side: 0.0,
            horizontal,
            vertical,
            notes: vec!["No common bleed amount was detected.".to_string()],
        }
    }
}

fn round4(value: f64) -> f64 {
    (value * 10000.0).round() / 10000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::StagedImposeUpload;
    use crate::imposition::{normalize_staged_imposition_geometry, StagedArtworkPdf};
    use lopdf::{dictionary, Dictionary, Object};

    fn staged_pdf(filename: &str, pages: &[(f64, f64, i64)], unique: &str) -> StagedImposeUpload {
        let mut document = LoDocument::with_version("1.7");
        let pages_id = document.new_object_id();
        let page_ids = pages
            .iter()
            .map(|(width, height, rotation)| {
                document.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), (*width * 72.0).into(), (*height * 72.0).into()],
                    "Rotate" => *rotation,
                    "Resources" => Dictionary::new(),
                })
            })
            .collect::<Vec<_>>();
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
                "Count" => page_ids.len() as i64,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        let path = std::env::temp_dir().join(format!(
            "pdf-tools-geometry-{unique}-{}-{}.pdf",
            std::process::id(),
            filename.replace(['/', '\\'], "-")
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        document
            .save_to(&mut std::fs::File::create(&path).unwrap())
            .unwrap();
        StagedImposeUpload::for_test(filename, path)
    }

    fn unique_test_name() -> String {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_string()
    }

    #[test]
    fn staged_geometry_accepts_mixed_physical_page_sizes() {
        let unique = unique_test_name();
        let files = vec![
            StagedArtworkPdf::uploaded_pdf(staged_pdf(
                "imposed.pdf",
                &[(12.0, 18.0, 0); 3],
                &unique,
            )),
            StagedArtworkPdf::uploaded_pdf(staged_pdf(
                "converted.pdf",
                &[(2.75, 3.75, 0)],
                &unique,
            )),
        ];

        normalize_staged_imposition_geometry(&files).unwrap();
        for file in files {
            let file = file.into_upload();
            let analysis = analyze_pdf_path_structural(
                file.filename().to_string(),
                file.path(),
                Vec::new(),
                None,
            )
            .unwrap();
            assert_eq!(analysis.source_pages.len(), analysis.page_count);
        }
    }

    #[test]
    fn staged_geometry_accepts_rotation_normalized_equivalent_files() {
        let unique = unique_test_name();
        let files = vec![
            StagedArtworkPdf::uploaded_pdf(staged_pdf(
                "camera-export.pdf",
                &[(5.25, 7.25, 270)],
                &unique,
            )),
            StagedArtworkPdf::uploaded_pdf(staged_pdf(
                "layout-export.pdf",
                &[(7.25, 5.25, 0)],
                &unique,
            )),
        ];

        normalize_staged_imposition_geometry(&files).unwrap();
    }

    #[test]
    fn staged_geometry_preserves_mixed_page_orientation() {
        let unique = unique_test_name();
        let files = vec![StagedArtworkPdf::uploaded_pdf(staged_pdf(
            "mixed-orientation.pdf",
            &[(7.25, 5.25, 0), (5.25, 7.25, 0)],
            &unique,
        ))];

        normalize_staged_imposition_geometry(&files).unwrap();

        let upload = files.into_iter().next().unwrap().into_upload();
        let normalized = LoDocument::load(upload.path()).unwrap();
        let pages = normalized.get_pages().into_values().collect::<Vec<_>>();
        let first = source_page_geometry_with_bleed(&normalized, pages[0], None).unwrap();
        let second = source_page_geometry_with_bleed(&normalized, pages[1], None).unwrap();
        assert_eq!(
            first.size,
            SizeInches {
                width: 7.25,
                height: 5.25
            }
        );
        assert_eq!(
            second.size,
            SizeInches {
                width: 5.25,
                height: 7.25
            }
        );
    }

    #[test]
    fn detects_common_bleed_in_landscape_orientation() {
        let source = SizeInches {
            width: 6.25,
            height: 4.25,
        };
        let preset = GangUpPreset {
            imposition_mode: Default::default(),
            finished_size_mode: Default::default(),
            artwork_fit: None,
            source_bleed_override: None,
            id: "saved-postcard".to_string(),
            name: "Saved postcard".to_string(),
            finished_cut_size: SizeInches {
                width: 4.0,
                height: 6.0,
            },
            parent_sheet_size: SizeInches {
                width: 12.0,
                height: 18.0,
            },
            bleed_handling: super::super::model::BleedOption::UseAsIs,
            created_bleed_amount: 0.125,
            gutter: super::super::model::GuttersInches {
                horizontal: 0.299,
                vertical: 0.299,
            },
            orientation_preference: super::super::model::OrientationPreference::Auto,
            sides: super::super::model::Sides::Single,
            layout_preference: super::super::model::LayoutMode::Auto,
            manual: None,
            duplex: None,
            output_preference: super::super::model::OutputPreference::CleanPdf,
            built_in: false,
        };
        let (_, finished, bleed) = detect_finished_size_and_bleed(source, None, &[preset]);

        assert_eq!(
            finished,
            Some(SizeInches {
                width: 6.0,
                height: 4.0,
            })
        );
        assert!(bleed.detected);
        assert_eq!(bleed.amount_per_side, 0.125);
    }
}
