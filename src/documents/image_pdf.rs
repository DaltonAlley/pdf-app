use image::{metadata::Orientation, DynamicImage, ImageDecoder};
use lopdf::{
    content::{Content, Operation},
    dictionary, Dictionary, Document, Object, Stream,
};
use rayon::prelude::*;
use std::sync::{Arc, Mutex};

use super::pdf_io;
use crate::{
    adapters::{BoundedBytes, FormData, UploadFile},
    error::{AppError, AppResult},
    progress::{report, ProgressCallback},
};

const PT_PER_IN: f32 = 72.0;
const IMAGE_DPI: f32 = 300.0;
const MAX_PARALLEL_IMAGE_PREP_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const MAX_SINGLE_IMAGE_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_SINGLE_IMAGE_WORKING_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ImagePdfOptions {
    page_size: ImagePdfPageSize,
    orientation: ImagePdfOrientation,
    fit: ImagePdfFit,
    margin_points: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImagePdfPageSize {
    Original,
    Letter,
    A4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImagePdfOrientation {
    Auto,
    Portrait,
    Landscape,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImagePdfFit {
    Contain,
    Cover,
}

impl Default for ImagePdfOptions {
    fn default() -> Self {
        Self {
            page_size: ImagePdfPageSize::Original,
            orientation: ImagePdfOrientation::Auto,
            fit: ImagePdfFit::Contain,
            margin_points: 0.0,
        }
    }
}

impl ImagePdfOptions {
    pub(crate) fn from_form(form: &FormData) -> AppResult<Self> {
        if form.field("layout").unwrap_or("single") != "single" {
            return Err(AppError::bad_request("layout must be single"));
        }
        let page_size = match form.field("pageSize").unwrap_or("original") {
            "original" => ImagePdfPageSize::Original,
            "letter" => ImagePdfPageSize::Letter,
            "a4" => ImagePdfPageSize::A4,
            _ => {
                return Err(AppError::bad_request(
                    "pageSize must be original, letter, or a4",
                ))
            }
        };
        let orientation = match form.field("orientation").unwrap_or("auto") {
            "auto" => ImagePdfOrientation::Auto,
            "portrait" => ImagePdfOrientation::Portrait,
            "landscape" => ImagePdfOrientation::Landscape,
            _ => {
                return Err(AppError::bad_request(
                    "orientation must be auto, portrait, or landscape",
                ))
            }
        };
        let fit = match form.field("imageFit").unwrap_or("contain") {
            "contain" => ImagePdfFit::Contain,
            "cover" => ImagePdfFit::Cover,
            _ => return Err(AppError::bad_request("imageFit must be contain or cover")),
        };
        let margin_points = form
            .field("marginPoints")
            .unwrap_or("0")
            .parse::<f32>()
            .map_err(|_| AppError::bad_request("marginPoints must be a number"))?;
        if !margin_points.is_finite() || margin_points < 0.0 {
            return Err(AppError::bad_request(
                "marginPoints must be a finite non-negative number",
            ));
        }
        if page_size == ImagePdfPageSize::Original
            && (orientation != ImagePdfOrientation::Auto
                || fit != ImagePdfFit::Contain
                || margin_points != 0.0)
        {
            return Err(AppError::bad_request(
                "orientation, imageFit, and marginPoints require pageSize letter or a4",
            ));
        }
        if let Some((width, height)) = page_size.base_dimensions() {
            if margin_points * 2.0 >= width.min(height) {
                return Err(AppError::bad_request(
                    "marginPoints leaves no printable area on the selected page size",
                ));
            }
        }
        Ok(Self {
            page_size,
            orientation,
            fit,
            margin_points,
        })
    }
}

impl ImagePdfPageSize {
    fn base_dimensions(self) -> Option<(f32, f32)> {
        match self {
            Self::Original => None,
            Self::Letter => Some((612.0, 792.0)),
            Self::A4 => Some((595.275_6, 841.889_8)),
        }
    }
}

pub(crate) fn images_to_pdf(
    files: Vec<UploadFile>,
    max_download_bytes: Option<usize>,
    progress: Option<ProgressCallback>,
) -> AppResult<Vec<u8>> {
    images_to_pdf_with_options(
        files,
        max_download_bytes,
        progress,
        ImagePdfOptions::default(),
    )
}

pub(crate) fn images_to_pdf_with_options(
    files: Vec<UploadFile>,
    max_download_bytes: Option<usize>,
    progress: Option<ProgressCallback>,
    options: ImagePdfOptions,
) -> AppResult<Vec<u8>> {
    let total = files.len();
    images_to_pdf_with_options_and_progress(
        files,
        max_download_bytes,
        progress,
        ImageProgressPlan::new(30, 90, 0, total),
        options,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImageProgressPlan {
    start: u8,
    end: u8,
    completed_before: usize,
    total: usize,
}

impl ImageProgressPlan {
    pub(crate) fn new(start: u8, end: u8, completed_before: usize, total: usize) -> Self {
        Self {
            start,
            end,
            completed_before,
            total,
        }
    }

    fn percent(self, units_completed_in_batch: usize) -> u8 {
        let completed_units = self
            .completed_before
            .saturating_mul(2)
            .saturating_add(units_completed_in_batch);
        let total_units = self.total.saturating_mul(2).max(1);
        let span = usize::from(self.end.saturating_sub(self.start));
        self.start
            .saturating_add(((completed_units.min(total_units) * span) / total_units) as u8)
    }
}

pub(crate) fn images_to_pdf_with_progress(
    files: Vec<UploadFile>,
    max_download_bytes: Option<usize>,
    progress: Option<ProgressCallback>,
    progress_plan: ImageProgressPlan,
) -> AppResult<Vec<u8>> {
    images_to_pdf_with_options_and_progress(
        files,
        max_download_bytes,
        progress,
        progress_plan,
        ImagePdfOptions::default(),
    )
}

fn images_to_pdf_with_options_and_progress(
    files: Vec<UploadFile>,
    max_download_bytes: Option<usize>,
    progress: Option<ProgressCallback>,
    progress_plan: ImageProgressPlan,
    options: ImagePdfOptions,
) -> AppResult<Vec<u8>> {
    let file_count = files.len();
    let prepared = prepare_images_bounded(files, &progress, progress_plan)?;
    let mut document = Document::with_version("1.5");
    let pages_id = document.new_object_id();
    let mut page_ids = Vec::with_capacity(file_count);

    for (index, image) in prepared.into_iter().enumerate() {
        let filename = image.filename.clone();
        let width = image.width;
        let height = image.height;
        let image_id = add_prepared_image(&mut document, image);
        let image_name = format!("Image{}", index + 1);
        let geometry = image_page_geometry(width, height, options);
        let mut operations = vec![Operation::new("q", Vec::new())];
        if geometry.clip_to_printable_area {
            operations.extend([
                Operation::new(
                    "re",
                    vec![
                        options.margin_points.into(),
                        options.margin_points.into(),
                        geometry.printable_width.into(),
                        geometry.printable_height.into(),
                    ],
                ),
                Operation::new("W", Vec::new()),
                Operation::new("n", Vec::new()),
            ]);
        }
        operations.extend([
            Operation::new(
                "cm",
                vec![
                    geometry.image_width.into(),
                    0.into(),
                    0.into(),
                    geometry.image_height.into(),
                    geometry.image_x.into(),
                    geometry.image_y.into(),
                ],
            ),
            Operation::new("Do", vec![Object::Name(image_name.as_bytes().to_vec())]),
            Operation::new("Q", Vec::new()),
        ]);
        let content = Content { operations };
        let content_bytes = content
            .encode()
            .map_err(|err| AppError::internal_cause("could not build image PDF content", err))?;
        let content_id = document.add_object(Stream::new(dictionary! {}, content_bytes));

        let mut xobjects = Dictionary::new();
        xobjects.set(image_name.as_bytes(), image_id);
        let page_box = Object::Array(vec![
            0.into(),
            0.into(),
            Object::Real(geometry.page_width),
            Object::Real(geometry.page_height),
        ]);
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => dictionary! { "XObject" => xobjects },
            "MediaBox" => page_box.clone(),
            "CropBox" => page_box.clone(),
            "TrimBox" => page_box,
        });
        page_ids.push(page_id);
        let completed = index + 1;
        report(
            &progress,
            progress_plan.percent(file_count.saturating_add(completed)),
            format!(
                "Built image PDF page {} of {}: `{filename}`",
                progress_plan.completed_before.saturating_add(completed),
                progress_plan.total
            ),
        )?;
    }

    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Count" => page_ids.len() as i64,
            "Kids" => page_ids.into_iter().map(Object::Reference).collect::<Vec<_>>(),
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    document.compress();

    let mut output = BoundedBytes::new(
        pdf_io::estimated_lopdf_output_capacity(&document),
        max_download_bytes,
    );
    let result = document.save_to(&mut output);
    output.finish(result, "could not save image PDF")
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ImagePageGeometry {
    page_width: f32,
    page_height: f32,
    printable_width: f32,
    printable_height: f32,
    image_width: f32,
    image_height: f32,
    image_x: f32,
    image_y: f32,
    clip_to_printable_area: bool,
}

fn image_page_geometry(width: usize, height: usize, options: ImagePdfOptions) -> ImagePageGeometry {
    let original_width = px_to_pt(width);
    let original_height = px_to_pt(height);
    let Some((base_width, base_height)) = options.page_size.base_dimensions() else {
        return ImagePageGeometry {
            page_width: original_width,
            page_height: original_height,
            printable_width: original_width,
            printable_height: original_height,
            image_width: original_width,
            image_height: original_height,
            image_x: 0.0,
            image_y: 0.0,
            clip_to_printable_area: false,
        };
    };
    let landscape = match options.orientation {
        ImagePdfOrientation::Auto => width > height,
        ImagePdfOrientation::Portrait => false,
        ImagePdfOrientation::Landscape => true,
    };
    let (page_width, page_height) = if landscape {
        (base_height, base_width)
    } else {
        (base_width, base_height)
    };
    let printable_width = page_width - options.margin_points * 2.0;
    let printable_height = page_height - options.margin_points * 2.0;
    let width_scale = printable_width / original_width;
    let height_scale = printable_height / original_height;
    let scale = match options.fit {
        ImagePdfFit::Contain => width_scale.min(height_scale),
        ImagePdfFit::Cover => width_scale.max(height_scale),
    };
    let image_width = original_width * scale;
    let image_height = original_height * scale;
    ImagePageGeometry {
        page_width,
        page_height,
        printable_width,
        printable_height,
        image_width,
        image_height,
        image_x: options.margin_points + (printable_width - image_width) / 2.0,
        image_y: options.margin_points + (printable_height - image_height) / 2.0,
        clip_to_printable_area: options.fit == ImagePdfFit::Cover,
    }
}

#[derive(Debug)]
struct PreparedImage {
    filename: String,
    width: usize,
    height: usize,
    stream: Stream,
    alpha: Option<Stream>,
}

#[derive(Debug)]
struct ImagePreflight {
    file: UploadFile,
    format: SupportedImageFormat,
    working_bytes: usize,
    orientation: Orientation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportedImageFormat {
    Jpeg,
    Png,
}

fn prepare_images_bounded(
    files: Vec<UploadFile>,
    progress: &Option<ProgressCallback>,
    progress_plan: ImageProgressPlan,
) -> AppResult<Vec<PreparedImage>> {
    let preflight = files
        .into_iter()
        .map(preflight_image)
        .collect::<AppResult<Vec<_>>>()?;
    let mut prepared = Vec::with_capacity(preflight.len());
    let mut batch = Vec::new();
    let mut batch_bytes = 0usize;
    let max_batch_len = rayon::current_num_threads().max(1);
    let completed = Arc::new(Mutex::new(0_usize));

    for image in preflight {
        if should_flush_image_batch(batch.len(), batch_bytes, image.working_bytes, max_batch_len) {
            prepare_image_batch(
                batch,
                progress,
                progress_plan,
                completed.clone(),
                &mut prepared,
            )?;
            batch = Vec::new();
            batch_bytes = 0;
        }
        batch_bytes = batch_bytes.saturating_add(image.working_bytes);
        batch.push(image);
    }
    prepare_image_batch(batch, progress, progress_plan, completed, &mut prepared)?;
    Ok(prepared)
}

fn should_flush_image_batch(
    batch_len: usize,
    batch_bytes: usize,
    next_image_bytes: usize,
    max_batch_len: usize,
) -> bool {
    batch_len > 0
        && (batch_len >= max_batch_len
            || batch_bytes.saturating_add(next_image_bytes) > MAX_PARALLEL_IMAGE_PREP_BYTES)
}

fn prepare_image_batch(
    batch: Vec<ImagePreflight>,
    progress: &Option<ProgressCallback>,
    progress_plan: ImageProgressPlan,
    completed: Arc<Mutex<usize>>,
    prepared: &mut Vec<PreparedImage>,
) -> AppResult<()> {
    let mut encoded = batch
        .into_par_iter()
        .map(|image| {
            let filename = image.file.filename.clone();
            let prepared = prepare_image(image)?;
            let mut completed = completed.lock().map_err(|_| {
                AppError::Internal("image progress counter is unavailable".to_string())
            })?;
            *completed = completed.saturating_add(1);
            let batch_completed = *completed;
            report(
                progress,
                progress_plan.percent(batch_completed),
                format!(
                    "Converted image {} of {}: `{filename}`",
                    progress_plan
                        .completed_before
                        .saturating_add(batch_completed),
                    progress_plan.total
                ),
            )?;
            Ok(prepared)
        })
        .collect::<AppResult<Vec<_>>>()?;
    prepared.append(&mut encoded);
    Ok(())
}

fn preflight_image(file: UploadFile) -> AppResult<ImagePreflight> {
    validate_image_input_size(&file.filename, file.bytes.len())?;
    let format = match image::guess_format(&file.bytes) {
        Ok(image::ImageFormat::Jpeg) => SupportedImageFormat::Jpeg,
        Ok(image::ImageFormat::Png) => SupportedImageFormat::Png,
        _ => Err(AppError::bad_request(format!(
            "could not decode `{}` for PDF: unsupported image format",
            file.filename
        )))?,
    };
    let (working_bytes, orientation) = match format {
        SupportedImageFormat::Png => {
            let mut decoder = image::ImageReader::with_format(
                std::io::Cursor::new(&file.bytes),
                image::ImageFormat::Png,
            )
            .into_decoder()
            .map_err(|err| {
                AppError::bad_request(format!("could not read `{}`: {err}", file.filename))
            })?;
            let (width, height) = decoder.dimensions();
            let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
            (
                checked_image_working_bytes(&file.filename, width, height, 16)?,
                orientation,
            )
        }
        SupportedImageFormat::Jpeg => {
            let mut decoder = image::ImageReader::with_format(
                std::io::Cursor::new(&file.bytes),
                image::ImageFormat::Jpeg,
            )
            .into_decoder()
            .map_err(|err| {
                AppError::bad_request(format!("could not read `{}`: {err}", file.filename))
            })?;
            let (width, height) = decoder.dimensions();
            let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
            let needs_reencode =
                orientation != Orientation::NoTransforms || jpeg_component_count(&file.bytes)? == 4;
            let working_bytes = if needs_reencode {
                checked_image_working_bytes(&file.filename, width, height, 12)?
            } else {
                0
            };
            (working_bytes, orientation)
        }
    };
    Ok(ImagePreflight {
        file,
        format,
        working_bytes,
        orientation,
    })
}

pub(crate) fn validate_image_input_size(filename: &str, byte_len: usize) -> AppResult<()> {
    if byte_len > MAX_SINGLE_IMAGE_INPUT_BYTES {
        return Err(AppError::payload_too_large(format!(
            "image `{filename}` exceeds the {} MiB compressed input limit",
            MAX_SINGLE_IMAGE_INPUT_BYTES / (1024 * 1024)
        )));
    }
    Ok(())
}

fn checked_image_working_bytes(
    filename: &str,
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
) -> AppResult<usize> {
    let working_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
        .ok_or_else(|| {
            AppError::payload_too_large(format!(
                "image `{filename}` dimensions {width} × {height} require an unrepresentable working buffer"
            ))
        })?;
    if working_bytes > MAX_SINGLE_IMAGE_WORKING_BYTES {
        return Err(AppError::payload_too_large(format!(
            "image `{filename}` dimensions {width} × {height} require an estimated {} MiB working buffer; the limit is {} MiB",
            working_bytes / (1024 * 1024),
            MAX_SINGLE_IMAGE_WORKING_BYTES / (1024 * 1024)
        )));
    }
    Ok(working_bytes)
}

fn prepare_image(image: ImagePreflight) -> AppResult<PreparedImage> {
    match image.format {
        SupportedImageFormat::Jpeg => prepare_jpeg(image.file, image.orientation),
        SupportedImageFormat::Png => prepare_png(image.file, image.orientation),
    }
}

fn prepare_jpeg(file: UploadFile, orientation: Orientation) -> AppResult<PreparedImage> {
    if jpeg_component_count(&file.bytes)? == 4 || orientation != Orientation::NoTransforms {
        return prepare_jpeg(
            reencode_jpeg_as_rgb(file, orientation)?,
            Orientation::NoTransforms,
        );
    }
    let decoder = image::ImageReader::with_format(
        std::io::Cursor::new(&file.bytes),
        image::ImageFormat::Jpeg,
    )
    .into_decoder()
    .map_err(|err| AppError::bad_request(format!("could not read `{}`: {err}", file.filename)))?;
    let (width, height) = decoder.dimensions();
    let color_space = match decoder.color_type() {
        image::ColorType::L8 | image::ColorType::L16 => "DeviceGray",
        _ => "DeviceRGB",
    };
    drop(decoder);
    let UploadFile { filename, bytes } = file;
    let stream = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => i64::from(width),
            "Height" => i64::from(height),
            "ColorSpace" => color_space,
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
        },
        bytes.into(),
    );
    Ok(PreparedImage {
        filename,
        width: width as usize,
        height: height as usize,
        stream,
        alpha: None,
    })
}

fn reencode_jpeg_as_rgb(file: UploadFile, orientation: Orientation) -> AppResult<UploadFile> {
    let mut image = image::load_from_memory_with_format(&file.bytes, image::ImageFormat::Jpeg)
        .map_err(|error| {
            AppError::bad_request(format!(
                "could not decode JPEG `{}` for PDF: {error}",
                file.filename
            ))
        })?;
    image.apply_orientation(orientation);
    let image = image.into_rgb8();
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90)
        .encode(
            &image,
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|error| {
            AppError::internal_cause(
                format!("could not prepare JPEG `{}` for PDF", file.filename),
                error,
            )
        })?;
    Ok(UploadFile {
        filename: file.filename,
        bytes: bytes.into(),
    })
}

fn jpeg_component_count(bytes: &[u8]) -> AppResult<u8> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err(AppError::bad_request("JPEG has an invalid header"));
    }
    let mut index = 2usize;
    while index < bytes.len() {
        while bytes.get(index) == Some(&0xff) {
            index += 1;
        }
        let marker = *bytes
            .get(index)
            .ok_or_else(|| AppError::bad_request("JPEG has a truncated marker"))?;
        index += 1;
        if matches!(marker, 0xd8 | 0xd9) || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if marker == 0xda {
            break;
        }
        let length_bytes = bytes
            .get(index..index + 2)
            .ok_or_else(|| AppError::bad_request("JPEG has a truncated segment"))?;
        let segment_len = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if segment_len < 2 {
            return Err(AppError::bad_request("JPEG has an invalid segment length"));
        }
        let segment_end = index
            .checked_add(segment_len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| AppError::bad_request("JPEG has a truncated segment"))?;
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            let component_count = *bytes
                .get(index + 7)
                .ok_or_else(|| AppError::bad_request("JPEG has a truncated frame header"))?;
            let expected_len = 8usize
                .checked_add(usize::from(component_count).saturating_mul(3))
                .ok_or_else(|| AppError::bad_request("JPEG frame header is too large"))?;
            if component_count == 0 || segment_len < expected_len {
                return Err(AppError::bad_request(
                    "JPEG has an invalid frame component count",
                ));
            }
            return Ok(component_count);
        }
        index = segment_end;
    }
    Err(AppError::bad_request("JPEG has no image frame"))
}

