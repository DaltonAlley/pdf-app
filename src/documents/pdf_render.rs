use std::{collections::HashSet, io::Cursor, path::Path, sync::Arc};

use image::{codecs::jpeg::JpegEncoder, ColorType};
use pdfium_render::prelude::{PdfDocument, PdfRenderConfig, Pdfium};
use rayon::prelude::*;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

use super::{page_selection::PageSelection, pdf_io};
use crate::{
    adapters::{validate_download_size, FormData, UploadFile},
    error::{AppError, AppResult},
    progress::{report, ProgressCallback},
    MAX_SOURCE_PDF_PAGES,
};

const PT_PER_IN: f64 = 72.0;
const DEFAULT_PDF_IMAGE_DPI: u16 = 300;
const MIN_PDF_IMAGE_DPI: u16 = 72;
const MAX_PDF_IMAGE_DPI: u16 = 600;
const DEFAULT_PDF_IMAGE_JPEG_QUALITY: u8 = 92;
const MIN_PDF_IMAGE_JPEG_QUALITY: u8 = 1;
const MAX_PDF_IMAGE_JPEG_QUALITY: u8 = 100;
const MAX_SINGLE_RENDER_PIXELS: u64 = 50_000_000;
const MAX_BUFFERED_RASTER_BYTES: u64 = 256 * 1024 * 1024;
const RASTER_BYTES_PER_PIXEL: u64 = 4;
const PREVIEW_MAX_WIDTH: u32 = 480;
const PREVIEW_MAX_HEIGHT: u32 = 2_048;
const PREVIEW_MAX_PIXELS: u64 = 2_000_000;
const PREVIEW_RENDER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
pub(crate) const MAX_PREVIEW_BATCH_PAGES: usize = 4;
const PREVIEW_BATCH_MAGIC: &[u8; 8] = b"PDFPV001";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RasterFormat {
    Png,
    Jpeg,
}

#[derive(Debug)]
pub(crate) struct RenderedImage {
    pub(crate) page: usize,
    pub(crate) zip_path: String,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) struct PreviewRaster {
    page: usize,
    image: image::DynamicImage,
}

pub(crate) fn raster_preview_pages(
    pdfium: &Pdfium,
    path: &Path,
    page_numbers: Vec<usize>,
) -> AppResult<Vec<PreviewRaster>> {
    let started = std::time::Instant::now();
    let deadline = started + PREVIEW_RENDER_DEADLINE;
    if page_numbers.is_empty() {
        return Err(AppError::bad_request(
            "preview batch requires at least one page",
        ));
    }
    if page_numbers.len() > MAX_PREVIEW_BATCH_PAGES {
        return Err(AppError::bad_request(format!(
            "preview batches are limited to {MAX_PREVIEW_BATCH_PAGES} pages"
        )));
    }
    let mut unique_pages = HashSet::with_capacity(page_numbers.len());
    for page_number in &page_numbers {
        if !unique_pages.insert(*page_number) {
            return Err(AppError::bad_request(format!(
                "preview page {page_number} was requested more than once"
            )));
        }
    }

    let document = pdf_io::load_pdf_path(pdfium, path)?;
    let page_count = pdf_io::page_count(document.pages().len())?;
    let mut rasters = Vec::with_capacity(page_numbers.len());
    for page_number in page_numbers {
        // PDFium rendering is a native blocking call and cannot be killed
        // safely. The deadline is therefore cooperative: it prevents another
        // page from starting after the budget has elapsed.
        ensure_preview_deadline(deadline)?;
        rasters.push(raster_preview_page(&document, page_count, page_number)?);
    }
    tracing::debug!(
        page_count = rasters.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "rendered source preview pages"
    );
    Ok(rasters)
}

fn ensure_preview_deadline(deadline: std::time::Instant) -> AppResult<()> {
    if std::time::Instant::now() >= deadline {
        return Err(AppError::RequestTimeout(
            "preview rendering exceeded its page-boundary deadline".to_string(),
        ));
    }
    Ok(())
}

fn raster_preview_page(
    document: &PdfDocument<'_>,
    page_count: usize,
    page_number: usize,
) -> AppResult<PreviewRaster> {
    if page_number == 0 || page_number > page_count {
        return Err(AppError::bad_request(format!(
            "preview page {page_number} is out of range"
        )));
    }
    let page = document
        .pages()
        .get(pdf_io::page_index(page_number)?)
        .map_err(|error| {
            AppError::bad_request(format!("could not read page {page_number}: {error}"))
        })?;
    let width = f64::from(page.width().value);
    let height = f64::from(page.height().value);
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(AppError::bad_request("PDF page has invalid dimensions"));
    }
    let scale = (f64::from(PREVIEW_MAX_WIDTH) / width)
        .min(f64::from(PREVIEW_MAX_HEIGHT) / height)
        .min((PREVIEW_MAX_PIXELS as f64 / (width * height)).sqrt());
    let target_width = (width * scale).floor().max(1.0) as i32;
    let target_height = (height * scale).floor().max(1.0) as i32;
    let config = PdfRenderConfig::new()
        .set_target_width(target_width)
        .set_maximum_height(target_height)
        .use_print_quality(true)
        .force_half_tone(true);
    let image = page
        .render_with_config(&config)
        .and_then(|bitmap| bitmap.as_image())
        .map_err(|error| {
            AppError::internal_cause(
                format!("could not render preview page {page_number}"),
                error,
            )
        })?;
    Ok(PreviewRaster {
        page: page_number,
        image,
    })
}

