//! Target-neutral frontend domain rules.
#![cfg_attr(test, allow(dead_code))]

use std::fmt;

use crate::files::{classify_file, FileDescriptor, FileKind};

/// A supported PDF Tools operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    /// Split selected PDF pages into a ZIP.
    Split,
    /// Rasterize PDF pages.
    PdfImage,
    /// Merge ordered PDFs.
    Merge,
    /// Create a PDF from ordered images.
    ImagePdf,
    /// Arrange artwork on production-ready print sheets.
    Impose,
}

/// Operations submitted through the generic `/jobs` multipart contract.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GenericJobOperation {
    Split,
    PdfImage,
    Merge,
    ImagePdf,
}

#[cfg(any(target_arch = "wasm32", test))]
impl GenericJobOperation {
    pub(crate) const fn from_operation(operation: Operation) -> Option<Self> {
        match operation {
            Operation::Split => Some(Self::Split),
            Operation::PdfImage => Some(Self::PdfImage),
            Operation::Merge => Some(Self::Merge),
            Operation::ImagePdf => Some(Self::ImagePdf),
            Operation::Impose => None,
        }
    }
}

impl Operation {
    /// Stable identifier used by controls and routing.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::PdfImage => "pdf-image",
            Self::Merge => "merge",
            Self::ImagePdf => "image-pdf",
            Self::Impose => "impose",
        }
    }

    /// User-facing label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Split => "Extract pages",
            Self::PdfImage => "PDF to images",
            Self::Merge => "Combine PDFs",
            Self::ImagePdf => "Images to PDF",
            Self::Impose => "Impose artwork",
        }
    }

    /// Short explanation of the operation.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Split => "Choose pages and how to package them.",
            Self::PdfImage => "Choose pages, format, and quality.",
            Self::Merge => "Put PDFs in order and combine them into one file.",
            Self::ImagePdf => "Order images, then create a PDF.",
            Self::Impose => "Arrange ordered artwork on production-ready print sheets.",
        }
    }

    /// Label for the primary action.
    pub const fn submit_label(self) -> &'static str {
        match self {
            Self::Split => "Download ZIP",
            Self::PdfImage => "Download images",
            Self::Merge => "Download PDF",
            Self::ImagePdf => "Create PDF",
            Self::Impose => "Prepare print sheet",
        }
    }
}

/// Packaging used when extracting pages from a PDF.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtractOutputMode {
    /// One single-page PDF per selected page in a ZIP archive.
    Individual,
    /// All selected pages in one PDF, preserving source-document order.
    Combined,
    /// Consecutive groups of selected pages in a ZIP archive.
    Chunks,
}

impl ExtractOutputMode {
    /// Backend multipart value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Individual => "individual",
            Self::Combined => "combined",
            Self::Chunks => "chunks",
        }
    }

    /// User-facing primary action for this packaging choice.
    pub const fn submit_label(self) -> &'static str {
        match self {
            Self::Individual | Self::Chunks => "Download ZIP",
            Self::Combined => "Download PDF",
        }
    }
}

/// Current generic-workflow settings submitted with a browser job.
#[cfg(target_arch = "wasm32")]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GenericJobSettings {
    pub(crate) target: ImageTarget,
    pub(crate) quality: RasterQuality,
    pub(crate) extract_pages: PageSelection,
    pub(crate) export_pages: PageSelection,
    pub(crate) extract_output_mode: ExtractOutputMode,
    pub(crate) extract_chunk_size_draft: String,
}

/// Validates the number of selected pages placed in each extracted chunk.
pub fn extract_chunk_size(value: &str) -> Result<u16, &'static str> {
    let size = value
        .trim()
        .parse::<u16>()
        .map_err(|_| "Enter a whole number from 1 to 1,000.")?;
    if !(1..=1_000).contains(&size) {
        return Err("Enter a whole number from 1 to 1,000.");
    }
    Ok(size)
}

/// One ordered browser-file part in a Combine PDFs upload.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MergeUploadPart<'a> {
    /// Position of the browser file in the current selected-file vector.
    pub(crate) source_index: usize,
    /// Filename written to the multipart part.
    pub(crate) filename: &'a str,
}

/// Plans Combine PDFs multipart parts without crossing into browser APIs.
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn plan_merge_upload(
    files: &[FileDescriptor],
) -> Result<Vec<MergeUploadPart<'_>>, &'static str> {
    if files.len() < 2 {
        return Err("Choose two or more PDFs to merge.");
    }
    Ok(files
        .iter()
        .enumerate()
        .map(|(source_index, file)| MergeUploadPart {
            source_index,
            filename: &file.name,
        })
        .collect())
}

