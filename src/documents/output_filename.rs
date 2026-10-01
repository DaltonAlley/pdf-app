const MAX_STEM_CHARS: usize = 96;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputKind {
    ImposedPdf,
    ExtractedPages,
    ExtractedCombinedPdf,
    ExtractedChunks,
    RenderedImages,
    CombinedPdf,
    ConvertedPdf,
}

pub(crate) fn output_filename(source_filename: &str, kind: OutputKind) -> String {
    let stem = source_stem(source_filename);
    match kind {
        OutputKind::ImposedPdf => format!("{stem}-imposed.pdf"),
        OutputKind::ExtractedPages => format!("{stem}-pages.zip"),
        OutputKind::ExtractedCombinedPdf => format!("{stem}-pages.pdf"),
        OutputKind::ExtractedChunks => format!("{stem}-chunks.zip"),
        OutputKind::RenderedImages => format!("{stem}-images.zip"),
        OutputKind::CombinedPdf => format!("{stem}-combined.pdf"),
        OutputKind::ConvertedPdf => format!("{stem}-converted.pdf"),
    }
}

pub(crate) fn source_stem(source_filename: &str) -> String {
    let basename = source_filename.rsplit(['/', '\\']).next().unwrap_or("");
    let raw_stem = basename
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(basename);
    let mut sanitized = String::new();
    let mut pending_separator = false;
    for character in raw_stem.trim().chars() {
        if character.is_alphanumeric() || matches!(character, '-' | '_') {
            if pending_separator && !sanitized.is_empty() {
                sanitized.push('-');
            }
            pending_separator = false;
            sanitized.push(character);
        } else {
            pending_separator = true;
        }
        if sanitized.chars().count() >= MAX_STEM_CHARS {
            break;
        }
    }
    let sanitized = sanitized.trim_matches(['-', '_']).to_string();
    if sanitized.is_empty() || is_reserved(&sanitized) {
        "document".to_string()
    } else {
        sanitized
    }
}

fn is_reserved(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_safe_deterministic_names_from_source_basename() {
        assert_eq!(
            output_filename(r"C:\\jobs\\cards.final.pdf", OutputKind::ImposedPdf),
            "cards-final-imposed.pdf"
        );
        assert_eq!(source_stem("café.print.pdf"), "café-print");
        assert_eq!(source_stem("no-extension"), "no-extension");
        assert_eq!(source_stem("../<>.pdf"), "document");
        assert_eq!(source_stem("NUL.pdf"), "document");
        assert_eq!(
            source_stem(&format!("{}.pdf", "a".repeat(200)))
                .chars()
                .count(),
            MAX_STEM_CHARS
        );
    }
}
