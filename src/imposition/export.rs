use std::{collections::HashMap, fs::OpenOptions, path::Path};

use lopdf::{
    content::{Content, Operation},
    dictionary, Dictionary, Document as LoDocument, Object, ObjectId, Stream,
};
use pdfium_render::prelude::Pdfium;

use super::{
    geometry::{inherited_page_attribute, source_page_geometry_with_bleed, SourcePageGeometry},
    layout as gang_up_layout,
    model::{BleedSource, ImpositionMode, LayoutRequest, LayoutResult, PiecePlacement, SizeInches},
};
use crate::{
    adapters::UploadFile,
    documents::{self as pdf_io, decode_page_content_stream},
    error::{AppError, AppResult},
    progress::{report, ProgressCallback},
    MAX_SOURCE_PDF_PAGES,
};

const PT_PER_IN: f64 = 72.0;
const PAGE_SIZE_TOLERANCE_IN: f64 = 0.01;
const MAX_COMBINED_PAGE_CONTENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_EXPORT_DECODED_CONTENT_BYTES: usize = 64 * 1024 * 1024;

pub(crate) fn flatten_annotations_for_export(
    pdfium: &Pdfium,
    file: UploadFile,
    progress: Option<ProgressCallback>,
) -> AppResult<UploadFile> {
    report(&progress, 28, "Opening source artwork")?;
    let document = pdf_io::load_pdf(pdfium, file.bytes.clone())?;
    let page_count = usize::try_from(document.pages().len())
        .map_err(|_| AppError::Internal("invalid PDF page count".to_string()))?;
    validate_flatten_page_count(page_count)?;
    let mut has_annotations = false;
    for page_index in document.pages().as_range() {
        let page_number = usize::try_from(page_index)
            .map_err(|_| AppError::Internal("invalid PDF page index".to_string()))?
            + 1;
        let page = document.pages().get(page_index).map_err(|error| {
            AppError::bad_request(format!(
                "could not inspect PDF page {page_number} annotations: {error}"
            ))
        })?;
        if !page.annotations().is_empty() {
            has_annotations = true;
            break;
        }
        report(
            &progress,
            28 + ((page_number * 8) / page_count.max(1)) as u8,
            format!("Inspected annotations on page {page_number} of {page_count}"),
        )?;
    }
    if !has_annotations {
        report(&progress, 36, "Source artwork is ready")?;
        return Ok(file);
    }

    let filename = file.filename;
    drop(file.bytes);
    for page_index in document.pages().as_range() {
        let page_number = page_index + 1;
        let mut page = document.pages().get(page_index).map_err(|error| {
            AppError::bad_request(format!(
                "could not inspect PDF page {page_number} annotations: {error}"
            ))
        })?;
        page.flatten().map_err(|error| {
            AppError::bad_request(format!(
                "could not flatten printable annotations on PDF page {page_number}: {error}"
            ))
        })?;
        let completed = usize::try_from(page_index)
            .map_err(|_| AppError::Internal("invalid PDF page index".to_string()))?
            + 1;
        report(
            &progress,
            30 + ((completed * 10) / page_count.max(1)) as u8,
            format!("Flattened annotations on page {completed} of {page_count}"),
        )?;
    }
    let bytes = document.save_to_bytes().map_err(|error| {
        AppError::internal_cause(
            "could not save PDF after flattening printable annotations",
            error,
        )
    })?;
    Ok(UploadFile {
        filename,
        bytes: bytes.into(),
    })
}

pub(crate) fn flatten_annotations_path(
    pdfium: &Pdfium,
    path: &Path,
    progress: Option<ProgressCallback>,
) -> AppResult<()> {
    report(&progress, 28, "Opening source artwork")?;
    let document = pdf_io::load_pdf_path(pdfium, path)?;
    let page_count = pdf_io::page_count(document.pages().len())?;
    validate_flatten_page_count(page_count)?;
    let mut has_annotations = false;
    for page_index in document.pages().as_range() {
        let page = document.pages().get(page_index).map_err(|error| {
            AppError::bad_request(format!("could not inspect PDF annotations: {error}"))
        })?;
        has_annotations |= !page.annotations().is_empty();
        let completed = usize::try_from(page_index)
            .map_err(|_| AppError::Internal("invalid PDF page index".to_string()))?
            + 1;
        report(
            &progress,
            28 + ((completed * 8) / page_count.max(1)) as u8,
            format!("Inspected annotations on page {completed} of {page_count}"),
        )?;
    }
    if !has_annotations {
        return Ok(());
    }
    for page_index in document.pages().as_range() {
        let mut page = document.pages().get(page_index).map_err(|error| {
            AppError::bad_request(format!("could not inspect PDF annotations: {error}"))
        })?;
        page.flatten().map_err(|error| {
            AppError::bad_request(format!("could not flatten printable annotations: {error}"))
        })?;
        let completed = usize::try_from(page_index)
            .map_err(|_| AppError::Internal("invalid PDF page index".to_string()))?
            + 1;
        report(
            &progress,
            30 + ((completed * 10) / page_count.max(1)) as u8,
            format!("Flattened annotations on page {completed} of {page_count}"),
        )?;
    }
    let temporary = path.with_extension("flattening.tmp");
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| AppError::internal_cause("could not create flattened PDF", error))?;
    if let Err(error) = document.save_to_writer(&mut output) {
        drop(output);
        let _ = std::fs::remove_file(&temporary);
        return Err(AppError::internal_cause(
            "could not save flattened PDF",
            error,
        ));
    }
    if let Err(error) = output.sync_all() {
        drop(output);
        let _ = std::fs::remove_file(&temporary);
        return Err(AppError::internal_cause(
            "could not flush flattened PDF",
            error,
        ));
    }
    drop(output);
    drop(document);
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(AppError::internal_cause(
            "could not commit flattened PDF",
            error,
        ));
    }
    report(&progress, 36, format!("Prepared {page_count} source pages"))?;
    Ok(())
}

fn validate_flatten_page_count(page_count: usize) -> AppResult<()> {
    if page_count > MAX_SOURCE_PDF_PAGES {
        return Err(AppError::payload_too_large(format!(
            "source PDFs are limited to {MAX_SOURCE_PDF_PAGES} pages"
        )));
    }
    Ok(())
}

pub(crate) fn export_clean_imposed_pdf(
    file: UploadFile,
    request: LayoutRequest,
    progress: Option<ProgressCallback>,
) -> AppResult<ExportArtifact> {
    report(&progress, 42, "Reading source artwork")?;
    let mut document = LoDocument::load_mem(&file.bytes).map_err(|err| {
        AppError::bad_request(format!("could not read PDF for imposition: {err}"))
    })?;
    drop(file);
    export_loaded_document(&mut document, request, progress)
}