/// How a browser selection should affect the current workflow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionIntent {
    /// Retain the current operation when it remains valid.
    ContinueWorkflow,
    /// Choose the most likely operation for a replacement selection.
    NewUpload,
}

/// Validated outcome of normalizing a browser file selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NormalizedSelection {
    /// Operation that should own the selected files.
    pub operation: Operation,
}

/// File-selection validation error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionError(&'static str);

impl fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// Returns operations reachable for a homogeneous selection.
pub fn visible_operations(files: &[FileDescriptor]) -> Vec<Operation> {
    let all_pdf = all_kind(files, FileKind::Pdf);
    let all_images = all_kind(files, FileKind::Image);
    if files.len() > 1 && all_pdf {
        vec![Operation::PdfImage, Operation::Merge, Operation::Impose]
    } else if files.len() == 1 && all_pdf {
        vec![Operation::PdfImage, Operation::Split, Operation::Impose]
    } else if all_images {
        vec![Operation::ImagePdf, Operation::Impose]
    } else if !files.is_empty()
        && files
            .iter()
            .all(|file| classify_file(file) != FileKind::Unsupported)
    {
        vec![Operation::Impose]
    } else {
        Vec::new()
    }
}

/// Chooses the default operation for a valid selection.
pub fn preferred_operation(files: &[FileDescriptor]) -> Operation {
    if files.len() > 1 && all_kind(files, FileKind::Pdf) {
        Operation::Merge
    } else if all_kind(files, FileKind::Image) {
        Operation::ImagePdf
    } else if files
        .iter()
        .any(|file| classify_file(file) == FileKind::Image)
    {
        Operation::Impose
    } else {
        Operation::PdfImage
    }
}

/// Validates selected files and preserves a compatible active operation.
pub fn normalize_selection(
    files: &[FileDescriptor],
    previous: Operation,
    intent: SelectionIntent,
) -> Result<NormalizedSelection, SelectionError> {
    if files.is_empty() {
        return Err(SelectionError("Choose at least one file."));
    }
    if files
        .iter()
        .any(|file| classify_file(file) == FileKind::Unsupported)
    {
        return Err(SelectionError("Upload a PDF, PNG, or JPEG file."));
    }
    let available = visible_operations(files);
    let operation = if intent == SelectionIntent::ContinueWorkflow && available.contains(&previous)
    {
        previous
    } else {
        preferred_operation(files)
    };
    Ok(NormalizedSelection { operation })
}

fn all_kind(files: &[FileDescriptor], kind: FileKind) -> bool {
    !files.is_empty() && files.iter().all(|file| classify_file(file) == kind)
}

/// Page selection submitted to the backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageSelection {
    /// Every page.
    All,
    /// A comma-separated list of one-based pages and inclusive ranges.
    Range(String),
}

impl PageSelection {
    /// Backend request expression.
    pub fn expression(&self) -> &str {
        match self {
            Self::All => "all",
            Self::Range(range) => range,
        }
    }

    /// Validates the same input constraints exposed by the UI.
    pub fn validate(&self) -> Result<(), &'static str> {
        let Self::Range(range) = self else {
            return Ok(());
        };
        validate_page_range(range).map(|_| ())
    }

    /// Validates syntax and rejects pages beyond a known PDF source boundary.
    pub fn validate_for_page_count(&self, page_count: Option<usize>) -> Result<(), String> {
        let Self::Range(range) = self else {
            return Ok(());
        };
        let maximum = validate_page_range(range).map_err(str::to_owned)?;
        if page_count.is_some_and(|count| maximum > count as u64) {
            let count = page_count.unwrap_or_default();
            return Err(format!(
                "Page {maximum} is out of range. The selected PDF source has {count} {}.",
                if count == 1 { "page" } else { "pages" },
            ));
        }
        Ok(())
    }
}

