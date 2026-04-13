use std::{fs, path::Path};

use serde::de::DeserializeOwned;

use crate::{Result, WikiError};

/// Loads and deserializes the TOML frontmatter at the top of one Markdown file.
pub(crate) fn load_markdown_frontmatter<T>(path: &Path) -> Result<T>
where
    T: DeserializeOwned,
{
    let content = fs::read_to_string(path).map_err(|source| WikiError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    let frontmatter = split_frontmatter(&content).ok_or_else(|| WikiError::MissingFrontmatter {
        path: path.to_path_buf(),
    })?;
    toml::from_str(frontmatter).map_err(|source| WikiError::ParseToml {
        path: path.to_path_buf(),
        source,
    })
}

/// Splits one Markdown document into its leading TOML frontmatter block.
fn split_frontmatter(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("+++\n")?;
    let end = rest.find("\n+++\n")?;
    Some(&rest[..end])
}