fn prepare_png(file: UploadFile, orientation: Orientation) -> AppResult<PreparedImage> {
    let mut decoded = image::load_from_memory_with_format(&file.bytes, image::ImageFormat::Png)
        .map_err(|err| {
            AppError::bad_request(format!(
                "could not decode `{}` for PDF: {err}",
                file.filename
            ))
        })?;
    decoded.apply_orientation(orientation);
    let width = decoded.width() as usize;
    let height = decoded.height() as usize;
    let (pixels, alpha, color_space, bits_per_component) = split_png_channels(decoded);
    let dictionary = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => width as i64,
        "Height" => height as i64,
        "ColorSpace" => color_space,
        "BitsPerComponent" => bits_per_component,
    };

    let alpha = alpha
        .map(|alpha| {
            let mut alpha_stream = Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => width as i64,
                    "Height" => height as i64,
                    "ColorSpace" => "DeviceGray",
                    "BitsPerComponent" => bits_per_component,
                },
                alpha,
            );
            alpha_stream.compress().map_err(|err| {
                AppError::internal_cause(
                    format!("could not compress `{}` alpha", file.filename),
                    err,
                )
            })?;
            Ok::<_, AppError>(alpha_stream)
        })
        .transpose()?;

    let mut stream = Stream::new(dictionary, pixels);
    stream.compress().map_err(|err| {
        AppError::internal_cause(format!("could not compress `{}`", file.filename), err)
    })?;
    Ok(PreparedImage {
        filename: file.filename,
        width,
        height,
        stream,
        alpha,
    })
}