fn validate_page_range(range: &str) -> Result<u64, &'static str> {
    let expression = range.trim();
    if expression.is_empty() {
        return Err("Enter at least one page.");
    }
    if expression.len() > 4_096 {
        return Err("Keep the page range under 4,096 characters.");
    }
    let mut selected = 0_u64;
    let mut maximum = 0_u64;
    for raw_part in expression.split(',') {
        let part = raw_part.trim();
        let mut ends = part.split('-').map(str::trim);
        let start = ends
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or("Use page numbers like 1-3, 5, 8-10.")?;
        let end = match ends.next() {
            Some(value) if !value.is_empty() => value
                .parse::<u64>()
                .map_err(|_| "Use page numbers like 1-3, 5, 8-10.")?,
            Some(_) => return Err("Use page numbers like 1-3, 5, 8-10."),
            None => start,
        };
        if ends.next().is_some() {
            return Err("Use page numbers like 1-3, 5, 8-10.");
        }
        if start == 0 || end == 0 {
            return Err("Page numbers start at 1.");
        }
        if start > end {
            return Err("Page ranges must count up, such as 3-5.");
        }
        selected = selected.saturating_add(end - start + 1);
        if selected > 10_000 {
            return Err("Select no more than 10,000 pages.");
        }
        maximum = maximum.max(end);
    }
    Ok(maximum)
}

/// Raster output type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageTarget {
    /// PNG output.
    Png,
    /// JPEG output.
    Jpeg,
}

impl ImageTarget {
    /// Backend field value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
        }
    }
}

/// Raster quality preset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RasterQuality {
    /// 144 DPI, fast browser-oriented output.
    Screen,
    /// 300 DPI print-oriented output.
    Print,
    /// 600 DPI detailed output.
    High,
}

impl RasterQuality {
    /// Stable control value.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Print => "print",
            Self::High => "high",
        }
    }

    /// User-facing label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Screen => "Low",
            Self::Print => "Standard",
            Self::High => "High",
        }
    }

    /// Backend DPI and JPEG quality values.
    pub const fn settings(self) -> (u16, u8) {
        match self {
            Self::Screen => (144, 80),
            Self::Print => (300, 92),
            Self::High => (600, 95),
        }
    }

    /// Contextual help.
    pub const fn help(self) -> &'static str {
        match self {
            Self::Screen => "144 DPI · fastest and recommended for large PDFs.",
            Self::Print => "300 DPI · sharper output for smaller page ranges.",
            Self::High => "600 DPI · fine detail for short page ranges.",
        }
    }
}

