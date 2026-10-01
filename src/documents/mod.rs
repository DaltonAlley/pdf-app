//! Document transformation domain.
//!
//! The module root is the only seam used by the web and imposition layers;
//! Pdfium, image encoding, and PDF parsing details remain private children.

mod image_pdf;
mod output_filename;
mod page_selection;
mod pdf_content_decode;
mod pdf_io;
mod pdf_merge;
mod pdf_render;
mod pdf_split;

pub(crate) use image_pdf::{
    images_to_pdf, images_to_pdf_with_options, images_to_pdf_with_progress,
    validate_image_input_size, ImagePdfOptions, ImageProgressPlan,
};
pub(crate) use output_filename::{output_filename, source_stem, OutputKind};
pub(crate) use page_selection::PageSelection;
pub(crate) use pdf_content_decode::decode_page_content_stream;
pub(crate) use pdf_io::{estimated_lopdf_output_capacity, load_pdf, load_pdf_path, page_count};
pub(crate) use pdf_merge::{merge_pdfs, merge_staged_pdfs};
pub(crate) use pdf_render::{
    encode_preview_batch, encode_preview_page, raster_preview_pages, render_pdf_files, zip_images,
    RasterFormat, RasterOptions, RenderLimits,
};
pub(crate) use pdf_split::{extract_pdf_pages, ExtractOutputMode};