pub(crate) fn encode_preview_page(raster: PreviewRaster) -> AppResult<Vec<u8>> {
    encode_rendered_image(raster.image, RasterFormat::Png, RasterOptions::default())
}

pub(crate) fn encode_preview_batch(rasters: Vec<PreviewRaster>) -> AppResult<Vec<u8>> {
    let encoded = rasters
        .into_par_iter()
        .map(|raster| {
            let page = raster.page;
            Ok((page, encode_preview_page(raster)?))
        })
        .collect::<AppResult<Vec<_>>>()?;
    let count = u32::try_from(encoded.len())
        .map_err(|error| AppError::internal_cause("preview batch is too large", error))?;
    let payload_capacity = encoded.iter().try_fold(12usize, |capacity, (_, bytes)| {
        capacity
            .checked_add(8)
            .and_then(|value| value.checked_add(bytes.len()))
            .ok_or_else(|| AppError::payload_too_large("preview batch is too large"))
    })?;
    let mut payload = Vec::with_capacity(payload_capacity);
    payload.extend_from_slice(PREVIEW_BATCH_MAGIC);
    payload.extend_from_slice(&count.to_be_bytes());
    for (page, bytes) in encoded {
        let page = u32::try_from(page)
            .map_err(|error| AppError::internal_cause("preview page number is too large", error))?;
        let length = u32::try_from(bytes.len())
            .map_err(|error| AppError::internal_cause("preview PNG is too large", error))?;
        payload.extend_from_slice(&page.to_be_bytes());
        payload.extend_from_slice(&length.to_be_bytes());
        payload.extend_from_slice(&bytes);
    }
    Ok(payload)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RasterOptions {
    dpi: u16,
    jpeg_quality: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderLimits {
    max_pages: usize,
    max_download_bytes: Option<usize>,
}

impl RenderLimits {
    pub(crate) const fn new(max_pages: usize, max_download_bytes: Option<usize>) -> Self {
        Self {
            max_pages,
            max_download_bytes,
        }
    }
}

impl RasterOptions {
    pub(crate) fn from_form(form: &FormData) -> AppResult<Self> {
        Ok(Self {
            dpi: parse_optional_bounded(
                form.field("dpi"),
                DEFAULT_PDF_IMAGE_DPI,
                MIN_PDF_IMAGE_DPI,
                MAX_PDF_IMAGE_DPI,
                "dpi",
            )?,
            jpeg_quality: parse_optional_bounded(
                form.field("jpegQuality"),
                DEFAULT_PDF_IMAGE_JPEG_QUALITY,
                MIN_PDF_IMAGE_JPEG_QUALITY,
                MAX_PDF_IMAGE_JPEG_QUALITY,
                "jpegQuality",
            )?,
        })
    }
}

impl Default for RasterOptions {
    fn default() -> Self {
        Self {
            dpi: DEFAULT_PDF_IMAGE_DPI,
            jpeg_quality: DEFAULT_PDF_IMAGE_JPEG_QUALITY,
        }
    }
}

impl RasterFormat {
    pub(crate) fn content_type(self) -> &'static str {
        match self {
            RasterFormat::Png => "image/png",
            RasterFormat::Jpeg => "image/jpeg",
        }
    }

    pub(crate) fn extension(self) -> &'static str {
        match self {
            RasterFormat::Png => "png",
            RasterFormat::Jpeg => "jpg",
        }
    }
}