fn add_prepared_image(document: &mut Document, mut image: PreparedImage) -> lopdf::ObjectId {
    if let Some(alpha) = image.alpha {
        image.stream.dict.set("SMask", document.add_object(alpha));
    }
    document.add_object(image.stream)
}

fn split_png_channels(image: DynamicImage) -> (Vec<u8>, Option<Vec<u8>>, &'static str, i64) {
    match image {
        DynamicImage::ImageLuma8(image) => (image.into_raw(), None, "DeviceGray", 8),
        DynamicImage::ImageLumaA8(image) => {
            let mut pixels = Vec::with_capacity(image.len() / 2);
            let mut alpha = Vec::with_capacity(image.len() / 2);
            for pair in image.into_raw().chunks_exact(2) {
                if let [pixel, alpha_value] = pair {
                    pixels.push(*pixel);
                    alpha.push(*alpha_value);
                }
            }
            (pixels, Some(alpha), "DeviceGray", 8)
        }
        DynamicImage::ImageRgb8(image) => (image.into_raw(), None, "DeviceRGB", 8),
        DynamicImage::ImageRgba8(image) => {
            let mut pixels = Vec::with_capacity(image.len() / 4 * 3);
            let mut alpha = Vec::with_capacity(image.len() / 4);
            for rgba in image.into_raw().chunks_exact(4) {
                if let [red, green, blue, alpha_value] = rgba {
                    pixels.extend_from_slice(&[*red, *green, *blue]);
                    alpha.push(*alpha_value);
                }
            }
            (pixels, Some(alpha), "DeviceRGB", 8)
        }
        DynamicImage::ImageLuma16(image) => {
            (u16s_to_be_bytes(image.into_raw()), None, "DeviceGray", 16)
        }
        DynamicImage::ImageLumaA16(image) => {
            let raw = image.into_raw();
            let mut pixels = Vec::with_capacity(raw.len());
            let mut alpha = Vec::with_capacity(raw.len());
            for pair in raw.chunks_exact(2) {
                if let [pixel, alpha_value] = pair {
                    pixels.extend_from_slice(&pixel.to_be_bytes());
                    alpha.extend_from_slice(&alpha_value.to_be_bytes());
                }
            }
            (pixels, Some(alpha), "DeviceGray", 16)
        }
        DynamicImage::ImageRgb16(image) => {
            (u16s_to_be_bytes(image.into_raw()), None, "DeviceRGB", 16)
        }
        DynamicImage::ImageRgba16(image) => {
            let raw = image.into_raw();
            let mut pixels = Vec::with_capacity(raw.len() / 4 * 6);
            let mut alpha = Vec::with_capacity(raw.len() / 4 * 2);
            for rgba in raw.chunks_exact(4) {
                if let [red, green, blue, alpha_value] = rgba {
                    pixels.extend_from_slice(&red.to_be_bytes());
                    pixels.extend_from_slice(&green.to_be_bytes());
                    pixels.extend_from_slice(&blue.to_be_bytes());
                    alpha.extend_from_slice(&alpha_value.to_be_bytes());
                }
            }
            (pixels, Some(alpha), "DeviceRGB", 16)
        }
        other => (other.to_rgb8().into_raw(), None, "DeviceRGB", 8),
    }
}

