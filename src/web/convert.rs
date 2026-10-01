use std::sync::Arc;

use axum::{
    extract::{Multipart, State},
    response::Response,
};

use super::state::OperationPermit;
use crate::{
    adapters::{
        read_multipart, require_files, require_images, require_pdfs, FileDownload, FormData,
    },
    documents as image_pdf, documents as pdf_render,
    documents::{output_filename, source_stem, OutputKind, PageSelection, RasterFormat},
    error::{AppError, AppResult},
    progress::{report, ProgressCallback},
    AppState,
};

pub(crate) async fn convert(
    State(state): State<Arc<AppState>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let form = read_multipart(multipart).await?;
    let operation_permit = state.admit_operation()?;
    Ok(convert_form(state, form, operation_permit, None)
        .await?
        .into_response())
}

pub(crate) async fn convert_form(
    state: Arc<AppState>,
    form: FormData,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
) -> AppResult<FileDownload> {
    match ConvertTarget::parse(form.required_field("target")?)? {
        ConvertTarget::Pdf => images_to_pdf(state, form, operation_permit, progress).await,
        ConvertTarget::Png => {
            pdfs_to_images(state, form, operation_permit, RasterFormat::Png, progress).await
        }
        ConvertTarget::Jpeg => {
            pdfs_to_images(state, form, operation_permit, RasterFormat::Jpeg, progress).await
        }
    }
}

async fn images_to_pdf(
    state: Arc<AppState>,
    form: FormData,
    operation_permit: OperationPermit,
    progress: Option<ProgressCallback>,
) -> AppResult<FileDownload> {
    let max_download_bytes = state.max_download_bytes();
    require_files(&form.files)?;
    require_images(&form.files, "target=pdf requires PNG or JPEG input")?;
    report(
        &progress,
        30,
        format!("Preparing {} image(s)", form.files.len()),
    )?;
    let options = image_pdf::ImagePdfOptions::from_form(&form)?;
    let source_filename = form
        .files
        .first()
        .map(|file| file.filename.clone())
        .ok_or_else(|| AppError::bad_request("expected at least one image"))?;

    let image_progress = progress.clone();
    let files = form.files;
    let bytes = state
        .run_cpu(operation_permit, move || {
            image_pdf::images_to_pdf_with_options(
                files,
                max_download_bytes,
                image_progress,
                options,
            )
        })
        .await?;
    report(&progress, 95, "Preparing download")?;
    FileDownload::bounded(
        "application/pdf",
        output_filename(&source_filename, OutputKind::ConvertedPdf),
        bytes,
        max_download_bytes,
    )
}

async fn pdfs_to_images(
    state: Arc<AppState>,
    form: FormData,
    operation_permit: OperationPermit,
    format: RasterFormat,
    progress: Option<ProgressCallback>,
) -> AppResult<FileDownload> {
    require_files(&form.files)?;
    require_pdfs(&form.files, "target=png/jpeg requires PDF input")?;
    let pages = PageSelection::parse(form.field("pages").unwrap_or("1"))?;
    let options = pdf_render::RasterOptions::from_form(&form)?;
    let source_filename = form
        .files
        .first()
        .map(|file| file.filename.clone())
        .ok_or_else(|| AppError::bad_request("expected at least one PDF"))?;

    report(&progress, 25, "Reading PDF")?;
    let is_single_file = form.files.len() == 1;
    let max_pages = state.max_render_pages();
    let max_download_bytes = state.max_download_bytes();
    let pdfium = state.pdfium();
    let render_progress = progress.clone();
    let mut rendered = state
        .run_pdfium(operation_permit.clone(), move || {
            pdf_render::render_pdf_files(
                pdfium,
                form.files,
                pages,
                pdf_render::RenderLimits::new(max_pages, max_download_bytes),
                format,
                options,
                render_progress,
            )
        })
        .await?;
    let source_stem = source_stem(&source_filename);
    for (index, image) in rendered.iter_mut().enumerate() {
        image.zip_path = format!("{source_stem}-page-{:04}.{}", index + 1, format.extension());
    }

    if is_single_file && rendered.len() == 1 {
        let Some(image) = rendered.into_iter().next() else {
            return Err(AppError::Internal(
                "renderer returned no image for a completed single-page request".to_string(),
            ));
        };
        report(&progress, 95, "Preparing download")?;
        return FileDownload::bounded(
            format.content_type(),
            format!(
                "{source_stem}-page-{:04}.{}",
                image.page,
                format.extension()
            ),
            image.bytes,
            max_download_bytes,
        );
    }

    report(&progress, 90, "Packaging images")?;
    let zip_bytes = state
        .run_cpu(operation_permit, move || pdf_render::zip_images(rendered))
        .await?;
    report(&progress, 95, "Preparing download")?;
    FileDownload::bounded(
        "application/zip",
        output_filename(&source_filename, OutputKind::RenderedImages),
        zip_bytes,
        max_download_bytes,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConvertTarget {
    Pdf,
    Png,
    Jpeg,
}

impl ConvertTarget {
    fn parse(target: &str) -> AppResult<Self> {
        match target.to_ascii_lowercase().as_str() {
            "pdf" => Ok(Self::Pdf),
            "png" => Ok(Self::Png),
            "jpeg" => Ok(Self::Jpeg),
            _ => Err(AppError::bad_request("target must be pdf, png, or jpeg")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::DynamicImage;

    fn sample_png() -> Vec<u8> {
        let image = image::RgbImage::from_pixel(20, 10, image::Rgb([12, 34, 56]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    #[tokio::test]
    async fn convert_form_applies_image_pdf_geometry_fields() {
        let Some(pdfium) = crate::test_pdfium() else {
            return;
        };
        let state = Arc::new(AppState::for_tests(pdfium.shared()).unwrap());
        let form = FormData {
            files: vec![crate::adapters::UploadFile {
                filename: "photo.png".to_string(),
                bytes: sample_png().into(),
            }],
            fields: vec![
                ("target".to_string(), "pdf".to_string()),
                ("pageSize".to_string(), "letter".to_string()),
                ("orientation".to_string(), "landscape".to_string()),
                ("imageFit".to_string(), "cover".to_string()),
                ("marginPoints".to_string(), "18".to_string()),
            ],
        };
        let permit = state.admit_operation().unwrap();
        let download = convert_form(state, form, permit, None).await.unwrap();

        assert_eq!(download.content_type, "application/pdf");
        assert_eq!(download.filename, "photo-converted.pdf");
        let output = pdfium
            .load_pdf_from_byte_vec(download.bytes.to_vec(), None)
            .unwrap();
        let page = output.pages().get(0).unwrap();
        assert!((page.width().value - 792.0).abs() < 0.01);
        assert!((page.height().value - 612.0).abs() < 0.01);
    }
}
