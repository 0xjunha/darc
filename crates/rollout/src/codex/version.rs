use std::{cmp::Ordering, fmt};

use super::error::{CodexCliVersionParseError, CodexSchemaError};
use crate::ParseDeterminism;

type ParseResult<T> = std::result::Result<T, CodexCliVersionParseError>;
type SchemaResult<T> = std::result::Result<T, CodexSchemaError>;

/// Parses one Codex CLI version string into comparable components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexCliVersion {
    major: u32,
    minor: u32,
    patch: u32,
    prerelease: Option<CodexPrerelease>,
}

impl CodexCliVersion {
    /// Creates one stable semantic version.
    pub(crate) const fn stable(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
            prerelease: None,
        }
    }

    /// Creates one alpha semantic version.
    pub(crate) const fn alpha(major: u32, minor: u32, patch: u32, alpha: u32) -> Self {
        Self {
            major,
            minor,
            patch,
            prerelease: Some(CodexPrerelease::Alpha(alpha)),
        }
    }

    /// Parses a persisted Codex CLI version such as `0.118.0-alpha.2`.
    pub fn parse(value: &str) -> ParseResult<Self> {
        let (core, prerelease) = match value.split_once('-') {
            Some((core, prerelease)) => (core, Some(prerelease)),
            None => (value, None),
        };
        let mut parts = core.split('.');
        let major = parse_numeric_part(parts.next(), value, "major")?;
        let minor = parse_numeric_part(parts.next(), value, "minor")?;
        let patch = parse_numeric_part(parts.next(), value, "patch")?;
        if parts.next().is_some() {
            return Err(CodexCliVersionParseError::InvalidFormat {
                raw_version: value.to_owned(),
            });
        }

        let prerelease = match prerelease {
            None => None,
            Some("") => {
                return Err(CodexCliVersionParseError::InvalidPrereleaseFormat {
                    raw_version: value.to_owned(),
                });
            }
            Some(prerelease) => Some(CodexPrerelease::parse(prerelease, value)?),
        };

        Ok(Self {
            major,
            minor,
            patch,
            prerelease,
        })
    }

    /// Returns whether this parsed version is a stable release.
    pub const fn is_stable(&self) -> bool {
        self.prerelease.is_none()
    }
}

/// Enumerates version-gated Codex rollout features that matter to darc parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexSchemaFeature {
    CompactedLine,
    TurnContextLine,
    MessagePhase,
    StructuredToolOutput,
    TaskLifecycleEvents,
}

impl CodexSchemaFeature {
    /// Returns the first Codex CLI version that supports this rollout feature.
    pub(crate) const fn introduced_in(self) -> CodexCliVersion {
        match self {
            Self::CompactedLine => CodexCliVersion::alpha(0, 35, 0, 3),
            Self::TurnContextLine => CodexCliVersion::alpha(0, 35, 0, 3),
            Self::MessagePhase => CodexCliVersion::stable(0, 95, 0),
            Self::StructuredToolOutput => CodexCliVersion::stable(0, 97, 0),
            Self::TaskLifecycleEvents => CodexCliVersion::alpha(0, 80, 0, 6),
        }
    }
}

/// Returns whether one version supports a specific rollout feature.
pub(crate) fn supports_feature(version: &CodexCliVersion, feature: CodexSchemaFeature) -> bool {
    version >= &feature.introduced_in()
}

/// Returns the latest Codex CLI version covered exactly by darc.
pub const fn latest_exact_supported_codex_cli_version() -> CodexCliVersion {
    CodexCliVersion::stable(0, 118, 0)
}

/// Returns whether one `response_item.type` variant is expected for the given Codex CLI version.
///
/// Historical rollout milestones tracked here:
/// - base variants (`message`, `reasoning`, `local_shell_call`, function/custom tool calls,
///   `web_search_call`) predate the earliest exact-supported epoch at `0.33.0`
/// - `ghost_snapshot` appears in `>=0.51.0`
/// - `compaction` appears in `>=0.59.0`
/// - `image_generation_call` appears in `>=0.108.0`
/// - `tool_search_call` and `tool_search_output` appear in `>=0.115.0`
pub(crate) fn supports_response_item(version: &CodexCliVersion, kind: &str) -> bool {
    match kind {
        "message"
        | "reasoning"
        | "local_shell_call"
        | "function_call"
        | "function_call_output"
        | "custom_tool_call"
        | "custom_tool_call_output"
        | "web_search_call" => true,
        "ghost_snapshot" => version >= &CodexCliVersion::stable(0, 51, 0),
        "compaction" => version >= &CodexCliVersion::stable(0, 59, 0),
        "image_generation_call" => version >= &CodexCliVersion::stable(0, 108, 0),
        "tool_search_call" | "tool_search_output" => version >= &CodexCliVersion::stable(0, 115, 0),
        _ => false,
    }
}