fn u16s_to_be_bytes(values: Vec<u16>) -> Vec<u8> {
    values
        .into_iter()
        .flat_map(u16::to_be_bytes)
        .collect::<Vec<_>>()
}

fn px_to_pt(px: usize) -> f32 {
    px as f32 * PT_PER_IN / IMAGE_DPI
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdfium_or_skip() -> Option<crate::adapters::TestPdfium> {
        let pdfium = crate::test_pdfium();
        if pdfium.is_none() {
            eprintln!("skipping PDFium test: PDF_TOOLS_PDFIUM_PATH is not set");
        }
        pdfium
    }

    fn sample_png_bytes_with_size(width: u32, height: u32) -> Vec<u8> {
        let image = image::ImageBuffer::from_fn(width, height, |x, y| {
            if (x + y) % 2 == 0 {
                image::Rgba([220, 30, 30, 255])
            } else {
                image::Rgba([30, 90, 220, 255])
            }
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    fn sample_png_bytes_with_orientation(
        width: u32,
        height: u32,
        orientation: Orientation,
    ) -> Vec<u8> {
        use image::ImageEncoder as _;

        let image = image::ImageBuffer::from_fn(width, height, |x, y| {
            image::Rgba([
                (x * 50) as u8,
                (y * 70) as u8,
                120,
                40 + (x * 20 + y * 30) as u8,
            ])
        });
        let exif = vec![
            b'I',
            b'I',
            0x2a,
            0,
            8,
            0,
            0,
            0,
            1,
            0,
            0x12,
            0x01,
            3,
            0,
            1,
            0,
            0,
            0,
            orientation.to_exif(),
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        encoder.set_exif_metadata(exif).unwrap();
        encoder
            .write_image(
                image.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        bytes
    }

    fn sample_jpeg_bytes_with_orientation(
        width: u32,
        height: u32,
        orientation: Orientation,
    ) -> Vec<u8> {
        let image = image::ImageBuffer::from_fn(width, height, |x, y| {
            image::Rgb([(x * 20) as u8, (y * 30) as u8, 120])
        });
        let mut jpeg = Vec::new();
        DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let mut exif = vec![
            0xff,
            0xe1,
            0x00,
            0x22,
            b'E',
            b'x',
            b'i',
            b'f',
            0,
            0,
            b'I',
            b'I',
            0x2a,
            0,
            8,
            0,
            0,
            0,
            1,
            0,
            0x12,
            0x01,
            3,
            0,
            1,
            0,
            0,
            0,
            orientation.to_exif(),
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        exif.extend_from_slice(&jpeg[2..]);
        jpeg.truncate(2);
        jpeg.extend(exif);
        jpeg
    }

    fn assert_points(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.01,
            "expected {expected} pt, got {actual} pt"
        );
    }

    #[test]
    fn images_to_pdf_creates_one_original_size_page_per_image_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };

        let bytes = images_to_pdf(
            vec![
                UploadFile {
                    filename: "wide.png".to_string(),
                    bytes: sample_png_bytes_with_size(20, 10).into(),
                },
                UploadFile {
                    filename: "tall.png".to_string(),
                    bytes: sample_png_bytes_with_size(10, 30).into(),
                },
            ],
            None,
            None,
        )
        .unwrap();

        let document = pdfium.load_pdf_from_byte_vec(bytes, None).unwrap();

        assert_eq!(document.pages().len(), 2);
        let first = document.pages().get(0).unwrap();
        let second = document.pages().get(1).unwrap();
        assert_points(first.width().value, px_to_pt(20));
        assert_points(first.height().value, px_to_pt(10));
        assert_points(second.width().value, px_to_pt(10));
        assert_points(second.height().value, px_to_pt(30));
    }

    #[test]
    fn image_pdf_options_accept_missing_and_single_layout() {
        assert_eq!(
            ImagePdfOptions::from_form(&FormData::default()).unwrap(),
            ImagePdfOptions::default()
        );
        assert!(ImagePdfOptions::from_form(&FormData {
            files: Vec::new(),
            fields: vec![("layout".to_string(), "single".to_string())],
        })
        .is_ok());
    }

    #[test]
    fn image_pdf_options_reject_unsupported_layout() {
        let form = FormData {
            files: Vec::new(),
            fields: vec![("layout".to_string(), "grid".to_string())],
        };

        assert_eq!(
            ImagePdfOptions::from_form(&form).unwrap_err().to_string(),
            "layout must be single"
        );
    }

    #[test]
    fn image_pdf_options_parse_fixed_page_geometry() {
        let options = ImagePdfOptions::from_form(&FormData {
            files: Vec::new(),
            fields: vec![
                ("pageSize".to_string(), "letter".to_string()),
                ("orientation".to_string(), "landscape".to_string()),
                ("imageFit".to_string(), "cover".to_string()),
                ("marginPoints".to_string(), "18".to_string()),
            ],
        })
        .unwrap();

        assert_eq!(options.page_size, ImagePdfPageSize::Letter);
        assert_eq!(options.orientation, ImagePdfOrientation::Landscape);
        assert_eq!(options.fit, ImagePdfFit::Cover);
        assert_eq!(options.margin_points, 18.0);
    }

    #[test]
    fn image_pdf_options_reject_ignored_and_impossible_geometry() {
        for fields in [
            vec![("orientation".to_string(), "portrait".to_string())],
            vec![
                ("pageSize".to_string(), "a4".to_string()),
                ("marginPoints".to_string(), "300".to_string()),
            ],
            vec![
                ("pageSize".to_string(), "letter".to_string()),
                ("marginPoints".to_string(), "NaN".to_string()),
            ],
        ] {
            assert!(ImagePdfOptions::from_form(&FormData {
                files: Vec::new(),
                fields,
            })
            .is_err());
        }
    }

    #[test]
    fn fixed_page_geometry_is_predictable_for_contain_and_cover() {
        let contain = image_page_geometry(
            1_000,
            500,
            ImagePdfOptions {
                page_size: ImagePdfPageSize::Letter,
                orientation: ImagePdfOrientation::Portrait,
                fit: ImagePdfFit::Contain,
                margin_points: 36.0,
            },
        );
        assert_points(contain.page_width, 612.0);
        assert_points(contain.page_height, 792.0);
        assert_points(contain.printable_width, 540.0);
        assert_points(contain.printable_height, 720.0);
        assert_points(contain.image_width, 540.0);
        assert_points(contain.image_height, 270.0);
        assert_points(contain.image_x, 36.0);
        assert_points(contain.image_y, 261.0);
        assert!(!contain.clip_to_printable_area);

        let cover = image_page_geometry(
            1_000,
            500,
            ImagePdfOptions {
                fit: ImagePdfFit::Cover,
                orientation: ImagePdfOrientation::Portrait,
                ..ImagePdfOptions::from_form(&FormData {
                    files: Vec::new(),
                    fields: vec![("pageSize".to_string(), "letter".to_string())],
                })
                .unwrap()
            },
        );
        assert_points(cover.image_height, 792.0);
        assert_points(cover.image_width, 1_584.0);
        assert_points(cover.image_x, -486.0);
        assert!(cover.clip_to_printable_area);
    }

    #[test]
    fn fixed_page_pdf_uses_requested_page_size_when_pdfium_is_available() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let options = ImagePdfOptions::from_form(&FormData {
            files: Vec::new(),
            fields: vec![
                ("pageSize".to_string(), "a4".to_string()),
                ("orientation".to_string(), "landscape".to_string()),
                ("marginPoints".to_string(), "12".to_string()),
            ],
        })
        .unwrap();
        let bytes = images_to_pdf_with_options(
            vec![UploadFile {
                filename: "art.png".to_string(),
                bytes: sample_png_bytes_with_size(20, 10).into(),
            }],
            None,
            None,
            options,
        )
        .unwrap();
        let document = pdfium.load_pdf_from_byte_vec(bytes, None).unwrap();
        let page = document.pages().get(0).unwrap();
        assert_points(page.width().value, 841.889_8);
        assert_points(page.height().value, 595.275_6);
    }

    #[test]
    fn image_dimension_errors_include_filename() {
        let error = preflight_image(UploadFile {
            filename: "notes.txt".to_string(),
            bytes: b"not an image".to_vec().into(),
        })
        .unwrap_err();

        assert!(error.to_string().contains("notes.txt"));
    }

    #[test]
    fn image_preflight_rejects_formats_outside_the_supported_enum() {
        let gif = UploadFile {
            filename: "animation.gif".to_string(),
            bytes: b"GIF89a".to_vec().into(),
        };

        let error = preflight_image(gif).unwrap_err();

        assert!(error.to_string().contains("unsupported image format"));
    }

    #[test]
    fn image_preparation_batches_respect_worker_and_memory_limits() {
        assert!(!should_flush_image_batch(
            0,
            0,
            MAX_PARALLEL_IMAGE_PREP_BYTES + 1,
            4
        ));
        assert!(should_flush_image_batch(
            1,
            MAX_PARALLEL_IMAGE_PREP_BYTES,
            1,
            4
        ));
        assert!(should_flush_image_batch(4, 0, 0, 4));
        assert!(!should_flush_image_batch(
            3,
            MAX_PARALLEL_IMAGE_PREP_BYTES - 1,
            1,
            4
        ));
    }

    #[test]
    fn staged_image_progress_is_global_monotonic_and_names_completed_files() {
        let events = Arc::new(Mutex::new(Vec::<(u8, String)>::new()));
        let captured = events.clone();
        let progress: ProgressCallback = Arc::new(move |percent, stage| {
            captured.lock().unwrap().push((percent, stage));
            false
        });
        let files = ["fifth.png", "sixth.png", "seventh.png"]
            .into_iter()
            .map(|filename| UploadFile {
                filename: filename.to_string(),
                bytes: sample_png_bytes_with_size(2, 2).into(),
            })
            .collect();

        images_to_pdf_with_progress(
            files,
            None,
            Some(progress),
            ImageProgressPlan::new(22, 60, 4, 10),
        )
        .unwrap();

        let events = events.lock().unwrap();
        assert!(events.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        let conversion = events
            .iter()
            .filter(|(_, stage)| stage.starts_with("Converted image"))
            .collect::<Vec<_>>();
        assert_eq!(conversion.len(), 3);
        for count in 5..=7 {
            assert!(conversion
                .iter()
                .any(|(_, stage)| stage.starts_with(&format!("Converted image {count} of 10:"))));
        }
        for filename in ["fifth.png", "sixth.png", "seventh.png"] {
            assert_eq!(
                conversion
                    .iter()
                    .filter(|(_, stage)| stage.ends_with(&format!("`{filename}`")))
                    .count(),
                1
            );
        }
        assert_eq!(events.last().map(|event| event.0), Some(48));
    }

    #[test]
    fn jpeg_exif_orientation_is_applied_before_pdf_embedding() {
        let preflight = preflight_image(UploadFile {
            filename: "phone-photo.jpg".to_string(),
            bytes: sample_jpeg_bytes_with_orientation(8, 5, Orientation::Rotate90).into(),
        })
        .unwrap();

        assert_eq!(preflight.orientation, Orientation::Rotate90);
        assert_eq!(preflight.working_bytes, 8 * 5 * 12);

        let prepared = prepare_image(preflight).unwrap();
        assert_eq!((prepared.width, prepared.height), (5, 8));
        let normalized =
            image::load_from_memory_with_format(&prepared.stream.content, image::ImageFormat::Jpeg)
                .unwrap();
        assert_eq!((normalized.width(), normalized.height()), (5, 8));
    }

    #[test]
    fn png_exif_orientation_is_applied_to_pixels_and_alpha_before_embedding() {
        let bytes = sample_png_bytes_with_orientation(3, 2, Orientation::Rotate90);
        let mut expected =
            image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).unwrap();
        expected.apply_orientation(Orientation::Rotate90);
        let expected = expected.into_rgba8();
        let expected_rgb = expected
            .pixels()
            .flat_map(|pixel| pixel.0[..3].iter().copied())
            .collect::<Vec<_>>();
        let expected_alpha = expected
            .pixels()
            .map(|pixel| pixel.0[3])
            .collect::<Vec<_>>();
        let preflight = preflight_image(UploadFile {
            filename: "phone-artwork.png".to_string(),
            bytes: bytes.into(),
        })
        .unwrap();

        assert_eq!(preflight.orientation, Orientation::Rotate90);
        assert_eq!(preflight.working_bytes, 3 * 2 * 16);

        let prepared = prepare_image(preflight).unwrap();
        assert_eq!((prepared.width, prepared.height), (2, 3));
        assert_eq!(
            prepared.stream.decompressed_content().unwrap(),
            expected_rgb
        );
        assert_eq!(
            prepared
                .alpha
                .as_ref()
                .unwrap()
                .decompressed_content()
                .unwrap(),
            expected_alpha
        );
    }

    #[test]
    fn png_without_orientation_preserves_dimensions_pixels_and_alpha() {
        let bytes = sample_png_bytes_with_orientation(3, 2, Orientation::NoTransforms);
        let expected =
            image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).unwrap();
        let expected = expected.into_rgba8();
        let expected_rgb = expected
            .pixels()
            .flat_map(|pixel| pixel.0[..3].iter().copied())
            .collect::<Vec<_>>();
        let expected_alpha = expected
            .pixels()
            .map(|pixel| pixel.0[3])
            .collect::<Vec<_>>();
        let preflight = preflight_image(UploadFile {
            filename: "ordinary.png".to_string(),
            bytes: bytes.into(),
        })
        .unwrap();

        assert_eq!(preflight.orientation, Orientation::NoTransforms);
        assert_eq!(preflight.working_bytes, 3 * 2 * 16);

        let prepared = prepare_image(preflight).unwrap();
        assert_eq!((prepared.width, prepared.height), (3, 2));
        assert_eq!(
            prepared.stream.decompressed_content().unwrap(),
            expected_rgb
        );
        assert_eq!(
            prepared
                .alpha
                .as_ref()
                .unwrap()
                .decompressed_content()
                .unwrap(),
            expected_alpha
        );
    }

    #[test]
    fn jpeg_bytes_are_embedded_without_lossy_reencoding() {
        let image = image::ImageBuffer::from_fn(8, 5, |x, y| {
            image::Rgb([(x * 20) as u8, (y * 30) as u8, 120])
        });
        let mut jpeg = Vec::new();
        DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let preflight = preflight_image(UploadFile {
            filename: "photo.jpg".to_string(),
            bytes: jpeg.clone().into(),
        })
        .unwrap();
        assert_eq!(preflight.orientation, Orientation::NoTransforms);
        assert_eq!(preflight.working_bytes, 0);
        let prepared = prepare_image(preflight).unwrap();
        assert_eq!(prepared.stream.content, jpeg);

        let bytes = images_to_pdf(
            vec![UploadFile {
                filename: "photo.jpg".to_string(),
                bytes: jpeg.clone().into(),
            }],
            None,
            None,
        )
        .unwrap();
        let document = Document::load_mem(&bytes).unwrap();

        assert!(document.objects.values().any(|object| {
            object.as_stream().is_ok_and(|stream| {
                stream
                    .dict
                    .get(b"Filter")
                    .is_ok_and(|filter| filter.as_name().is_ok_and(|name| name == b"DCTDecode"))
                    && stream.content == jpeg
            })
        }));
    }

    #[test]
    fn jpeg_frame_parser_detects_four_component_images() {
        let mut jpeg = vec![
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x14, 0x08, 0x00, 0x01, 0x00, 0x01, 0x04,
        ];
        jpeg.extend_from_slice(&[1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0, 4, 0x11, 0]);

        assert_eq!(jpeg_component_count(&jpeg).unwrap(), 4);
    }

    #[test]
    fn image_preflight_rejects_oversized_compressed_input_without_allocating_it() {
        let error =
            validate_image_input_size("oversized-card.png", MAX_SINGLE_IMAGE_INPUT_BYTES + 1)
                .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("oversized-card.png"));
        assert!(message.contains("64 MiB compressed input limit"));
    }

    #[test]
    fn image_preflight_rejects_oversized_decoded_working_set_with_dimensions() {
        let error = checked_image_working_bytes("poster.png", 8_192, 8_192, 16).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("poster.png"));
        assert!(message.contains("8192 × 8192"));
        assert!(message.contains("working buffer"));
    }

    #[test]
    fn image_preflight_uses_checked_working_set_arithmetic() {
        let error =
            checked_image_working_bytes("impossible.png", u32::MAX, u32::MAX, 16).unwrap_err();

        assert!(error.to_string().contains("impossible.png"));
    }

    #[test]
    fn image_pdf_enforces_download_limit_during_serialization() {
        let error = images_to_pdf(
            vec![UploadFile {
                filename: "small.png".to_string(),
                bytes: sample_png_bytes_with_size(2, 2).into(),
            }],
            Some(1),
            None,
        )
        .unwrap_err();

        assert!(matches!(error, AppError::PayloadTooLarge(_)));
    }
}