pub(crate) fn render_pdf_files(
    pdfium: Arc<Pdfium>,
    files: Vec<UploadFile>,
    selection: PageSelection,
    limits: RenderLimits,
    format: RasterFormat,
    options: RasterOptions,
    progress: Option<ProgressCallback>,
) -> AppResult<Vec<RenderedImage>> {
    let max_pages = limits.max_pages;
    let max_download_bytes = limits.max_download_bytes;
    let file_count = files.len();
    let pdfium = pdfium.as_ref();
    let mut prepared_files = Vec::with_capacity(file_count);
    let mut total_page_count = 0usize;

    for file in files {
        let document = pdf_io::load_pdf(pdfium, file.bytes)?;
        let page_count = pdf_io::page_count(document.pages().len())?;
        total_page_count = total_page_count
            .checked_add(page_count)
            .ok_or_else(|| AppError::payload_too_large("source PDFs contain too many pages"))?;
        if total_page_count > MAX_SOURCE_PDF_PAGES {
            return Err(AppError::payload_too_large(format!(
                "source PDFs are limited to {MAX_SOURCE_PDF_PAGES} total pages"
            )));
        }
        prepared_files.push(PreparedPdf {
            document,
            page_count,
        });
    }

    let selected_pages = resolve_render_pages(selection, total_page_count, max_pages)?;
    validate_render_pages(&prepared_files, &selected_pages, options.dpi)?;
    let mut rendered = Vec::new();
    let mut rendered_bytes = 0usize;
    let mut global_page_offset = 0;

    for (index, file) in prepared_files.into_iter().enumerate() {
        let remaining_pages = max_pages.saturating_sub(rendered.len());
        if remaining_pages == 0 {
            return Err(AppError::bad_request(format!(
                "requested more than {max_pages} pages"
            )));
        }

        let file_page_count = file.page_count;
        let file_pages =
            selected_pages_for_file(&selected_pages, file_page_count, global_page_offset);
        global_page_offset += file_page_count;
        if file_pages.is_empty() {
            continue;
        }

        let range_start = render_percent(index, 0, file_count);
        let range_end = render_percent(index + 1, 0, file_count);

        if file_count > 1 {
            report(
                &progress,
                range_start,
                format!("Reading PDF {} of {file_count}", index + 1),
            )?;
        }

        let images = render_pdf_pages(RenderPageJob {
            document: file.document,
            selected_pages: file_pages,
            max_pages: remaining_pages,
            format,
            options,
            progress: progress.clone(),
            progress_range: ProgressRange {
                start: range_start,
                end: range_end,
            },
            file_position: (index + 1, file_count),
            max_download_bytes,
            prior_output_bytes: rendered_bytes,
        })?;

        for mut image in images {
            rendered_bytes = rendered_bytes
                .checked_add(image.bytes.len())
                .ok_or_else(|| {
                    AppError::payload_too_large("rendered images exceed the download size limit")
                })?;
            let output_index = rendered.len() + 1;
            image.zip_path = format!("page-{output_index:04}.{}", format.extension());
            rendered.push(image);
        }
    }

    Ok(rendered)
}

struct PreparedPdf<'a> {
    document: PdfDocument<'a>,
    page_count: usize,
}

