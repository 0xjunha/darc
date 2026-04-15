use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{ProjectLayout, Result, WikiError, slug::is_valid_slug_id};

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
    fn normalize(mut self, categories_path: &Path, domains_path: &Path) -> Result<Self> {
        self.categories = dedupe_preserving_order(self.categories);
        self.domains = dedupe_preserving_order(self.domains);
        validate_category_ids(categories_path, &self.categories)?;
        validate_domain_ids(domains_path, &self.domains)?;
        Ok(self)
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
    ProjectRegistry {
        schema_version: REGISTRY_SCHEMA_VERSION,
        categories,
        domains,
    }
    .normalize(&layout.categories_path, &layout.domains_path)
}

/// Returns whether one category id is safe to use as a canonical path component.
pub fn is_valid_category_id(value: &str) -> bool {
    is_valid_registry_id(value)
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

/// Validates every loaded registry category id against the canonical slug rules.
fn validate_category_ids(path: &Path, categories: &[String]) -> Result<()> {
    for category in categories {
        if !is_valid_category_id(category) {
            return Err(WikiError::InvalidRegistryCategory {
                path: path.to_path_buf(),
                value: category.clone(),
            });
        }
    }
    Ok(())
}

/// Validates every loaded registry domain id against the canonical slug rules.
fn validate_domain_ids(path: &Path, domains: &[String]) -> Result<()> {
    for domain in domains {
        if !is_valid_registry_id(domain) {
            return Err(WikiError::InvalidRegistryDomain {
                path: path.to_path_buf(),
                value: domain.clone(),
            });
        }
    }
    Ok(())
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

/// Validates one registry identifier against the lowercase slug format.
fn is_valid_registry_id(value: &str) -> bool {
    is_valid_slug_id(value)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::ContextWikiLayout;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_test_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "darc-wiki-registry-{label}-{}-{nanos}-{counter}",
            std::process::id()
        ))
    }

    #[test]
    fn load_registry_rejects_unsafe_category_ids() {
        let darc_root = unique_test_dir("invalid-category");
        let layout = ContextWikiLayout::new(&darc_root)
            .project_layout("repo-123")
            .expect("project id should be valid");
        layout.ensure().expect("layout should be created");
        fs::write(
            &layout.categories_path,
            "schema_version = 1\ncategories = [\"../../outside\"]\n",
        )
        .expect("categories file should be written");

        let error = load_registry(&layout).expect_err("unsafe category should be rejected");
        assert!(matches!(error, WikiError::InvalidRegistryCategory { .. }));

        fs::remove_dir_all(&darc_root).expect("temporary test root should be removable");
    }
}
