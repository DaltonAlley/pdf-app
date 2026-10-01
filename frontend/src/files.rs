//! File facts shared by each frontend workflow.
#![cfg_attr(test, allow(dead_code))]

/// Browser-independent facts used to classify a selected file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileDescriptor {
    /// Display name supplied by the browser.
    pub(crate) name: String,
    /// MIME type supplied by the browser; extensions are used as a fallback.
    pub(crate) mime_type: String,
    /// File size in bytes.
    pub(crate) size: u64,
}

/// Supported file category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileKind {
    /// PDF input.
    Pdf,
    /// PNG or JPEG input.
    Image,
    /// Unsupported input.
    Unsupported,
}

/// Classifies a file by MIME type and safe extension fallback.
pub(crate) fn classify_file(file: &FileDescriptor) -> FileKind {
    let lower_name = file.name.to_ascii_lowercase();
    if file.mime_type == "application/pdf" || lower_name.ends_with(".pdf") {
        FileKind::Pdf
    } else if matches!(file.mime_type.as_str(), "image/png" | "image/jpeg")
        || lower_name.ends_with(".png")
        || lower_name.ends_with(".jpg")
        || lower_name.ends_with(".jpeg")
    {
        FileKind::Image
    } else {
        FileKind::Unsupported
    }
}

/// A browser file paired with its target-neutral description and stable UI key.
#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
pub(crate) struct SelectedFile {
    pub(crate) id: u64,
    pub(crate) file: web_sys::File,
    pub(crate) descriptor: FileDescriptor,
}

#[cfg(target_arch = "wasm32")]
impl SelectedFile {
    pub(crate) fn new(id: u64, file: web_sys::File) -> Self {
        Self {
            id,
            descriptor: FileDescriptor {
                name: file.name(),
                mime_type: file.type_(),
                size: file.size() as u64,
            },
            file,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_file, FileDescriptor, FileKind};

    fn file(name: &str, mime_type: &str) -> FileDescriptor {
        FileDescriptor {
            name: name.to_owned(),
            mime_type: mime_type.to_owned(),
            size: 10,
        }
    }

    #[test]
    fn classification_uses_mime_type_then_safe_extension_fallbacks() {
        assert_eq!(
            classify_file(&file("upload", "application/pdf")),
            FileKind::Pdf
        );
        assert_eq!(classify_file(&file("ARTWORK.PDF", "")), FileKind::Pdf);
        assert_eq!(classify_file(&file("photo", "image/jpeg")), FileKind::Image);
        assert_eq!(classify_file(&file("PHOTO.PNG", "")), FileKind::Image);
        assert_eq!(
            classify_file(&file("notes.txt", "text/plain")),
            FileKind::Unsupported
        );
    }
}
