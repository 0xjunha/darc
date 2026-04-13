use std::fs;

use serde::{Deserialize, Serialize};

use crate::{ProjectLayout, Result, WikiError};

const REGISTRY_SCHEMA_VERSION: u32 = 1;

/// Stores the fixed default decision-trace categories shipped in Milestone 1.
pub const DEFAULT_CATEGORY_IDS: &[&str] = &["architecture", "data", "product", "process"];

/// Represents the project-scoped wiki registry used by read-side filtering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRegistry {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_categories")]
    pub categories: Vec<String>,
    #[serde(default)]
    pub domains: Vec<String>,
}

impl Default for ProjectRegistry {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            categories: default_categories(),
            domains: Vec::new(),
        }
    }
}

impl ProjectRegistry {
    /// Normalizes registry ids into a deterministic read-side order.
    fn normalize(mut self) -> Self {
        self.categories = dedupe_preserving_order(self.categories);
        self.domains = dedupe_preserving_order(self.domains);
        self
    }
}

/// Creates the per-project registry files when they do not exist yet.
pub fn ensure_registry(layout: &ProjectLayout) -> Result<()> {
    layout.ensure()?;
    let registry = load_registry(layout)?;

    if !layout.categories_path.exists() {
        let content = toml::to_string_pretty(&CategoriesFile {
            schema_version: REGISTRY_SCHEMA_VERSION,
            categories: registry.categories.clone(),
        })
        .map_err(|source| WikiError::SerializeToml {
            path: layout.categories_path.clone(),
            source,
        })?;
        fs::write(&layout.categories_path, content).map_err(|source| WikiError::WriteFile {
            path: layout.categories_path.clone(),
            source,
        })?;
    }

    if !layout.domains_path.exists() {
        let content = toml::to_string_pretty(&DomainsFile {
            schema_version: REGISTRY_SCHEMA_VERSION,
            domains: registry.domains.clone(),
        })
        .map_err(|source| WikiError::SerializeToml {
            path: layout.domains_path.clone(),
            source,
        })?;
        fs::write(&layout.domains_path, content).map_err(|source| WikiError::WriteFile {
            path: layout.domains_path.clone(),
            source,
        })?;
    }

    Ok(())
}

/// Loads the project-scoped registry without mutating the filesystem.
pub fn load_registry(layout: &ProjectLayout) -> Result<ProjectRegistry> {
    layout.validate_storage()?;
    let categories = if layout.categories_path.exists() {
        let file = read_toml_file::<CategoriesFile>(&layout.categories_path)?;
        validate_schema_version(&layout.categories_path, file.schema_version)?;
        file.categories
    } else {
        default_categories()
    };
    let domains = if layout.domains_path.exists() {
        let file = read_toml_file::<DomainsFile>(&layout.domains_path)?;
        validate_schema_version(&layout.domains_path, file.schema_version)?;
        file.domains
    } else {
        Vec::new()
    };
    Ok(ProjectRegistry {
        schema_version: REGISTRY_SCHEMA_VERSION,
        categories,
        domains,
    }
    .normalize())
}

/// Stores the schema for the default categories registry file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CategoriesFile {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default = "default_categories")]
    categories: Vec<String>,
}

impl Default for CategoriesFile {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            categories: default_categories(),
        }
    }
}

/// Stores the schema for the project-scoped domains registry file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DomainsFile {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default)]
    domains: Vec<String>,
}

impl Default for DomainsFile {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            domains: Vec::new(),
        }
    }
}

/// Returns the fixed registry schema version.
fn default_schema_version() -> u32 {
    REGISTRY_SCHEMA_VERSION
}

/// Returns the fixed default category list as owned strings.
fn default_categories() -> Vec<String> {
    DEFAULT_CATEGORY_IDS
        .iter()
        .map(|category| (*category).to_owned())
        .collect()
}

/// Validates one persisted registry schema version against the current implementation.
fn validate_schema_version(path: &std::path::Path, schema_version: u32) -> Result<()> {
    if schema_version == REGISTRY_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(WikiError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            expected: REGISTRY_SCHEMA_VERSION,
            actual: schema_version,
        })
    }
}

/// Loads and deserializes one TOML file from disk.
fn read_toml_file<T>(path: &std::path::Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let content = fs::read_to_string(path).map_err(|source| WikiError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&content).map_err(|source| WikiError::ParseToml {
        path: path.to_path_buf(),
        source,
    })
}

/// Removes duplicate ids while preserving their first-seen order.
fn dedupe_preserving_order(values: Vec<String>) -> Vec<String> {
    let mut unique = Vec::with_capacity(values.len());
    for value in values {
        if !unique.contains(&value) {
            unique.push(value);
        }
    }
    unique
}