/// Moves an item directly to another queue position, returning whether it changed.
pub fn move_item_to<T>(items: &mut [T], from: usize, to: usize) -> bool {
    if from >= items.len() || to >= items.len() || from == to {
        return false;
    }
    if from < to {
        items[from..=to].rotate_left(1);
    } else {
        items[to..=from].rotate_right(1);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_quality_preserves_the_backend_resolution_and_compression_contract() {
        assert_eq!(
            [
                RasterQuality::Screen,
                RasterQuality::Print,
                RasterQuality::High,
            ]
            .map(|quality| quality.settings()),
            [(144, 80), (300, 92), (600, 95)]
        );
    }

    #[test]
    fn extract_packaging_maps_to_the_backend_contract() {
        assert_eq!(ExtractOutputMode::Individual.as_str(), "individual");
        assert_eq!(ExtractOutputMode::Combined.as_str(), "combined");
        assert_eq!(ExtractOutputMode::Chunks.as_str(), "chunks");
        assert_eq!(ExtractOutputMode::Individual.submit_label(), "Download ZIP");
        assert_eq!(ExtractOutputMode::Combined.submit_label(), "Download PDF");
        assert_eq!(extract_chunk_size("1"), Ok(1));
        assert_eq!(extract_chunk_size("1000"), Ok(1_000));
        assert_eq!(
            extract_chunk_size("0"),
            Err("Enter a whole number from 1 to 1,000.")
        );
        assert_eq!(
            extract_chunk_size("1001"),
            Err("Enter a whole number from 1 to 1,000.")
        );
    }

    #[test]
    fn generic_job_mapping_excludes_the_imposition_workspace() {
        assert_eq!(
            GenericJobOperation::from_operation(Operation::Split),
            Some(GenericJobOperation::Split)
        );
        assert_eq!(GenericJobOperation::from_operation(Operation::Impose), None);
    }

    fn file(name: &str, mime_type: &str) -> FileDescriptor {
        FileDescriptor {
            name: name.to_owned(),
            mime_type: mime_type.to_owned(),
            size: 10,
        }
    }

    #[test]
    fn selection_normalizes_by_homogeneous_file_kind() {
        let pdfs = [file("one.pdf", ""), file("two", "application/pdf")];
        let images = [file("one.png", ""), file("two", "image/jpeg")];
        assert_eq!(preferred_operation(&pdfs), Operation::Merge);
        assert_eq!(preferred_operation(&images), Operation::ImagePdf);
        assert!(visible_operations(&pdfs).contains(&Operation::PdfImage));
        assert!(visible_operations(&images).contains(&Operation::Impose));
    }

    #[test]
    fn selection_rejects_unsupported_files_and_preserves_compatible_operation() {
        let unsupported = [file("notes.txt", "text/plain")];
        assert!(normalize_selection(
            &unsupported,
            Operation::PdfImage,
            SelectionIntent::NewUpload
        )
        .is_err());
        let pdf = [file("one.pdf", "application/pdf")];
        assert_eq!(
            normalize_selection(&pdf, Operation::Split, SelectionIntent::ContinueWorkflow),
            Ok(NormalizedSelection {
                operation: Operation::Split
            })
        );
    }

    #[test]
    fn replacement_keeps_supported_workflow_and_falls_back_when_incompatible() {
        let pdfs = [file("one.pdf", "application/pdf"), file("two.pdf", "")];
        assert_eq!(
            normalize_selection(&pdfs, Operation::Impose, SelectionIntent::ContinueWorkflow),
            Ok(NormalizedSelection {
                operation: Operation::Impose
            })
        );

        let images = [file("one.png", "image/png"), file("two.jpg", "image/jpeg")];
        assert_eq!(
            normalize_selection(&images, Operation::Merge, SelectionIntent::ContinueWorkflow),
            Ok(NormalizedSelection {
                operation: Operation::ImagePdf
            })
        );
    }

    #[test]
    fn mixed_supported_selection_routes_only_to_imposition_workspace() {
        let mixed = [file("one.pdf", ""), file("two.jpg", "")];
        assert_eq!(visible_operations(&mixed), vec![Operation::Impose]);
        assert_eq!(preferred_operation(&mixed), Operation::Impose);
        assert_eq!(GenericJobOperation::from_operation(Operation::Impose), None);
    }

    #[test]
    fn page_selection_validation_covers_ranges_and_limits() {
        assert_eq!(
            PageSelection::Range("1-3, 5, 8-10".into()).validate(),
            Ok(())
        );
        assert_eq!(
            PageSelection::Range("3-1".into()).validate(),
            Err("Page ranges must count up, such as 3-5.")
        );
        assert_eq!(
            PageSelection::Range("1-10001".into()).validate(),
            Err("Select no more than 10,000 pages.")
        );
        assert!(PageSelection::All.validate().is_ok());
    }

    #[test]
    fn page_selection_rejects_pages_beyond_a_known_source() {
        assert_eq!(
            PageSelection::Range("1-3, 6".into()).validate_for_page_count(Some(5)),
            Err("Page 6 is out of range. The selected PDF source has 5 pages.".into())
        );
        assert_eq!(
            PageSelection::Range("5".into()).validate_for_page_count(Some(5)),
            Ok(())
        );
        assert_eq!(
            PageSelection::Range("6".into()).validate_for_page_count(None),
            Ok(())
        );
        assert_eq!(PageSelection::All.validate_for_page_count(Some(5)), Ok(()));
    }

    #[test]
    fn queue_direct_reorder_handles_both_directions_and_bounds() {
        let mut items = vec![1, 2, 3, 4];
        assert!(move_item_to(&mut items, 0, 2));
        assert_eq!(items, vec![2, 3, 1, 4]);
        assert!(move_item_to(&mut items, 3, 1));
        assert_eq!(items, vec![2, 4, 3, 1]);
        assert!(!move_item_to(&mut items, 1, 1));
        assert!(!move_item_to(&mut items, 4, 0));
        assert!(!move_item_to(&mut items, 0, 4));
        assert_eq!(items, vec![2, 4, 3, 1]);
    }

    #[test]
    fn combine_upload_plan_consumes_the_reordered_descriptor_vector_exactly() {
        let mut files = vec![
            file("cover.pdf", "application/pdf"),
            file("body.pdf", "application/pdf"),
            file("back.pdf", "application/pdf"),
        ];
        assert!(move_item_to(&mut files, 2, 0));
        let plan = plan_merge_upload(&files);
        assert_eq!(
            plan,
            Ok(vec![
                MergeUploadPart {
                    source_index: 0,
                    filename: "back.pdf",
                },
                MergeUploadPart {
                    source_index: 1,
                    filename: "cover.pdf",
                },
                MergeUploadPart {
                    source_index: 2,
                    filename: "body.pdf",
                },
            ])
        );
    }

    #[test]
    fn combine_upload_plan_rejects_less_than_two_files() {
        assert_eq!(
            plan_merge_upload(&[file("only.pdf", "application/pdf")]),
            Err("Choose two or more PDFs to merge.")
        );
    }
}
