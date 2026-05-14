use anyhow::{Context, Result};

/// Applies offset/limit pagination to a fully ranked in-memory row set.
pub(crate) fn paginate_ranked_rows<T>(
    rows: Vec<T>,
    limit: usize,
    offset: usize,
) -> Result<(Vec<T>, bool)> {
    let page_end = offset
        .checked_add(limit)
        .context("query pagination exceeds usize range")?;
    let has_more = rows.len() > page_end;
    let rows = rows.into_iter().skip(offset).take(limit).collect();
    Ok((rows, has_more))
}

/// Applies one optional matched-path preview cap to an already ordered path list.
pub(crate) fn apply_matched_path_limit(
    mut paths: Vec<String>,
    matched_path_limit: Option<usize>,
) -> (Vec<String>, bool) {
    if let Some(limit) = matched_path_limit
        && paths.len() > limit
    {
        paths.truncate(limit);
        return (paths, true);
    }
    (paths, false)
}
