/// Validates one lowercase slug identifier shared by registry categories and domains.
pub(crate) fn is_valid_slug_id(value: &str) -> bool {
    let mut parts = value.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    if first.is_empty()
        || !first
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
    {
        return false;
    }
    parts.all(|part| {
        !part.is_empty()
            && part
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
    })
}
