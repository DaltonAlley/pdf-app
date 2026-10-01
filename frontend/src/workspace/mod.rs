//! Generic file selection, tool choice, and job execution workflow.
#![cfg_attr(test, allow(unused_imports))]

mod model;

pub(crate) use model::{
    extract_chunk_size, move_item_to, normalize_selection, visible_operations, ExtractOutputMode,
    ImageTarget, Operation, PageSelection, RasterQuality, SelectionIntent,
};

#[cfg(target_arch = "wasm32")]
pub(crate) use model::{GenericJobOperation, GenericJobSettings};

#[cfg(any(target_arch = "wasm32", test))]
mod file_queue;

#[cfg(target_arch = "wasm32")]
mod browser;

#[cfg(target_arch = "wasm32")]
pub(crate) use browser::{build_job_form, execute_job, preflight_pdf_files};
#[cfg(target_arch = "wasm32")]
pub(crate) use file_queue::{operation_uses_file_order, FileQueue};

#[cfg(test)]
mod contract_tests {
    use crate::files::FileDescriptor;

    use super::{normalize_selection, Operation, SelectionIntent};

    #[test]
    fn images_to_pdf_has_no_frontend_geometry_choices() {
        let view = include_str!("../app/generic.rs");
        for removed in [
            "PDF page size",
            "PDF page orientation",
            "Image fit",
            "image-pdf-margin",
        ] {
            assert!(!view.contains(removed), "obsolete image control: {removed}");
        }
        assert!(view.contains("Each page matches its image at 300 DPI. One image per page."));
        let request = include_str!("browser.rs");
        let image_request = request
            .split("GenericJobOperation::ImagePdf =>")
            .nth(1)
            .unwrap_or_default();
        assert!(image_request.contains("append_text(&form, \"layout\", \"single\")?;"));
        assert!(image_request.contains("append_text(&form, \"pageSize\", \"original\")?;"));
        for removed in ["\"orientation\"", "\"imageFit\"", "\"marginPoints\""] {
            assert!(!image_request.contains(removed));
        }
    }

    #[test]
    fn module_seam_preserves_a_multi_pdf_merge_workflow() {
        let files = ["cover.pdf", "inside.pdf"].map(|name| FileDescriptor {
            name: name.into(),
            mime_type: "application/pdf".into(),
            size: 1,
        });

        let selection =
            normalize_selection(&files, Operation::PdfImage, SelectionIntent::NewUpload)
                .unwrap_or_else(|error| unreachable!("valid PDF workflow: {error}"));

        assert_eq!(selection.operation, Operation::Merge);
    }
}