fn validate_render_pages(
    files: &[PreparedPdf<'_>],
    selected_pages: &[usize],
    dpi: u16,
) -> AppResult<()> {
    let mut global_page_offset = 0usize;
    for file in files {
        for page_number in
            selected_pages_for_file(selected_pages, file.page_count, global_page_offset)
        {
            let page = file
                .document
                .pages()
                .get(pdf_io::page_index(page_number)?)
                .map_err(|err| {
                    AppError::bad_request(format!("could not read page {page_number}: {err}"))
                })?;
            let pixels = rendered_pixel_count(
                f64::from(page.width().value),
                f64::from(page.height().value),
                dpi,
            )?;
            if pixels > MAX_SINGLE_RENDER_PIXELS {
                return Err(AppError::payload_too_large(format!(
                    "The upload succeeded, but page {page_number} is too large to render at {dpi} DPI ({pixels} pixels). Choose a lower quality."
                )));
            }
        }
        global_page_offset += file.page_count;
    }
    Ok(())
}

fn rendered_pixel_count(width_points: f64, height_points: f64, dpi: u16) -> AppResult<u64> {
    if !width_points.is_finite()
        || !height_points.is_finite()
        || width_points <= 0.0
        || height_points <= 0.0
    {
        return Err(AppError::bad_request("PDF page has invalid dimensions"));
    }
    let width = (width_points * f64::from(dpi) / PT_PER_IN).ceil();
    let height = (height_points * f64::from(dpi) / PT_PER_IN).ceil();
    if width > u32::MAX as f64 || height > u32::MAX as f64 {
        return Err(AppError::payload_too_large(
            "PDF page dimensions are too large to render",
        ));
    }
    Ok((width as u64) * (height as u64))
}

fn render_pdf_pages(job: RenderPageJob<'_>) -> AppResult<Vec<RenderedImage>> {
    let RenderPageJob {
        document,
        selected_pages,
        max_pages,
        format,
        options,
        progress,
        progress_range,
        file_position,
        max_download_bytes,
        prior_output_bytes,
    } = job;

    if selected_pages.len() > max_pages {
        return Err(AppError::bad_request(format!(
            "requested {} pages; limit is {max_pages}",
            selected_pages.len()
        )));
    }

    report(
        &progress,
        progress_range.start,
        format!("Preparing {} page(s)", selected_pages.len()),
    )?;

    let total_pages = selected_pages.len();
    let mut rendered = Vec::with_capacity(total_pages);
    let mut output_bytes = 0usize;
    let max_batch_len = rayon::current_num_threads().max(1);
    let render_config = PdfRenderConfig::new()
        .scale_page_by_factor(options.dpi as f32 / PT_PER_IN as f32)
        .use_print_quality(true)
        .force_half_tone(true);
    let mut raw_pages = Vec::with_capacity(max_batch_len);
    let mut buffered_bytes = 0u64;

    for (selected_index, &page_number) in selected_pages.iter().enumerate() {
        let page = document
            .pages()
            .get(pdf_io::page_index(page_number)?)
            .map_err(|err| {
                AppError::bad_request(format!("could not read page {page_number}: {err}"))
            })?;
        let page_bytes = rendered_pixel_count(
            f64::from(page.width().value),
            f64::from(page.height().value),
            options.dpi,
        )?
        .saturating_mul(RASTER_BYTES_PER_PIXEL);
        if should_flush_raster_batch(raw_pages.len(), buffered_bytes, page_bytes, max_batch_len) {
            append_encoded_pages(
                encode_raw_pages(
                    std::mem::take(&mut raw_pages),
                    format,
                    options,
                    &progress,
                    progress_range.page_percent(rendered.len(), total_pages),
                )?,
                &mut rendered,
                &mut output_bytes,
                prior_output_bytes,
                max_download_bytes,
                &progress,
                progress_range,
                total_pages,
                file_position,
            )?;
            buffered_bytes = 0;
        }

        report(
            &progress,
            progress_range.page_percent(selected_index, total_pages),
            format!("Rendering page {page_number}"),
        )?;
        let image = page
            .render_with_config(&render_config)
            .and_then(|bitmap| bitmap.as_image())
            .map_err(|err| {
                AppError::internal_cause(format!("could not render page {page_number}"), err)
            })?;
        raw_pages.push((page_number, image));
        buffered_bytes = buffered_bytes.saturating_add(page_bytes);
    }
    append_encoded_pages(
        encode_raw_pages(
            raw_pages,
            format,
            options,
            &progress,
            progress_range.page_percent(rendered.len(), total_pages),
        )?,
        &mut rendered,
        &mut output_bytes,
        prior_output_bytes,
        max_download_bytes,
        &progress,
        progress_range,
        total_pages,
        file_position,
    )?;

    Ok(rendered)
}

fn should_flush_raster_batch(
    batch_len: usize,
    buffered_bytes: u64,
    next_page_bytes: u64,
    max_batch_len: usize,
) -> bool {
    batch_len > 0
        && (batch_len >= max_batch_len
            || buffered_bytes.saturating_add(next_page_bytes) > MAX_BUFFERED_RASTER_BYTES)
}

fn encode_raw_pages(
    raw_pages: Vec<(usize, image::DynamicImage)>,
    format: RasterFormat,
    options: RasterOptions,
    progress: &Option<ProgressCallback>,
    percent: u8,
) -> AppResult<Vec<RenderedImage>> {
    raw_pages
        .into_par_iter()
        .map(|(page, image)| {
            report(progress, percent, format!("Encoding page {page}"))?;
            Ok(RenderedImage {
                page,
                zip_path: format!("page-{:04}.{}", page, format.extension()),
                bytes: encode_rendered_image(image, format, options)?,
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn append_encoded_pages(
    encoded: Vec<RenderedImage>,
    rendered: &mut Vec<RenderedImage>,
    output_bytes: &mut usize,
    prior_output_bytes: usize,
    max_download_bytes: Option<usize>,
    progress: &Option<ProgressCallback>,
    progress_range: ProgressRange,
    total_pages: usize,
    file_position: (usize, usize),
) -> AppResult<()> {
    for image in encoded {
        *output_bytes = output_bytes.checked_add(image.bytes.len()).ok_or_else(|| {
            AppError::payload_too_large("rendered images exceed the download size limit")
        })?;
        let total_output_bytes =
            prior_output_bytes
                .checked_add(*output_bytes)
                .ok_or_else(|| {
                    AppError::payload_too_large("rendered images exceed the download size limit")
                })?;
        validate_download_size(total_output_bytes, max_download_bytes)?;
        rendered.push(image);
        report(
            progress,
            progress_range.page_percent(rendered.len(), total_pages),
            rendered_message(rendered.len(), total_pages, file_position),
        )?;
    }
    Ok(())
}

struct RenderPageJob<'a> {
    document: PdfDocument<'a>,
    selected_pages: Vec<usize>,
    max_pages: usize,
    format: RasterFormat,
    options: RasterOptions,
    progress: Option<ProgressCallback>,
    progress_range: ProgressRange,
    file_position: (usize, usize),
    max_download_bytes: Option<usize>,
    prior_output_bytes: usize,
}

fn resolve_render_pages(
    selection: PageSelection,
    page_count: usize,
    max_pages: usize,
) -> AppResult<Vec<usize>> {
    let pages = pdf_io::resolve_pages(selection, page_count, "PDF has no pages to render")?;

    if pages.len() > max_pages {
        return Err(AppError::bad_request(format!(
            "requested {} pages; limit is {max_pages}",
            pages.len()
        )));
    }

    Ok(pages)
}

fn selected_pages_for_file(
    selected_pages: &[usize],
    page_count: usize,
    global_page_offset: usize,
) -> Vec<usize> {
    let first_global_page = global_page_offset + 1;
    let last_global_page = global_page_offset + page_count;

    selected_pages
        .iter()
        .copied()
        .filter(|page| (first_global_page..=last_global_page).contains(page))
        .map(|page| page - global_page_offset)
        .collect()
}

fn encode_rendered_image(
    image: image::DynamicImage,
    format: RasterFormat,
    options: RasterOptions,
) -> AppResult<Vec<u8>> {
    let mut bytes = Vec::new();
    match format {
        RasterFormat::Png => {
            image
                .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
                .map_err(|err| AppError::internal_cause("could not encode PNG", err))?;
        }
        RasterFormat::Jpeg => {
            let rgb = image.into_rgb8();
            JpegEncoder::new_with_quality(&mut bytes, options.jpeg_quality)
                .encode(&rgb, rgb.width(), rgb.height(), ColorType::Rgb8.into())
                .map_err(|err| AppError::internal_cause("could not encode JPEG", err))?;
        }
    }
    Ok(bytes)
}

fn parse_optional_bounded<T>(
    value: Option<&str>,
    default: T,
    min: T,
    max: T,
    field: &str,
) -> AppResult<T>
where
    T: Copy + std::fmt::Display + std::str::FromStr + PartialOrd,
{
    let Some(value) = value else {
        return Ok(default);
    };
    let parsed = value
        .parse::<T>()
        .map_err(|_| AppError::bad_request(format!("{field} must be a number")))?;

    if parsed < min || parsed > max {
        return Err(AppError::bad_request(format!(
            "{field} must be between {min} and {max}"
        )));
    }

    Ok(parsed)
}

pub(crate) fn zip_images(images: Vec<RenderedImage>) -> AppResult<Vec<u8>> {
    let estimated_size = estimated_zip_capacity(images.iter().map(|image| image.bytes.len()))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(estimated_size).map_err(|_| {
        AppError::payload_too_large("rendered images are too large to package as a ZIP archive")
    })?;
    let mut cursor = Cursor::new(bytes);
    let mut zip = ZipWriter::new(&mut cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    for image in images {
        zip.start_file(image.zip_path, options)?;
        std::io::Write::write_all(&mut zip, &image.bytes)?;
    }
    zip.finish()?;
    Ok(cursor.into_inner())
}

fn estimated_zip_capacity(image_sizes: impl IntoIterator<Item = usize>) -> AppResult<usize> {
    image_sizes.into_iter().try_fold(0usize, |total, size| {
        size.checked_add(128)
            .and_then(|entry_size| total.checked_add(entry_size))
            .ok_or_else(|| {
                AppError::payload_too_large(
                    "rendered images are too large to package as a ZIP archive",
                )
            })
    })
}

fn render_percent(file_index: usize, page_index: usize, file_count: usize) -> u8 {
    let total_units = file_count.max(1) * 100;
    let completed_units = file_index * 100 + page_index.min(100);
    35 + ((completed_units * 50) / total_units) as u8
}

fn rendered_message(completed: usize, total: usize, file_position: (usize, usize)) -> String {
    let (file_index, file_count) = file_position;
    if file_count > 1 {
        format!("Rendered {completed} of {total} pages in PDF {file_index} of {file_count}")
    } else {
        format!("Rendered {completed} of {total} pages")
    }
}

#[derive(Clone, Copy)]
struct ProgressRange {
    start: u8,
    end: u8,
}

impl ProgressRange {
    fn page_percent(self, completed_pages: usize, total_pages: usize) -> u8 {
        let span = usize::from(self.end.saturating_sub(self.start));
        self.start + ((completed_pages * span) / total_pages.max(1)) as u8
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use pdfium_render::prelude::{PdfPagePaperSize, Pdfium};
    use zip::{DateTime, ZipArchive};

    fn pdfium_or_skip() -> Option<crate::adapters::TestPdfium> {
        let pdfium = crate::test_pdfium();
        if pdfium.is_none() {
            eprintln!("skipping PDFium test: PDF_TOOLS_PDFIUM_PATH is not set");
        }
        pdfium
    }

    fn sample_pdf_bytes(pdfium: &Pdfium, page_count: usize) -> Vec<u8> {
        let mut doc = pdfium.create_new_pdf().unwrap();
        for _ in 0..page_count {
            doc.pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .unwrap();
        }
        doc.save_to_bytes().unwrap()
    }

    fn sample_pdf_document(pdfium: &Pdfium, page_count: usize) -> PdfDocument<'_> {
        pdf_io::load_pdf(pdfium, sample_pdf_bytes(pdfium, page_count).into()).unwrap()
    }

    #[test]
    fn raster_formats_define_download_metadata() {
        assert_eq!(RasterFormat::Png.content_type(), "image/png");
        assert_eq!(RasterFormat::Png.extension(), "png");
        assert_eq!(RasterFormat::Jpeg.content_type(), "image/jpeg");
        assert_eq!(RasterFormat::Jpeg.extension(), "jpg");
    }

    #[test]
    fn raster_encoding_checks_for_cancellation_before_each_page() {
        let progress: ProgressCallback = Arc::new(|_, _| true);
        let error = encode_raw_pages(
            vec![(1, image::DynamicImage::new_rgb8(1, 1))],
            RasterFormat::Png,
            RasterOptions::default(),
            &Some(progress),
            50,
        )
        .unwrap_err();

        assert!(matches!(error, AppError::Cancelled(_)));
    }

    #[test]
    fn raster_options_default_to_print_quality() {
        let options = RasterOptions::from_form(&FormData::default()).unwrap();

        assert_eq!(options.dpi, 300);
        assert_eq!(options.jpeg_quality, 92);
    }

    #[test]
    fn raster_options_accept_bounded_quality_fields() {
        let options = RasterOptions::from_form(&FormData {
            files: Vec::new(),
            fields: vec![
                ("dpi".to_string(), "600".to_string()),
                ("jpegQuality".to_string(), "95".to_string()),
            ],
        })
        .unwrap();

        assert_eq!(options.dpi, 600);
        assert_eq!(options.jpeg_quality, 95);
    }

    #[test]
    fn raster_options_reject_out_of_range_values() {
        let high_dpi = RasterOptions::from_form(&FormData {
            files: Vec::new(),
            fields: vec![("dpi".to_string(), "1200".to_string())],
        })
        .unwrap_err();
        let bad_quality = RasterOptions::from_form(&FormData {
            files: Vec::new(),
            fields: vec![("jpegQuality".to_string(), "0".to_string())],
        })
        .unwrap_err();

        assert_eq!(high_dpi.to_string(), "dpi must be between 72 and 600");
        assert_eq!(
            bad_quality.to_string(),
            "jpegQuality must be between 1 and 100"
        );
    }

    #[test]
    fn render_pixel_count_rejects_invalid_or_unrepresentable_pages() {
        assert_eq!(rendered_pixel_count(612.0, 792.0, 300).unwrap(), 8_415_000);
        assert!(rendered_pixel_count(f64::NAN, 792.0, 300).is_err());
        assert!(rendered_pixel_count(1.0e12, 1.0e12, 600).is_err());
    }

    #[test]
    fn aggregate_render_pixels_over_the_old_limit_are_accepted() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let document = sample_pdf_document(&pdfium, 4);
        let files = [PreparedPdf {
            document,
            page_count: 4,
        }];

        assert!(rendered_pixel_count(595.0, 842.0, 600).unwrap() * 4 > 120_000_000);
        validate_render_pages(&files, &[1, 2, 3, 4], 600).unwrap();
    }

    #[test]
    fn single_page_render_pixel_limit_is_still_enforced() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let mut document = pdfium.create_new_pdf().unwrap();
        document
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a3())
            .unwrap();
        let files = [PreparedPdf {
            document,
            page_count: 1,
        }];

        let error = validate_render_pages(&files, &[1], 600).unwrap_err();
        assert!(matches!(error, AppError::PayloadTooLarge(_)));
        assert!(error.to_string().contains("page 1 is too large"));
    }

    #[test]
    fn encoded_pages_still_enforce_the_download_limit() {
        let mut rendered = Vec::new();
        let mut output_bytes = 0;
        let error = append_encoded_pages(
            vec![RenderedImage {
                page: 1,
                zip_path: "page-0001.png".to_string(),
                bytes: vec![0; 2],
            }],
            &mut rendered,
            &mut output_bytes,
            0,
            Some(1),
            &None,
            ProgressRange { start: 35, end: 85 },
            1,
            (1, 1),
        )
        .unwrap_err();

        assert!(matches!(error, AppError::PayloadTooLarge(_)));
        assert_eq!(
            error.to_string(),
            "generated download exceeds the configured 1-byte limit"
        );
    }

    #[test]
    fn raster_batches_respect_worker_and_memory_limits() {
        assert!(!should_flush_raster_batch(
            0,
            0,
            MAX_BUFFERED_RASTER_BYTES + 1,
            4
        ));
        assert!(should_flush_raster_batch(
            1,
            MAX_BUFFERED_RASTER_BYTES,
            1,
            4
        ));
        assert!(should_flush_raster_batch(4, 0, 0, 4));
        assert!(!should_flush_raster_batch(
            3,
            MAX_BUFFERED_RASTER_BYTES - 1,
            1,
            4
        ));
    }

    #[test]
    fn render_progress_math_is_monotonic_and_bounded() {
        let updates = [
            render_percent(0, 0, 2),
            render_percent(0, 100, 2),
            render_percent(1, 0, 2),
            render_percent(1, 100, 2),
        ];

        assert_eq!(updates[0], 35);
        assert_eq!(updates[3], 85);
        assert!(updates.windows(2).all(|pair| pair[0] <= pair[1]));

        let range = ProgressRange { start: 40, end: 60 };
        assert_eq!(range.page_percent(0, 4), 40);
        assert_eq!(range.page_percent(2, 4), 50);
        assert_eq!(range.page_percent(4, 4), 60);
    }

    #[test]
    fn rendered_messages_include_file_position_only_for_batches() {
        assert_eq!(rendered_message(1, 2, (1, 1)), "Rendered 1 of 2 pages");
        assert_eq!(
            rendered_message(1, 2, (2, 3)),
            "Rendered 1 of 2 pages in PDF 2 of 3"
        );
    }

    #[test]
    fn render_pdf_pages_renders_selected_page_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };

        let rendered = render_pdf_pages(RenderPageJob {
            document: sample_pdf_document(&pdfium, 2),
            selected_pages: vec![2],
            max_pages: 10,
            format: RasterFormat::Png,
            options: RasterOptions::default(),
            progress: None,
            progress_range: ProgressRange { start: 35, end: 85 },
            file_position: (1, 1),
            max_download_bytes: None,
            prior_output_bytes: 0,
        })
        .unwrap();

        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].page, 2);
        assert!(rendered[0].bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        let image = image::load_from_memory(&rendered[0].bytes).unwrap();
        assert_eq!(image.width(), 2480);
        assert_eq!(image.height(), 3508);
    }

    #[test]
    fn render_pdf_pages_renders_jpeg_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };

        let rendered = render_pdf_pages(RenderPageJob {
            document: sample_pdf_document(&pdfium, 1),
            selected_pages: vec![1],
            max_pages: 10,
            format: RasterFormat::Jpeg,
            options: RasterOptions::default(),
            progress: None,
            progress_range: ProgressRange { start: 35, end: 85 },
            file_position: (1, 1),
            max_download_bytes: None,
            prior_output_bytes: 0,
        })
        .unwrap();

        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].bytes.starts_with(&[0xff, 0xd8]));
    }

    #[test]
    fn zip_images_stores_precompressed_images_by_page_number() {
        let zip = zip_images(vec![RenderedImage {
            page: 2,
            zip_path: "page-0002.png".to_string(),
            bytes: b"already-compressed".to_vec(),
        }])
        .unwrap();

        assert_eq!(u16::from_le_bytes([zip[8], zip[9]]), 0);
        assert!(zip
            .windows("page-0002.png".len())
            .any(|window| window == b"page-0002.png"));
    }

    #[test]
    fn zip_capacity_rejects_entry_and_total_overflow() {
        let entry_error = estimated_zip_capacity([usize::MAX]).unwrap_err();
        assert!(matches!(entry_error, AppError::PayloadTooLarge(_)));

        let total_error = estimated_zip_capacity([usize::MAX - 128, 1]).unwrap_err();
        assert!(matches!(total_error, AppError::PayloadTooLarge(_)));
    }

    #[test]
    fn zip_images_uses_flat_root_level_paths() {
        let images = vec![
            RenderedImage {
                page: 1,
                zip_path: "page-0001.png".to_string(),
                bytes: b"one".to_vec(),
            },
            RenderedImage {
                page: 2,
                zip_path: "page-0002.png".to_string(),
                bytes: b"two".to_vec(),
            },
        ];
        let zip = zip_images(images).unwrap();

        for path in ["page-0001.png", "page-0002.png"] {
            assert!(zip
                .windows(path.len())
                .any(|window| window == path.as_bytes()));
        }
        assert!(!zip
            .windows("page-0001.png".len() + 1)
            .any(|window| window.ends_with(b"/page-0001.png")));
    }

    #[test]
    fn zip_images_entries_use_current_modified_time() {
        let zip = zip_images(vec![RenderedImage {
            page: 1,
            zip_path: "page-0001.png".to_string(),
            bytes: b"one".to_vec(),
        }])
        .unwrap();
        let mut archive = ZipArchive::new(Cursor::new(zip)).unwrap();
        let entry = archive.by_index(0).unwrap();

        assert_ne!(entry.last_modified(), Some(DateTime::default()));
    }

    #[test]
    fn multi_pdf_render_uses_flat_global_zip_sequence_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };

        let rendered = render_pdf_files(
            pdfium.shared(),
            vec![
                UploadFile {
                    filename: "first.pdf".to_string(),
                    bytes: sample_pdf_bytes(&pdfium, 1).into(),
                },
                UploadFile {
                    filename: "second.pdf".to_string(),
                    bytes: sample_pdf_bytes(&pdfium, 1).into(),
                },
            ],
            PageSelection::All,
            RenderLimits::new(10, None),
            RasterFormat::Png,
            RasterOptions::default(),
            None,
        )
        .unwrap();

        let zip_paths = rendered
            .iter()
            .map(|image| image.zip_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(zip_paths, ["page-0001.png", "page-0002.png"]);
        assert!(zip_paths.iter().all(|path| !path.contains('/')));
    }

    #[test]
    fn selected_pages_for_file_maps_global_pages_to_local_pages() {
        assert_eq!(
            selected_pages_for_file(&[1, 2, 98, 99, 100, 101], 99, 0),
            [1, 2, 98, 99]
        );
        assert_eq!(
            selected_pages_for_file(&[1, 2, 98, 99, 100, 101], 99, 99),
            [1, 2]
        );
    }

    #[test]
    fn global_page_selection_enforces_request_limit_before_rendering() {
        let error =
            resolve_render_pages(PageSelection::Pages((1..=101).collect()), 198, 100).unwrap_err();

        assert_eq!(error.to_string(), "requested 101 pages; limit is 100");
    }

    #[test]
    fn all_pages_selection_reports_total_request_limit() {
        let error = resolve_render_pages(PageSelection::All, 120, 100).unwrap_err();

        assert_eq!(error.to_string(), "requested 120 pages; limit is 100");
    }

    #[test]
    fn multi_pdf_render_sequences_global_selected_pages_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };

        let rendered = render_pdf_files(
            pdfium.shared(),
            vec![
                UploadFile {
                    filename: "first.pdf".to_string(),
                    bytes: sample_pdf_bytes(&pdfium, 3).into(),
                },
                UploadFile {
                    filename: "second.pdf".to_string(),
                    bytes: sample_pdf_bytes(&pdfium, 3).into(),
                },
            ],
            PageSelection::Pages(vec![2, 3, 4, 5]),
            RenderLimits::new(10, None),
            RasterFormat::Png,
            RasterOptions::default(),
            None,
        )
        .unwrap();

        let zip_paths = rendered
            .iter()
            .map(|image| image.zip_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            zip_paths,
            [
                "page-0001.png",
                "page-0002.png",
                "page-0003.png",
                "page-0004.png"
            ]
        );
        assert_eq!(
            rendered.iter().map(|image| image.page).collect::<Vec<_>>(),
            [2, 3, 1, 2]
        );
        assert!(zip_paths.iter().all(|path| !path.contains('/')));
    }

    #[test]
    fn multi_pdf_render_enforces_limit_after_global_page_selection_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };

        let error = render_pdf_files(
            pdfium.shared(),
            vec![
                UploadFile {
                    filename: "first.pdf".to_string(),
                    bytes: sample_pdf_bytes(&pdfium, 99).into(),
                },
                UploadFile {
                    filename: "second.pdf".to_string(),
                    bytes: sample_pdf_bytes(&pdfium, 99).into(),
                },
            ],
            PageSelection::Pages((1..=101).collect()),
            RenderLimits::new(100, None),
            RasterFormat::Png,
            RasterOptions::default(),
            None,
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "requested 101 pages; limit is 100");
    }

    #[test]
    fn multi_pdf_render_all_pages_reports_total_request_limit_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };

        let error = render_pdf_files(
            pdfium.shared(),
            vec![
                UploadFile {
                    filename: "first.pdf".to_string(),
                    bytes: sample_pdf_bytes(&pdfium, 60).into(),
                },
                UploadFile {
                    filename: "second.pdf".to_string(),
                    bytes: sample_pdf_bytes(&pdfium, 60).into(),
                },
            ],
            PageSelection::All,
            RenderLimits::new(100, None),
            RasterFormat::Png,
            RasterOptions::default(),
            None,
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "requested 120 pages; limit is 100");
    }

    #[test]
    fn multi_pdf_render_progress_is_monotonic_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };

        let updates = Arc::new(Mutex::new(Vec::new()));
        let progress_updates = updates.clone();
        let progress: ProgressCallback = Arc::new(move |percent, _| {
            progress_updates.lock().unwrap().push(percent);
            false
        });

        render_pdf_files(
            pdfium.shared(),
            vec![
                UploadFile {
                    filename: "one.pdf".to_string(),
                    bytes: sample_pdf_bytes(&pdfium, 1).into(),
                },
                UploadFile {
                    filename: "two.pdf".to_string(),
                    bytes: sample_pdf_bytes(&pdfium, 1).into(),
                },
            ],
            PageSelection::All,
            RenderLimits::new(10, None),
            RasterFormat::Png,
            RasterOptions::default(),
            Some(progress),
        )
        .unwrap();

        let updates = updates.lock().unwrap();
        assert!(updates.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn preview_deadline_is_checked_between_native_page_calls() {
        let expired = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(1))
            .unwrap();

        let error = ensure_preview_deadline(expired).unwrap_err();
        assert!(matches!(error, AppError::RequestTimeout(_)));
    }
}