impl Ord for CodexCliVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.prerelease, &other.prerelease) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(left), Some(right)) => left.cmp(right),
            })
    }
}

impl PartialOrd for CodexCliVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for CodexCliVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(prerelease) = &self.prerelease {
            write!(f, "-{prerelease}")?;
        }
        Ok(())
    }
}

/// Stores one parsed prerelease suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexPrerelease {
    Alpha(u32),
    Other(String),
}

impl CodexPrerelease {
    /// Parses one prerelease segment such as `alpha.2`.
    fn parse(value: &str, raw_version: &str) -> ParseResult<Self> {
        if let Some(number) = value.strip_prefix("alpha.") {
            return Ok(Self::Alpha(number.parse().map_err(|source| {
                CodexCliVersionParseError::InvalidAlphaPrerelease {
                    raw_version: raw_version.to_owned(),
                    source,
                }
            })?));
        }
        Ok(Self::Other(value.to_owned()))
    }
}

impl Ord for CodexPrerelease {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Alpha(left), Self::Alpha(right)) => left.cmp(right),
            (Self::Alpha(_), Self::Other(_)) => Ordering::Less,
            (Self::Other(_), Self::Alpha(_)) => Ordering::Greater,
            (Self::Other(left), Self::Other(right)) => left.cmp(right),
        }
    }
}

impl PartialOrd for CodexPrerelease {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for CodexPrerelease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Alpha(number) => write!(f, "alpha.{number}"),
            Self::Other(value) => f.write_str(value),
        }
    }
}

/// Identifies the coarse Codex rollout parser families recorded in darc metadata.
///
/// These schema ids are intentionally broad parser families, not the sole source of exact
/// compatibility truth. Fine-grained support for rollout features and response-item variants is
/// version-gated by `supports_feature()` and `supports_response_item()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexSchemaId {
    /// Earliest supported rollout family before `compacted` and `turn_context` lines existed.
    ///
    /// Supported Codex CLI versions: `>=0.33.0, <0.35.0-alpha.3`.
    Initial,
    /// Rollout family that introduced `compacted` and `turn_context`, before replacement history.
    ///
    /// Supported Codex CLI versions: `>=0.35.0-alpha.3, <0.35.0-alpha.8`.
    CompactionPrelude,
    /// Legacy rollout family with compaction context, but before phased assistant messages.
    ///
    /// Supported Codex CLI versions: `>=0.35.0-alpha.8, <0.95.0`.
    Legacy,
    /// Rollout family with phased assistant messages before structured tool outputs landed.
    ///
    /// Supported Codex CLI versions: `>=0.95.0, <0.97.0`.
    PhasedMessages,
    /// Rollout family with structured tool outputs, before the later turn-lifecycle family.
    ///
    /// Supported Codex CLI versions: `>=0.97.0, <0.104.0-alpha.1`.
    StructuredToolOutput,
    /// Current rollout family used for modern Codex sessions.
    ///
    /// Supported Codex CLI versions: `>=0.104.0-alpha.1, <=0.118.0`.
    ///
    /// Versions newer than `0.118.0` currently map here in `BestEffortForward` mode until a newer
    /// exact family is added.
    TurnLifecycle,
}

impl CodexSchemaId {
    /// Returns the stable string stored on parsed rollouts.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "codex.initial",
            Self::CompactionPrelude => "codex.compaction_prelude",
            Self::Legacy => "codex.legacy",
            Self::PhasedMessages => "codex.phased_messages",
            Self::StructuredToolOutput => "codex.structured_tool_output",
            Self::TurnLifecycle => "codex.turn_lifecycle",
        }
    }
}

/// Describes the selected Codex schema resolution for one rollout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CodexSchemaResolution {
    pub(crate) schema_id: CodexSchemaId,
    pub(crate) determinism: ParseDeterminism,
}

/// Resolves the parser epoch for one Codex CLI version string.
pub(crate) fn resolve_codex_schema(cli_version: &str) -> SchemaResult<CodexSchemaResolution> {
    let version = CodexCliVersion::parse(cli_version).map_err(CodexSchemaError::from)?;

    let schema_id = if version < CodexCliVersion::alpha(0, 35, 0, 3) {
        if version < CodexCliVersion::stable(0, 33, 0) {
            return Err(CodexSchemaError::UnsupportedVersion {
                cli_version: cli_version.to_owned(),
            });
        }
        CodexSchemaId::Initial
    } else if version < CodexCliVersion::alpha(0, 35, 0, 8) {
        CodexSchemaId::CompactionPrelude
    } else if version < CodexCliVersion::stable(0, 95, 0) {
        CodexSchemaId::Legacy
    } else if version < CodexCliVersion::stable(0, 97, 0) {
        CodexSchemaId::PhasedMessages
    } else if version < CodexCliVersion::alpha(0, 104, 0, 1) {
        CodexSchemaId::StructuredToolOutput
    } else {
        CodexSchemaId::TurnLifecycle
    };

    let determinism = if version <= latest_exact_supported_codex_cli_version() {
        ParseDeterminism::Exact
    } else {
        ParseDeterminism::BestEffortForward
    };

    Ok(CodexSchemaResolution {
        schema_id,
        determinism,
    })
}

