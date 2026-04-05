use std::{
    fs,
    path::{Path, PathBuf},
};

/// Normalizes a project path using canonicalization when possible.
pub(crate) fn normalize_project_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path_textually(path))
}

/// Normalizes a path textually when canonicalization is not possible.
fn normalize_path_textually(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    let absolute = path.is_absolute();

    if absolute {
        normalized.push(Path::new("/"));
    }

    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::Normal(segment) => normalized.push(segment),
            std::path::Component::ParentDir => {
                if !normalized.pop() && !absolute {
                    normalized.push("..");
                }
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        if absolute {
            PathBuf::from("/")
        } else {
            PathBuf::from(".")
        }
    } else {
        normalized
    }
}