fn export_loaded_document(
    document: &mut LoDocument,
    mut request: LayoutRequest,
    progress: Option<ProgressCallback>,
) -> AppResult<ExportArtifact> {
    let source_page_ids = document.get_pages().into_values().collect::<Vec<_>>();
    let page_count = source_page_ids.len();
    if page_count == 0 {
        return Err(AppError::bad_request("cannot export a PDF with no pages"));
    }
    if page_count > MAX_SOURCE_PDF_PAGES {
        return Err(AppError::payload_too_large(format!(
            "gang-up PDFs are limited to {MAX_SOURCE_PDF_PAGES} pages"
        )));
    }

    resolve_request_source(document, &mut request)?;
    report(&progress, 50, "Calculating imposed layout")?;
    let layout = gang_up_layout::generate_layout(request.clone())?;
    validate_source_page_count(page_count, &layout)?;

    let bytes = compose_shared_resource_imposition(document, &source_page_ids, &layout, progress)?;

    Ok(ExportArtifact {
        bytes,
        layout,
        canonical_request: request,
    })
}

pub(crate) fn export_clean_imposed_pdf_path(
    path: &Path,
    request: LayoutRequest,
    progress: Option<ProgressCallback>,
) -> AppResult<ExportArtifact> {
    report(&progress, 42, "Reading source artwork")?;
    let mut document = LoDocument::load(path).map_err(|err| {
        AppError::bad_request(format!("could not read PDF for imposition: {err}"))
    })?;
    export_loaded_document(&mut document, request, progress)
}

pub(crate) fn resolve_request_source_path(
    path: &Path,
    request: &mut LayoutRequest,
) -> AppResult<()> {
    let document = LoDocument::load(path)
        .map_err(|e| AppError::bad_request_cause("could not read source artwork", e))?;
    resolve_request_source(&document, request)
}

fn resolve_request_source(document: &LoDocument, request: &mut LayoutRequest) -> AppResult<()> {
    let pages = document.get_pages();
    if pages.is_empty() || pages.len() > MAX_SOURCE_PDF_PAGES {
        return Err(AppError::bad_request("invalid source page count"));
    }
    if request
        .source_bleed_override
        .is_some_and(|b| !b.is_finite() || !(0.001..=1.0).contains(&b))
    {
        return Err(AppError::bad_request(
            "source bleed override must be from 0.001 to 1 inch",
        ));
    }
    let mut source_pages = Vec::with_capacity(pages.len());
    let mut first = None;
    for id in pages.into_values() {
        let geometry =
            source_page_geometry_with_bleed(document, id, request.source_bleed_override)?;
        source_pages.push(super::pdf::page_metadata(document, id, &geometry)?);
        if first.is_none() {
            first = Some(geometry);
        }
    }
    let first = first.ok_or_else(|| AppError::bad_request("source PDF has no pages"))?;
    request.source_pages = source_pages;
    request.source_pdf_size = first.size;
    request.source_trim_box = first.trim_box;
    request.source_page_count = Some(request.source_pages.len());
    if !super::mixed::enabled(request) {
        validate_resolved_source_bleed(request, &first)?;
    }
    Ok(())
}

fn validate_resolved_source_bleed(
    request: &LayoutRequest,
    geometry: &SourcePageGeometry,
) -> AppResult<()> {
    let Some(amount) = request.source_bleed_override else {
        return Ok(());
    };
    let expected_width = request.finished_cut_size.width + amount * 2.0;
    let expected_height = request.finished_cut_size.height + amount * 2.0;
    if (geometry.size.width - expected_width).abs() > PAGE_SIZE_TOLERANCE_IN
        || (geometry.size.height - expected_height).abs() > PAGE_SIZE_TOLERANCE_IN
    {
        return Err(AppError::bad_request(format!(
            "source PDF does not contain {amount:.4} in of artwork outside every finished edge"
        )));
    }
    Ok(())
}

pub(crate) struct ExportArtifact {
    pub(crate) bytes: Vec<u8>,
    pub(crate) layout: LayoutResult,
    pub(crate) canonical_request: LayoutRequest,
}

#[derive(Debug, Clone, Copy)]
struct SourceForm {
    object_id: ObjectId,
    size: SizeInches,
}