fn parse_numeric_part(
    part: Option<&str>,
    raw_version: &str,
    label: &'static str,
) -> ParseResult<u32> {
    let Some(part) = part else {
        return Err(CodexCliVersionParseError::InvalidFormat {
            raw_version: raw_version.to_owned(),
        });
    };
    part.parse().map_err(
        |source| CodexCliVersionParseError::InvalidNumericComponent {
            raw_version: raw_version.to_owned(),
            label,
            source,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CodexCliVersion, CodexSchemaFeature, CodexSchemaId,
        latest_exact_supported_codex_cli_version, resolve_codex_schema, supports_feature,
        supports_response_item,
    };
    use crate::ParseDeterminism;

    #[test]
    fn resolves_exact_known_codex_schema_epochs() {
        assert_eq!(
            resolve_codex_schema("0.34.0").unwrap(),
            super::CodexSchemaResolution {
                schema_id: CodexSchemaId::Initial,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_codex_schema("0.35.0-alpha.4").unwrap(),
            super::CodexSchemaResolution {
                schema_id: CodexSchemaId::CompactionPrelude,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_codex_schema("0.72.0").unwrap(),
            super::CodexSchemaResolution {
                schema_id: CodexSchemaId::Legacy,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_codex_schema("0.95.0").unwrap(),
            super::CodexSchemaResolution {
                schema_id: CodexSchemaId::PhasedMessages,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_codex_schema("0.99.0-alpha.5").unwrap(),
            super::CodexSchemaResolution {
                schema_id: CodexSchemaId::StructuredToolOutput,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_codex_schema("0.118.0").unwrap(),
            super::CodexSchemaResolution {
                schema_id: CodexSchemaId::TurnLifecycle,
                determinism: ParseDeterminism::Exact,
            }
        );
    }

    #[test]
    fn resolves_newer_versions_as_best_effort_forward() {
        assert_eq!(
            resolve_codex_schema("0.119.0").unwrap(),
            super::CodexSchemaResolution {
                schema_id: CodexSchemaId::TurnLifecycle,
                determinism: ParseDeterminism::BestEffortForward,
            }
        );
    }

    #[test]
    fn exposes_latest_exact_supported_codex_cli_version() {
        assert_eq!(
            latest_exact_supported_codex_cli_version().to_string(),
            "0.118.0"
        );
    }

    #[test]
    fn gates_response_item_variants_by_version() {
        let v094 = CodexCliVersion::parse("0.94.0").unwrap();
        let v099 = CodexCliVersion::parse("0.99.0-alpha.5").unwrap();
        let v108 = CodexCliVersion::parse("0.108.0").unwrap();
        let v114 = CodexCliVersion::parse("0.114.0").unwrap();
        let v115 = CodexCliVersion::parse("0.115.0").unwrap();

        assert!(supports_response_item(&v094, "web_search_call"));
        assert!(!supports_response_item(&v094, "tool_search_call"));
        assert!(supports_response_item(&v099, "ghost_snapshot"));
        assert!(!supports_response_item(&v099, "image_generation_call"));
        assert!(supports_response_item(&v108, "image_generation_call"));
        assert!(!supports_response_item(&v114, "tool_search_output"));
        assert!(supports_response_item(&v115, "tool_search_output"));
    }

    #[test]
    fn gates_rollout_features_by_version() {
        let v079 = CodexCliVersion::parse("0.79.0").unwrap();
        let v080a6 = CodexCliVersion::parse("0.80.0-alpha.6").unwrap();
        let v094 = CodexCliVersion::parse("0.94.0").unwrap();
        let v095 = CodexCliVersion::parse("0.95.0").unwrap();
        let v096 = CodexCliVersion::parse("0.96.0").unwrap();
        let v097 = CodexCliVersion::parse("0.97.0").unwrap();

        assert!(!supports_feature(
            &v079,
            CodexSchemaFeature::TaskLifecycleEvents
        ));
        assert!(supports_feature(
            &v080a6,
            CodexSchemaFeature::TaskLifecycleEvents
        ));
        assert!(!supports_feature(&v094, CodexSchemaFeature::MessagePhase));
        assert!(supports_feature(&v095, CodexSchemaFeature::MessagePhase));
        assert!(!supports_feature(
            &v096,
            CodexSchemaFeature::StructuredToolOutput
        ));
        assert!(supports_feature(
            &v097,
            CodexSchemaFeature::StructuredToolOutput
        ));
    }
}
