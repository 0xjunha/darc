use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

use serde::de::DeserializeOwned;

use crate::{Result, WikiError};

/// Loads and deserializes the TOML frontmatter at the top of one Markdown file.
pub(crate) fn load_markdown_frontmatter<T>(path: &Path) -> Result<T>
where
    T: DeserializeOwned,
{
    let frontmatter = read_frontmatter(path)?;
    toml::from_str(&frontmatter).map_err(|source| WikiError::ParseToml {
        path: path.to_path_buf(),
        source,
    })
}

/// Reads only the TOML frontmatter block from the start of one Markdown document.
fn read_frontmatter(path: &Path) -> Result<String> {
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
    Ok(frontmatter)
}

/// Removes trailing line endings from one buffered Markdown line.
fn trim_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}