fn compose_shared_resource_imposition(
    document: &mut LoDocument,
    source_page_ids: &[ObjectId],
    layout: &LayoutResult,
    progress: Option<ProgressCallback>,
) -> AppResult<Vec<u8>> {
    if source_page_ids.is_empty() {
        return Err(AppError::bad_request("cannot export a PDF with no pages"));
    }

    let pages_id = document
        .catalog()
        .and_then(|catalog| catalog.get(b"Pages"))
        .and_then(Object::as_reference)
        .map_err(|err| AppError::internal_cause("could not find source PDF page tree", err))?;

    let total_output_pages = output_page_count(layout.sheets_required, layout.duplex.is_some())?;
    let mut completed_output_pages = 0usize;
    let mut output_page_ids = Vec::with_capacity(total_output_pages);
    let mut forms = HashMap::new();
    let mut decoded_content_budget = MAX_EXPORT_DECODED_CONTENT_BYTES;
    for sheet_index in 0..layout.sheets_required {
        output_page_ids.push(add_shared_resource_sheet(
            document,
            pages_id,
            layout,
            source_page_ids,
            &mut forms,
            &mut decoded_content_budget,
            sheet_index,
            SheetSide::Front,
        )?);
        completed_output_pages += 1;
        report(
            &progress,
            output_page_percent(completed_output_pages, total_output_pages),
            format!("Built imposed page {completed_output_pages} of {total_output_pages}"),
        )?;
        if layout.duplex.is_some() {
            output_page_ids.push(add_shared_resource_sheet(
                document,
                pages_id,
                layout,
                source_page_ids,
                &mut forms,
                &mut decoded_content_budget,
                sheet_index,
                SheetSide::Back,
            )?);
            completed_output_pages += 1;
            report(
                &progress,
                output_page_percent(completed_output_pages, total_output_pages),
                format!("Built imposed page {completed_output_pages} of {total_output_pages}"),
            )?;
        }
    }

    let pages = document
        .get_object_mut(pages_id)
        .and_then(Object::as_dict_mut)
        .map_err(|err| AppError::internal_cause("could not update imposed PDF page tree", err))?;
    pages.set(
        "Kids",
        Object::Array(
            output_page_ids
                .iter()
                .copied()
                .map(Object::Reference)
                .collect(),
        ),
    );
    pages.set("Count", output_page_ids.len() as i64);

    let retained_catalog_entries = document
        .catalog()
        .map_err(|err| AppError::internal_cause("could not inspect imposed PDF catalog", err))?
        .iter()
        .filter(|(key, _)| {
            matches!(
                key.as_slice(),
                b"Metadata" | b"OCProperties" | b"OutputIntents"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    let catalog = document
        .catalog_mut()
        .map_err(|err| AppError::internal_cause("could not sanitize imposed PDF catalog", err))?;
    *catalog = dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    };
    for (key, value) in retained_catalog_entries {
        catalog.set(key, value);
    }
    document.trailer.remove(b"Info");
    document.trailer.remove(b"Encrypt");

    document.prune_objects();
    document.change_producer("PDF Tools shared-resource imposition");

    report(&progress, 90, "Writing imposed PDF")?;
    let mut bytes = Vec::with_capacity(pdf_io::estimated_lopdf_output_capacity(document));
    document
        .save_to(&mut bytes)
        .map_err(|err| AppError::internal_cause("could not save imposed PDF", err))?;
    Ok(bytes)
}

fn output_page_count(sheets_required: usize, duplex: bool) -> AppResult<usize> {
    let sides_per_sheet = if duplex { 2 } else { 1 };
    sheets_required.checked_mul(sides_per_sheet).ok_or_else(|| {
        AppError::payload_too_large("imposed PDF would contain too many output pages")
    })
}

fn output_page_percent(completed: usize, total: usize) -> u8 {
    55 + ((completed * 33) / total.max(1)) as u8
}

fn create_source_form(
    document: &mut LoDocument,
    page_id: ObjectId,
    decoded_content_budget: &mut usize,
    source_bleed_override: Option<f64>,
) -> AppResult<SourceForm> {
    let resources = inherited_page_attribute(document, page_id, b"Resources")?
        .unwrap_or_else(|| Object::Dictionary(Dictionary::new()));
    let geometry = source_page_geometry_with_bleed(document, page_id, source_bleed_override)?;
    let [left, bottom, right, top] = geometry.page_box;

    let mut form_stream = if let Some(mut stream) = single_page_content_stream(document, page_id) {
        account_decoded_stream(&stream, decoded_content_budget)?;
        stream.start_position = None;
        stream
    } else {
        let content = combined_page_content(document, page_id, decoded_content_budget)?;
        Stream::new(Dictionary::new(), content)
    };
    form_stream.dict.set("Type", "XObject");
    form_stream.dict.set("Subtype", "Form");
    form_stream.dict.set("FormType", 1);
    form_stream.dict.set(
        "BBox",
        vec![left.into(), bottom.into(), right.into(), top.into()],
    );
    form_stream.dict.set(
        "Matrix",
        geometry
            .normalization
            .into_iter()
            .map(Object::from)
            .collect::<Vec<_>>(),
    );
    form_stream.dict.set("Resources", resources);
    if let Ok(group) = document
        .get_dictionary(page_id)
        .and_then(|page| page.get(b"Group"))
    {
        form_stream.dict.set("Group", group.clone());
    }
    form_stream
        .compress()
        .map_err(|err| AppError::internal_cause("could not compress source page form", err))?;
    let object_id = document.add_object(form_stream);

    Ok(SourceForm {
        object_id,
        size: geometry.size,
    })
}

fn account_decoded_stream(stream: &Stream, decoded_content_budget: &mut usize) -> AppResult<()> {
    let limit = (*decoded_content_budget).min(MAX_COMBINED_PAGE_CONTENT_BYTES);
    let decoded = decode_page_content_stream(stream, limit)?;
    *decoded_content_budget = decoded_content_budget
        .checked_sub(decoded.len())
        .ok_or_else(|| {
            AppError::payload_too_large("source PDF decoded page content exceeds the export limit")
        })?;
    Ok(())
}

fn single_page_content_stream(document: &LoDocument, page_id: ObjectId) -> Option<Stream> {
    let content_ids = document.get_page_contents(page_id);
    let content_id = *content_ids.first().filter(|_| content_ids.len() == 1)?;
    document
        .get_object(content_id)
        .ok()?
        .as_stream()
        .ok()
        .cloned()
}

fn combined_page_content(
    document: &LoDocument,
    page_id: ObjectId,
    decoded_content_budget: &mut usize,
) -> AppResult<Vec<u8>> {
    let mut content = Vec::new();
    for content_id in document.get_page_contents(page_id) {
        let stream = document
            .get_object(content_id)
            .and_then(Object::as_stream)
            .map_err(|err| {
                AppError::bad_request(format!("source page content is not a valid stream: {err}"))
            })?;
        let remaining = MAX_COMBINED_PAGE_CONTENT_BYTES
            .checked_sub(content.len().saturating_add(1))
            .ok_or_else(|| AppError::payload_too_large("source page content is too large"))?;
        let decoded = decode_page_content_stream(stream, remaining)?;
        *decoded_content_budget = decoded_content_budget
            .checked_sub(decoded.len())
            .ok_or_else(|| {
                AppError::payload_too_large(
                    "source PDF decoded page content exceeds the export limit",
                )
            })?;
        content.extend_from_slice(&decoded);
        content.push(b'\n');
    }
    Ok(content)
}

#[allow(clippy::too_many_arguments)]
fn add_shared_resource_sheet(
    document: &mut LoDocument,
    pages_id: ObjectId,
    layout: &LayoutResult,
    source_page_ids: &[ObjectId],
    forms: &mut HashMap<usize, SourceForm>,
    decoded_content_budget: &mut usize,
    sheet_index: usize,
    side: SheetSide,
) -> AppResult<ObjectId> {
    let mut xobjects = Dictionary::new();
    let mut operations = Vec::with_capacity(layout.placements.len() * 7);

    for placement in &layout.placements {
        let Some(page_number) = source_page_number(layout, sheet_index, placement.index, side)
        else {
            continue;
        };
        let source_index = page_number - 1;
        let Some(page_id) = source_page_ids.get(source_index).copied() else {
            continue;
        };
        let form = if let Some(form) = forms.get(&source_index) {
            *form
        } else {
            let source_bleed_override = layout.source_bleed_override.or_else(|| {
                (layout.bleed.source == BleedSource::Manual)
                    .then_some(layout.bleed.effective_amount_per_side)
            });
            let form = create_source_form(
                document,
                page_id,
                decoded_content_budget,
                source_bleed_override,
            )?;
            forms.insert(source_index, form);
            form
        };
        let name = format!("Source{}", source_index + 1).into_bytes();
        if !xobjects.has(&name) {
            xobjects.set(name.clone(), Object::Reference(form.object_id));
        }
        let (resolved, clip) = if let Some(plan) = layout.page_plans.get(source_index) {
            resolve_page_plan(layout, plan, placement, side)
        } else {
            (
                resolve_placement(layout, placement, side),
                placement_clip_rect(layout, placement, side),
            )
        };
        let rotate_back_180 = side == SheetSide::Back
            && layout
                .duplex
                .as_ref()
                .is_some_and(|duplex| duplex.rotate_back_180);
        let matrix = placement_matrix(
            resolved,
            form.size,
            layout.rotation_degrees,
            rotate_back_180,
        );
        operations.push(Operation::new("q", Vec::new()));
        operations.push(Operation::new(
            "re",
            [clip.x, clip.y, clip.width, clip.height]
                .into_iter()
                .map(Object::from)
                .collect(),
        ));
        operations.push(Operation::new("W", Vec::new()));
        operations.push(Operation::new("n", Vec::new()));
        operations.push(Operation::new(
            "cm",
            matrix.into_iter().map(Object::from).collect(),
        ));
        operations.push(Operation::new("Do", vec![Object::Name(name)]));
        operations.push(Operation::new("Q", Vec::new()));
    }

    let content = Content { operations }
        .encode()
        .map_err(|err| AppError::internal_cause("could not encode imposed sheet content", err))?;
    let mut content_stream = Stream::new(Dictionary::new(), content);
    content_stream
        .compress()
        .map_err(|err| AppError::internal_cause("could not compress imposed sheet content", err))?;
    let content_id = document.add_object(content_stream);
    let resources = dictionary! { "XObject" => xobjects };
    let media_box = vec![
        0.into(),
        0.into(),
        inches_to_points(layout.parent_sheet_size.width).into(),
        inches_to_points(layout.parent_sheet_size.height).into(),
    ];
    let page = dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => media_box.clone(),
        "CropBox" => media_box,
        "Rotate" => 0,
        "Resources" => resources,
        "Contents" => content_id,
    };
    Ok(document.add_object(page))
}

fn validate_source_page_count(page_count: usize, layout: &LayoutResult) -> AppResult<()> {
    if page_count == 0 {
        return Err(AppError::bad_request("cannot export a PDF with no pages"));
    }
    if layout.duplex.is_some() {
        match layout.imposition_mode {
            ImpositionMode::Repeat if page_count < 2 => {
                return Err(AppError::bad_request(
                    "double-sided gang-up export requires a source PDF with at least two pages",
                ));
            }
            ImpositionMode::Repeat
                if layout.impression_quantities.is_some() && !page_count.is_multiple_of(2) =>
            {
                return Err(AppError::bad_request(
                    "double-sided repeat imposition requires an even source page count",
                ));
            }
            ImpositionMode::Unique if page_count < 2 => {
                return Err(AppError::bad_request(
                    "double-sided unique imposition requires at least two source pages",
                ));
            }
            ImpositionMode::Unique if !page_count.is_multiple_of(2) => {
                return Err(AppError::bad_request(
                    "double-sided unique imposition requires an even source page count",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SheetSide {
    Front,
    Back,
}

fn source_page_number(
    layout: &LayoutResult,
    sheet_index: usize,
    placement_index: usize,
    side: SheetSide,
) -> Option<usize> {
    let impression_index = sheet_index * layout.pieces_per_sheet + placement_index;
    let source_impression_index = match (&layout.imposition_mode, &layout.impression_quantities) {
        (ImpositionMode::Repeat, None) => 0,
        (ImpositionMode::Repeat, Some(quantities)) => {
            if impression_index >= layout.impressions_requested {
                return None;
            }
            let mut remaining = impression_index;
            quantities.iter().position(|quantity| {
                if remaining < *quantity {
                    true
                } else {
                    remaining -= *quantity;
                    false
                }
            })?
        }
        (ImpositionMode::Unique, _) => {
            if impression_index >= layout.impressions_requested {
                return None;
            }
            impression_index
        }
    };

    let zero_based_page = if layout.duplex.is_some() {
        match side {
            SheetSide::Front => source_impression_index * 2,
            SheetSide::Back => source_impression_index * 2 + 1,
        }
    } else {
        source_impression_index
    };

    Some(zero_based_page + 1)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ResolvedPlacement {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn resolve_page_plan(
    layout: &LayoutResult,
    plan: &super::model::PagePlan,
    placement: &PiecePlacement,
    side: SheetSide,
) -> (ResolvedPlacement, ResolvedPlacement) {
    let (artwork, clip) = super::mixed::placed_rects(plan, placement, layout.rotation_degrees);
    let rect = |r: super::model::PlanRect| ResolvedPlacement {
        x: r.x,
        y: r.y,
        width: r.width,
        height: r.height,
    };
    let mapped_clip = resolve_side_placement(layout, rect(clip), side);
    let mut dx = artwork.x - clip.x;
    let mut dy = artwork.y - clip.y;
    if side == SheetSide::Back && layout.duplex.as_ref().is_some_and(|d| d.rotate_back_180) {
        dx = clip.width - dx - artwork.width;
        dy = clip.height - dy - artwork.height;
    }
    let mapped_artwork = ResolvedPlacement {
        x: mapped_clip.x + dx,
        y: mapped_clip.y + dy,
        width: artwork.width,
        height: artwork.height,
    };
    (
        placement_to_points(layout.parent_sheet_size, mapped_artwork),
        placement_to_points(layout.parent_sheet_size, mapped_clip),
    )
}

fn resolve_placement(
    layout: &LayoutResult,
    placement: &PiecePlacement,
    side: SheetSide,
) -> ResolvedPlacement {
    let artwork = gang_up_layout::resolved_artwork_rect(layout, placement);
    let resolved = ResolvedPlacement {
        x: artwork.x,
        y: artwork.y,
        width: artwork.width,
        height: artwork.height,
    };

    placement_to_points(
        layout.parent_sheet_size,
        resolve_side_placement(layout, resolved, side),
    )
}

fn placement_clip_rect(
    layout: &LayoutResult,
    placement: &PiecePlacement,
    side: SheetSide,
) -> ResolvedPlacement {
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
    let clip = ResolvedPlacement {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    };
    placement_to_points(
        layout.parent_sheet_size,
        resolve_side_placement(layout, clip, side),
    )
}

fn resolve_side_placement(
    layout: &LayoutResult,
    placement: ResolvedPlacement,
    side: SheetSide,
) -> ResolvedPlacement {
    let Some(duplex) = &layout.duplex else {
        return placement;
    };
    if side == SheetSide::Front {
        return placement;
    }

    let landscape = layout.parent_sheet_size.width > layout.parent_sheet_size.height;
    let base_mirror_x = match duplex.flip_edge {
        super::model::DuplexFlipEdge::LongEdge => !landscape,
        super::model::DuplexFlipEdge::ShortEdge => landscape,
    };
    let mirror_x = base_mirror_x != duplex.rotate_back_180;
    let mirror_y = base_mirror_x == duplex.rotate_back_180;

    ResolvedPlacement {
        x: if mirror_x {
            layout.parent_sheet_size.width - placement.x - placement.width
        } else {
            placement.x
        },
        y: if mirror_y {
            layout.parent_sheet_size.height - placement.y - placement.height
        } else {
            placement.y
        },
        width: placement.width,
        height: placement.height,
    }
}

fn placement_to_points(parent: SizeInches, placement: ResolvedPlacement) -> ResolvedPlacement {
    ResolvedPlacement {
        x: inches_to_points(placement.x),
        y: inches_to_points(parent.height - placement.y - placement.height),
        width: inches_to_points(placement.width),
        height: inches_to_points(placement.height),
    }
}

fn placement_matrix(
    placement: ResolvedPlacement,
    source_size: SizeInches,
    rotation_degrees: u16,
    rotate_180: bool,
) -> [f64; 6] {
    if rotation_degrees == 90 && rotate_180 {
        let scale_x = placement.height / inches_to_points(source_size.width);
        let scale_y = placement.width / inches_to_points(source_size.height);
        [
            0.0,
            scale_x,
            -scale_y,
            0.0,
            placement.x + placement.width,
            placement.y,
        ]
    } else if rotation_degrees == 90 {
        let scale_x = placement.height / inches_to_points(source_size.width);
        let scale_y = placement.width / inches_to_points(source_size.height);
        [
            0.0,
            -scale_x,
            scale_y,
            0.0,
            placement.x,
            placement.y + placement.height,
        ]
    } else if rotate_180 {
        [
            -placement.width / inches_to_points(source_size.width),
            0.0,
            0.0,
            -placement.height / inches_to_points(source_size.height),
            placement.x + placement.width,
            placement.y + placement.height,
        ]
    } else {
        [
            placement.width / inches_to_points(source_size.width),
            0.0,
            0.0,
            placement.height / inches_to_points(source_size.height),
            placement.x,
            placement.y,
        ]
    }
}

fn inches_to_points(inches: f64) -> f64 {
    inches * PT_PER_IN
}

#[cfg(test)]
mod tests {
    use super::super::geometry::source_page_geometry;
    use super::super::model::{
        BleedOption, BleedSettings, BleedSource, DuplexFlipEdge, DuplexSettings, GuttersInches,
        ImpositionMode, MarginsInches, ProductionWarning,
    };
    use super::*;
    use lopdf::dictionary;

    fn size(width: f64, height: f64) -> SizeInches {
        SizeInches { width, height }
    }

    fn placement() -> PiecePlacement {
        PiecePlacement {
            index: 0,
            row: 0,
            column: 0,
            x: 0.875,
            y: 0.875,
            width: 3.75,
            height: 2.25,
            finished_x: 1.0,
            finished_y: 1.0,
            finished_width: 3.5,
            finished_height: 2.0,
        }
    }

    fn layout(option: BleedOption, detected_amount_per_side: f64) -> LayoutResult {
        LayoutResult {
            source_bleed_override: None,
            page_plans: Vec::new(),
            source_pdf_size: size(3.75, 2.25),
            source_trim_box: None,
            source_page_count: None,
            finished_cut_size: size(3.5, 2.0),
            parent_sheet_size: size(12.0, 18.0),
            quantity_requested: 1,
            imposition_mode: ImpositionMode::Repeat,
            impression_quantities: None,
            orientation_preference: super::super::model::OrientationPreference::Auto,
            impressions_requested: 1,
            pieces_per_sheet: 1,
            sheets_required: 1,
            total_pieces_produced: 1,
            extra_pieces_produced: 0,
            unused_positions: 0,
            waste_percent: 0.0,
            rotation_degrees: 0,
            rows: 1,
            columns: 1,
            margins: MarginsInches {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: 0.0,
            },
            gutters: GuttersInches {
                horizontal: 0.0,
                vertical: 0.0,
            },
            placements: vec![placement()],
            duplex: None::<DuplexSettings>,
            bleed: BleedSettings {
                option,
                detected_amount_per_side,
                effective_amount_per_side: detected_amount_per_side,
                source: if detected_amount_per_side > 0.0 {
                    BleedSource::Detected
                } else {
                    BleedSource::None
                },
                source_larger_than_cut: true,
            },
            created_bleed_amount: gang_up_layout::DEFAULT_CREATED_BLEED_IN,
            warnings: Vec::<ProductionWarning>::new(),
        }
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 0.001,
            "expected {expected}, got {actual}"
        );
    }

    fn transformed_point(matrix: [f64; 6], x: f64, y: f64) -> (f64, f64) {
        (
            matrix[0] * x + matrix[2] * y + matrix[4],
            matrix[1] * x + matrix[3] * y + matrix[5],
        )
    }

    #[test]
    fn stretch_export_matches_preview_plan_with_rotation_bleed_and_duplex() {
        for orientation in ["upright", "quarterTurn"] {
            for (bleed, source_bleed) in [
                ("useAsIs", None),
                ("scaleToBleed", None),
                ("fitInside", None),
                ("useAsIs", Some(0.125)),
            ] {
                for rotate_back in [false, true] {
                    let mut source = LoDocument::with_version("1.7");
                    let pages_id = source.new_object_id();
                    let mut page_ids = Vec::new();
                    for (width, height) in [(20.0, 2.0), (2.0, 20.0)] {
                        let content = source.add_object(Stream::new(
                            Dictionary::new(),
                            b"1 0 0 rg 0 0 72 72 re f".to_vec(),
                        ));
                        page_ids.push(source.add_object(dictionary! {
                            "Type" => "Page", "Parent" => pages_id,
                            "MediaBox" => vec![0.into(), 0.into(), (width * 72.0).into(), (height * 72.0).into()],
                            "Resources" => Dictionary::new(), "Contents" => content,
                        }));
                    }
                    source.objects.insert(pages_id, Object::Dictionary(dictionary! {
                        "Type" => "Pages", "Count" => 2,
                        "Kids" => page_ids.into_iter().map(Object::Reference).collect::<Vec<_>>(),
                    }));
                    let catalog =
                        source.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
                    source.trailer.set("Root", catalog);
                    let request: LayoutRequest = serde_json::from_value(serde_json::json!({
                        "sourcePdfSize":{"width":20.0,"height":2.0}, "sourcePageCount":2,
                        "finishedCutSize":{"width":8.5,"height":11.0},
                        "parentSheetSize":{"width":12.0,"height":18.0},
                        "quantityRequested":1,"impositionMode":"unique","orientationPreference":orientation,
                        "sides":"double", "duplex":{"flipEdge":"longEdge","rotateBack180":rotate_back,"backAlignment":""},
                        "layoutMode":"auto","bleedOption":bleed,"createdBleedAmount":0.125,
                        "sourceBleedOverride":source_bleed,
                        "gutter":{"horizontal":0.0,"vertical":0.0},"manual":null,
                        "artworkFit":{"mode":"stretch","position":{"x":0.0,"y":1.0}}
                    })).unwrap();
                    let mut preview_request = request.clone();
                    resolve_request_source(&source, &mut preview_request).unwrap();
                    let preview = gang_up_layout::generate_layout(preview_request).unwrap();
                    if source_bleed.is_some() {
                        let plan = &preview.page_plans[0];
                        assert_close(plan.artwork.width, 20.0 * 8.5 / 19.75);
                        assert_close(plan.artwork.height, 2.0 * 11.0 / 1.75);
                        assert_close(plan.bleed_amount, 0.125 * 8.5 / 19.75);
                        assert_close(plan.position_travel.x, 0.0);
                        assert_close(plan.position_travel.y, 0.0);
                    }
                    let artifact = export_loaded_document(&mut source, request, None).unwrap();
                    assert_eq!(artifact.layout.page_plans, preview.page_plans);
                    let output = LoDocument::load_mem(&artifact.bytes).unwrap();
                    assert_eq!(output.get_pages().len(), 2);
                    for (index, id) in output.get_pages().into_values().enumerate() {
                        let side = if index == 0 {
                            SheetSide::Front
                        } else {
                            SheetSide::Back
                        };
                        let plan = &preview.page_plans[index];
                        let (art, clip) =
                            resolve_page_plan(&preview, plan, &preview.placements[0], side);
                        let matrix = placement_matrix(
                            art,
                            plan.source_pdf_size,
                            preview.rotation_degrees,
                            index == 1 && rotate_back,
                        );
                        let operations = Content::decode(&output.get_page_content(id))
                            .unwrap()
                            .operations;
                        let exported_matrix =
                            operations.iter().find(|op| op.operator == "cm").unwrap();
                        for (value, expected) in exported_matrix.operands.iter().zip(matrix) {
                            assert_close(f64::from(value.as_float().unwrap()), expected);
                        }
                        let exported_clip =
                            operations.iter().find(|op| op.operator == "re").unwrap();
                        for (value, expected) in exported_clip.operands.iter().zip([
                            clip.x,
                            clip.y,
                            clip.width,
                            clip.height,
                        ]) {
                            assert_close(f64::from(value.as_float().unwrap()), expected);
                        }
                        // Independently check both source axes map to the planned dimensions.
                        let origin = transformed_point(matrix, 0.0, 0.0);
                        let corner = transformed_point(
                            matrix,
                            plan.source_pdf_size.width * 72.0,
                            plan.source_pdf_size.height * 72.0,
                        );
                        assert_close((corner.0 - origin.0).abs(), art.width);
                        assert_close((corner.1 - origin.1).abs(), art.height);
                    }
                }
            }
        }
    }

    #[test]
    fn back_rotation_rotates_artwork_content_inside_the_resolved_slot() {
        let target = ResolvedPlacement {
            x: 10.0,
            y: 20.0,
            width: 252.0,
            height: 144.0,
        };
        let source = size(3.5, 2.0);
        let matrix = placement_matrix(target, source, 0, true);

        assert_eq!(transformed_point(matrix, 0.0, 0.0), (262.0, 164.0));
        assert_eq!(transformed_point(matrix, 252.0, 144.0), (10.0, 20.0));

        let rotated = placement_matrix(target, source, 90, true);
        assert_eq!(transformed_point(rotated, 0.0, 0.0), (262.0, 20.0));
        assert_eq!(transformed_point(rotated, 252.0, 144.0), (10.0, 164.0));
    }

    #[test]
    fn source_page_rotation_is_normalized_clockwise() {
        let mut document = LoDocument::with_version("1.7");
        let page_90 = document.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 100.into()],
            "Rotate" => 90,
        });
        let page_270 = document.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 100.into()],
            "Rotate" => 270,
        });

        let clockwise = source_page_geometry(&document, page_90).unwrap();
        assert_close(clockwise.size.width, 100.0 / PT_PER_IN);
        assert_close(clockwise.size.height, 200.0 / PT_PER_IN);
        assert_eq!(
            transformed_point(clockwise.normalization, 0.0, 100.0),
            (100.0, 200.0),
            "the original top-left must become the displayed top-right"
        );

        let counter_clockwise = source_page_geometry(&document, page_270).unwrap();
        assert_eq!(
            transformed_point(counter_clockwise.normalization, 200.0, 0.0),
            (100.0, 200.0),
            "the original bottom-right must become the displayed top-right"
        );
    }

    #[test]
    fn shared_resource_imposition_reuses_artwork_across_placements_and_sheets() {
        let mut source = LoDocument::with_version("1.7");
        let pages_id = source.new_object_id();
        let artwork = (0..1_000_000).map(|index| (index % 251) as u8).collect();
        let artwork_id = source.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 864.into(), 1296.into()],
            },
            artwork,
        ));
        let content_id = source.add_object(Stream::new(Dictionary::new(), b"/Artwork Do".to_vec()));
        let page_id = source.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => dictionary! {
                "XObject" => dictionary! { "Artwork" => artwork_id },
            },
        });
        let unused_page_id = source.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => Dictionary::new(),
        });
        source.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id), Object::Reference(unused_page_id)],
                "Count" => 2,
                "MediaBox" => vec![0.into(), 0.into(), 864.into(), 1296.into()],
                "CropBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            }),
        );
        let metadata_id = source.add_object(Stream::new(
            dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
            b"<production-metadata />".to_vec(),
        ));
        let output_intent_id = source.add_object(dictionary! {
            "Type" => "OutputIntent",
            "S" => "GTS_PDFX",
            "OutputConditionIdentifier" => "Print profile",
        });
        let optional_content_id = source.add_object(dictionary! {
            "Type" => "OCG",
            "Name" => "Artwork layer",
        });
        let catalog_id = source.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "Metadata" => metadata_id,
            "OutputIntents" => vec![Object::Reference(output_intent_id)],
            "OCProperties" => dictionary! {
                "OCGs" => vec![Object::Reference(optional_content_id)],
                "D" => dictionary! { "ON" => vec![Object::Reference(optional_content_id)] },
            },
            "OpenAction" => dictionary! { "S" => "JavaScript", "JS" => "app.alert('no')" },
            "Names" => dictionary! { "JavaScript" => Dictionary::new() },
        });
        source.trailer.set("Root", catalog_id);
        let mut source_bytes = Vec::new();
        source.save_to(&mut source_bytes).unwrap();

        let mut repeated_layout = layout(BleedOption::FitInside, 0.0);
        repeated_layout.quantity_requested = 250;
        repeated_layout.impressions_requested = 250;
        repeated_layout.pieces_per_sheet = 4;
        repeated_layout.sheets_required = 63;
        repeated_layout.total_pieces_produced = 252;
        repeated_layout.extra_pieces_produced = 2;
        repeated_layout.source_pdf_size = size(12.0, 18.0);
        repeated_layout.placements = (0..4)
            .map(|index| PiecePlacement {
                index,
                row: index / 2,
                column: index % 2,
                x: (index % 2) as f64 * 6.0,
                y: (index / 2) as f64 * 9.0,
                width: 5.0,
                height: 7.0,
                finished_x: (index % 2) as f64 * 6.0,
                finished_y: (index / 2) as f64 * 9.0,
                finished_width: 5.0,
                finished_height: 7.0,
            })
            .collect();

        let mut source = LoDocument::load_mem(&source_bytes).unwrap();
        let source_page_ids = source.get_pages().into_values().collect::<Vec<_>>();
        let imposed = compose_shared_resource_imposition(
            &mut source,
            &source_page_ids,
            &repeated_layout,
            None,
        )
        .unwrap();
        let imposed_document = LoDocument::load_mem(&imposed).unwrap();

        assert_eq!(imposed_document.get_pages().len(), 63);
        let catalog = imposed_document.catalog().unwrap();
        assert!(catalog.get(b"OpenAction").is_err());
        assert!(catalog.get(b"Names").is_err());
        assert!(catalog.get(b"Metadata").is_ok());
        assert!(catalog.get(b"OutputIntents").is_ok());
        assert!(catalog.get(b"OCProperties").is_ok());
        assert!(imposed.len() < source_bytes.len() * 2);
        let first_page_id = imposed_document.get_pages()[&1];
        let content = imposed_document.get_page_content(first_page_id);
        let first_page = imposed_document.get_dictionary(first_page_id).unwrap();
        let crop_box = first_page.get(b"CropBox").unwrap().as_array().unwrap();
        assert_eq!(
            content.windows(2).filter(|window| *window == b"Do").count(),
            4
        );
        assert_eq!(crop_box[2].as_float().unwrap(), 864.0);
        assert_eq!(crop_box[3].as_float().unwrap(), 1296.0);
        let form_count = imposed_document
            .objects
            .values()
            .filter(|object| {
                object
                    .as_stream()
                    .ok()
                    .and_then(|stream| stream.dict.get(b"Subtype").ok())
                    .and_then(|subtype| subtype.as_name().ok())
                    == Some(b"Form")
            })
            .count();
        assert_eq!(form_count, 2, "unused source pages should not become forms");
    }

    #[test]
    fn output_page_count_rejects_duplex_overflow() {
        assert_eq!(output_page_count(7, false).unwrap(), 7);
        assert_eq!(output_page_count(7, true).unwrap(), 14);

        let error = output_page_count(usize::MAX, true).unwrap_err();
        assert!(matches!(error, AppError::PayloadTooLarge(_)));
        assert!(error.to_string().contains("too many output pages"));
    }

    #[test]
    fn source_form_keeps_single_encoded_content_stream_encoded() {
        let mut source = LoDocument::with_version("1.7");
        let pages_id = source.new_object_id();
        let mut content = Stream::new(
            Dictionary::new(),
            b"0 0 100 100 re 0.2 0.4 0.8 rg f".repeat(2_000),
        );
        content.compress().unwrap();
        let encoded_content = content.content.clone();
        let content_id = source.add_object(content);
        let page_id = source.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => Dictionary::new(),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        source.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );

        let mut budget = MAX_EXPORT_DECODED_CONTENT_BYTES;
        let form = create_source_form(&mut source, page_id, &mut budget, None).unwrap();
        let stream = source
            .get_object(form.object_id)
            .unwrap()
            .as_stream()
            .unwrap();

        assert_eq!(stream.content, encoded_content);
        assert_eq!(
            stream.dict.get(b"Filter").unwrap().as_name().unwrap(),
            b"FlateDecode"
        );
        assert!(stream.dict.get(b"Matrix").is_ok());
    }

    #[test]
    fn single_encoded_stream_consumes_the_decoded_content_budget() {
        let mut stream = Stream::new(Dictionary::new(), b"q Q\n".repeat(1_000));
        stream.compress().unwrap();
        let mut budget = 100;

        let error = account_decoded_stream(&stream, &mut budget).unwrap_err();

        assert!(matches!(error, AppError::PayloadTooLarge(_)));
    }

    #[test]
    fn unsupported_multi_stream_content_encoding_returns_an_error() {
        let mut source = LoDocument::with_version("1.7");
        let pages_id = source.new_object_id();
        let plain_id = source.add_object(Stream::new(Dictionary::new(), b"q".to_vec()));
        let unsupported_id = source.add_object(Stream::new(
            dictionary! { "Filter" => "DCTDecode" },
            vec![1, 2, 3, 4],
        ));
        let page_id = source.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => vec![Object::Reference(plain_id), Object::Reference(unsupported_id)],
            "Resources" => Dictionary::new(),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        source.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );

        let mut budget = MAX_EXPORT_DECODED_CONTENT_BYTES;
        let error = create_source_form(&mut source, page_id, &mut budget, None).unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported multi-stream content encoding"));
    }

    #[test]
    fn resolved_placement_use_as_is_keeps_source_size_and_bleed_offset() {
        let resolved = resolve_placement(
            &layout(BleedOption::UseAsIs, 0.125),
            &placement(),
            SheetSide::Front,
        );

        assert_close(resolved.x, 0.875 * PT_PER_IN);
        assert_close(resolved.y, (18.0 - 0.875 - 2.25) * PT_PER_IN);
        assert_close(resolved.width, 3.75 * PT_PER_IN);
        assert_close(resolved.height, 2.25 * PT_PER_IN);
    }

    #[test]
    fn resolved_placement_scale_to_bleed_uses_detected_bleed_when_available() {
        let resolved = resolve_placement(
            &layout(BleedOption::ScaleToBleed, 0.125),
            &placement(),
            SheetSide::Front,
        );

        assert_close(resolved.x, 0.875 * PT_PER_IN);
        assert_close(resolved.y, (18.0 - 0.875 - 2.25) * PT_PER_IN);
        assert_close(resolved.width, 3.75 * PT_PER_IN);
        assert_close(resolved.height, 2.25 * PT_PER_IN);
    }

    #[test]
    fn resolved_placement_scale_to_bleed_resizes_odd_source_without_cropping() {
        let mut no_bleed = layout(BleedOption::ScaleToBleed, 0.0);
        no_bleed.source_pdf_size = size(3.5, 2.0);
        let resolved = resolve_placement(&no_bleed, &placement(), SheetSide::Front);

        assert_close(resolved.x, 0.875 * PT_PER_IN);
        assert_close(resolved.y, (18.0 - 0.875 - 2.25) * PT_PER_IN);
        assert_close(resolved.width, 3.75 * PT_PER_IN);
        assert_close(resolved.height, 2.25 * PT_PER_IN);
    }

    #[test]
    fn resolved_placement_fit_inside_centers_artwork_in_finished_rect() {
        let resolved = resolve_placement(
            &layout(BleedOption::FitInside, 0.125),
            &placement(),
            SheetSide::Front,
        );

        assert_close(resolved.x, (1.0 + (3.5 - 3.3333333333) / 2.0) * PT_PER_IN);
        assert_close(resolved.y, (18.0 - 1.0 - 2.0) * PT_PER_IN);
        assert_close(resolved.width, (10.0 / 3.0) * PT_PER_IN);
        assert_close(resolved.height, 2.0 * PT_PER_IN);
    }

    #[test]
    fn back_side_long_edge_alignment_mirrors_horizontally() {
        let mut layout = layout(BleedOption::UseAsIs, 0.125);
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });

        let resolved = resolve_placement(&layout, &placement(), SheetSide::Back);

        assert_close(resolved.x, (12.0 - 0.875 - 3.75) * PT_PER_IN);
        assert_close(resolved.y, (18.0 - 0.875 - 2.25) * PT_PER_IN);
        assert_close(resolved.width, 3.75 * PT_PER_IN);
        assert_close(resolved.height, 2.25 * PT_PER_IN);
    }

    #[test]
    fn back_side_short_edge_alignment_mirrors_vertically() {
        let mut layout = layout(BleedOption::UseAsIs, 0.125);
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::ShortEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });

        let resolved = resolve_placement(&layout, &placement(), SheetSide::Back);

        assert_close(resolved.x, 0.875 * PT_PER_IN);
        assert_close(resolved.y, 0.875 * PT_PER_IN);
        assert_close(resolved.width, 3.75 * PT_PER_IN);
        assert_close(resolved.height, 2.25 * PT_PER_IN);
    }

    #[test]
    fn landscape_back_side_long_edge_alignment_mirrors_vertically() {
        let mut layout = layout(BleedOption::UseAsIs, 0.125);
        layout.parent_sheet_size = size(18.0, 12.0);
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });

        let resolved = resolve_placement(&layout, &placement(), SheetSide::Back);

        assert_close(resolved.x, 0.875 * PT_PER_IN);
        assert_close(resolved.y, 0.875 * PT_PER_IN);
    }

    #[test]
    fn landscape_back_side_short_edge_alignment_mirrors_horizontally() {
        let mut layout = layout(BleedOption::UseAsIs, 0.125);
        layout.parent_sheet_size = size(18.0, 12.0);
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::ShortEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });

        let resolved = resolve_placement(&layout, &placement(), SheetSide::Back);

        assert_close(resolved.x, (18.0 - 0.875 - 3.75) * PT_PER_IN);
        assert_close(resolved.y, (12.0 - 0.875 - 2.25) * PT_PER_IN);
    }

    #[test]
    fn rotating_a_duplex_back_toggles_both_mirror_axes() {
        let mut layout = layout(BleedOption::UseAsIs, 0.125);
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: true,
            back_alignment: String::new(),
        });

        let resolved = resolve_placement(&layout, &placement(), SheetSide::Back);

        assert_close(resolved.x, 0.875 * PT_PER_IN);
        assert_close(resolved.y, 0.875 * PT_PER_IN);
    }

    #[test]
    fn double_sided_export_requires_two_source_pages() {
        let mut layout = layout(BleedOption::UseAsIs, 0.125);
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });

        let err = validate_source_page_count(1, &layout).unwrap_err();
        assert_eq!(
            err.to_string(),
            "double-sided gang-up export requires a source PDF with at least two pages"
        );
    }

    #[test]
    fn annotation_flattening_rejects_page_counts_before_scanning_pages() {
        assert!(validate_flatten_page_count(MAX_SOURCE_PDF_PAGES).is_ok());
        let error = validate_flatten_page_count(MAX_SOURCE_PDF_PAGES + 1).unwrap_err();
        assert!(matches!(error, AppError::PayloadTooLarge(_)));
    }

    #[test]
    fn unique_single_sided_source_pages_advance_by_sheet_and_placement() {
        let mut layout = layout(BleedOption::UseAsIs, 0.125);
        layout.imposition_mode = ImpositionMode::Unique;
        layout.impressions_requested = 35;
        layout.pieces_per_sheet = 30;

        assert_eq!(source_page_number(&layout, 0, 0, SheetSide::Front), Some(1));
        assert_eq!(
            source_page_number(&layout, 0, 29, SheetSide::Front),
            Some(30)
        );
        assert_eq!(
            source_page_number(&layout, 1, 0, SheetSide::Front),
            Some(31)
        );
        assert_eq!(source_page_number(&layout, 1, 5, SheetSide::Front), None);
    }

    #[test]
    fn unique_double_sided_source_pages_use_front_back_pairs() {
        let mut layout = layout(BleedOption::UseAsIs, 0.125);
        layout.imposition_mode = ImpositionMode::Unique;
        layout.impressions_requested = 2;
        layout.pieces_per_sheet = 30;
        layout.duplex = Some(DuplexSettings {
            flip_edge: DuplexFlipEdge::LongEdge,
            rotate_back_180: false,
            back_alignment: String::new(),
        });

        assert_eq!(source_page_number(&layout, 0, 0, SheetSide::Front), Some(1));
        assert_eq!(source_page_number(&layout, 0, 0, SheetSide::Back), Some(2));
        assert_eq!(source_page_number(&layout, 0, 1, SheetSide::Front), Some(3));
        assert_eq!(source_page_number(&layout, 0, 1, SheetSide::Back), Some(4));
        assert_eq!(source_page_number(&layout, 0, 2, SheetSide::Front), None);
    }

    #[test]
    fn repeated_source_pages_follow_exact_quantities() {
        let mut layout = layout(BleedOption::UseAsIs, 0.125);
        layout.imposition_mode = ImpositionMode::Repeat;
        layout.impression_quantities = Some(vec![2, 0, 1]);
        layout.impressions_requested = 3;
        layout.pieces_per_sheet = 4;

        assert_eq!(source_page_number(&layout, 0, 0, SheetSide::Front), Some(1));
        assert_eq!(source_page_number(&layout, 0, 1, SheetSide::Front), Some(1));
        assert_eq!(source_page_number(&layout, 0, 2, SheetSide::Front), Some(3));
        assert_eq!(source_page_number(&layout, 0, 3, SheetSide::Front), None);
    }
}
