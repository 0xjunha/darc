use super::{DEFAULT_TEXT_PREVIEW_CHARS, ONELINE_TEXT_PREVIEW_CHARS};

/// Stores one normalized text preview plus source-size metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextPreview {
    pub(crate) text: String,
    pub(crate) truncated: bool,
    pub(crate) chars: u64,
    pub(crate) total_chars: u64,
}

/// Normalizes one text field into a single-line preview with metadata.
pub(crate) fn preview_text(text: &str) -> TextPreview {
    preview_text_with_limit(text, DEFAULT_TEXT_PREVIEW_CHARS)
}

/// Normalizes one text field into a capped single-line preview with metadata.
fn preview_text_with_limit(text: &str, max_chars: usize) -> TextPreview {
    preview_normalized_text(&normalize_preview_whitespace(text), max_chars)
}

/// Normalizes one text field's first line into a capped single-line preview with metadata.
pub(crate) fn preview_first_line(text: &str) -> TextPreview {
    preview_text_first_line_with_limit(text, ONELINE_TEXT_PREVIEW_CHARS)
}

/// Normalizes one text field's first line into a capped single-line preview with metadata.
fn preview_text_first_line_with_limit(text: &str, max_chars: usize) -> TextPreview {
    preview_normalized_text(
        &normalize_preview_whitespace(text.lines().next().unwrap_or_default()),
        max_chars,
    )
}

/// Collapses one preview string's whitespace into single spaces.
fn normalize_preview_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Builds metadata for one already normalized text preview.
fn preview_normalized_text(text: &str, max_chars: usize) -> TextPreview {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return TextPreview {
            text: text.to_owned(),
            truncated: false,
            chars: u64::try_from(total_chars).unwrap_or(u64::MAX),
            total_chars: u64::try_from(total_chars).unwrap_or(u64::MAX),
        };
    }
    let mut preview = text
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    preview.push('…');
    let chars = preview.chars().count();
    TextPreview {
        text: preview,
        truncated: true,
        chars: u64::try_from(chars).unwrap_or(u64::MAX),
        total_chars: u64::try_from(total_chars).unwrap_or(u64::MAX),
    }
}
