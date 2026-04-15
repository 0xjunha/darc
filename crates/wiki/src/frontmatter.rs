use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
};

use serde::{Serialize, de::DeserializeOwned};

use crate::{Result, WikiError};

/// Loads and deserializes the TOML frontmatter at the top of one Markdown file.
pub(crate) fn load_markdown_frontmatter<T>(path: &Path) -> Result<T>
where
    T: DeserializeOwned,
{
    let (frontmatter, _) = read_frontmatter_and_body(path)?;
    toml::from_str(&frontmatter).map_err(|source| WikiError::ParseToml {
        path: path.to_path_buf(),
        source,
    })
}

/// Loads and deserializes one Markdown frontmatter block plus the remaining body text.
pub(crate) fn load_markdown_frontmatter_and_body<T>(path: &Path) -> Result<(T, String)>
where
    T: DeserializeOwned,
{
    let (frontmatter, body) = read_frontmatter_and_body(path)?;
    let frontmatter = toml::from_str(&frontmatter).map_err(|source| WikiError::ParseToml {
        path: path.to_path_buf(),
        source,
    })?;
    Ok((frontmatter, body))
}

/// Serializes one TOML frontmatter block plus Markdown body without writing it to disk.
pub(crate) fn render_markdown_document<T>(
    path: &Path,
    frontmatter: &T,
    body_markdown: &str,
) -> Result<String>
where
    T: Serialize,
{
    let mut content =
        toml::to_string_pretty(frontmatter).map_err(|source| WikiError::SerializeToml {
            path: path.to_path_buf(),
            source,
        })?;
    if !content.ends_with('\n') {
        content.push('\n');
    }

    let body_markdown = body_markdown.trim();
    let mut document = format!("+++\n{content}+++\n");
    if !body_markdown.is_empty() {
        document.push('\n');
        document.push_str(body_markdown);
        document.push('\n');
    }
    Ok(document)
}

/// Reads the TOML frontmatter block plus the remaining Markdown body from one document.
fn read_frontmatter_and_body(path: &Path) -> Result<(String, String)> {
    let file = File::open(path).map_err(|source| WikiError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .map_err(|source| WikiError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes == 0 || trim_line_ending(&line) != "+++" {
        return Err(WikiError::MissingFrontmatter {
            path: path.to_path_buf(),
        });
    }

    let mut frontmatter = String::new();
    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|source| WikiError::ReadFile {
                path: path.to_path_buf(),
                source,
            })?;
        if bytes == 0 {
            return Err(WikiError::MissingFrontmatter {
                path: path.to_path_buf(),
            });
        }
        if trim_line_ending(&line) == "+++" {
            break;
        }
        frontmatter.push_str(&line);
    }
    let mut body = String::new();
    reader
        .read_to_string(&mut body)
        .map_err(|source| WikiError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
    Ok((frontmatter, body))
}

/// Removes trailing line endings from one buffered Markdown line.
fn trim_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}
