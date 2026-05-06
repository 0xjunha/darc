use std::{cmp::Ordering, fmt};

use super::ClaudeSessionKind;
use super::error::ClaudeCliVersionParseError;
use crate::ParseDeterminism;

type Result<T> = std::result::Result<T, ClaudeCliVersionParseError>;

/// Parses one Claude CLI version string into comparable components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCliVersion {
    major: u32,
    minor: u32,
    patch: u32,
    prerelease: Option<String>,
}

impl ClaudeCliVersion {
    /// Creates one stable semantic version.
    pub(crate) const fn stable(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
            prerelease: None,
        }
    }

    /// Parses one persisted Claude CLI version such as `2.1.87`.
    pub fn parse(value: &str) -> Result<Self> {
        let (core, prerelease) = match value.split_once('-') {
            Some((core, prerelease)) => (core, Some(prerelease)),
            None => (value, None),
        };
        let mut parts = core.split('.');
        let major = parse_numeric_part(parts.next(), value, "major")?;
        let minor = parse_numeric_part(parts.next(), value, "minor")?;
        let patch = parse_numeric_part(parts.next(), value, "patch")?;
        if parts.next().is_some() {
            return Err(ClaudeCliVersionParseError::InvalidFormat {
                raw_version: value.to_owned(),
            });
        }

        Ok(Self {
            major,
            minor,
            patch,
            prerelease: prerelease
                .filter(|prerelease| !prerelease.is_empty())
                .map(str::to_owned),
        })
    }

    /// Returns whether this parsed version is a stable release.
    pub const fn is_stable(&self) -> bool {
        self.prerelease.is_none()
    }
}

/// Identifies one provisional Claude transcript parser family derived from coarse survey data.
///
/// These epochs intentionally track coarse observed windows, not exact schema boundaries. Exact
/// support remains version-specific through `ParseDeterminism`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeSchemaEpoch {
    /// Earliest practical live Claude transcript family.
    ///
    /// Observed Claude CLI versions: `1.0.88 ..= 2.0.5`.
    V1_0_88To2_0_5,
    /// Claude transcript family observed after the first sampled `2.0.x` drift.
    ///
    /// Observed Claude CLI versions: `2.0.8 ..= 2.0.28`.
    V2_0_8To2_0_28,
    /// Claude transcript family observed across later `2.0.x` Task-style delegation sessions.
    ///
    /// Observed Claude CLI versions: `2.0.29 ..= 2.0.52`.
    V2_0_29To2_0_52,
    /// Claude transcript family observed across the next `2.0.x` drift window.
    ///
    /// Observed Claude CLI versions: `2.0.53 ..= 2.0.72`.
    V2_0_53To2_0_72,
    /// Claude transcript family observed through early `2.1.x`.
    ///
    /// Observed Claude CLI versions: `2.0.73 ..= 2.1.15`.
    V2_0_73To2_1_15,
    /// Claude transcript family observed before the next sampled `2.1.x` drift window.
    ///
    /// Observed Claude CLI versions: `2.1.16 ..= 2.1.37`.
    V2_1_16To2_1_37,
    /// Claude transcript family observed across mid-`2.1.x`.
    ///
    /// Observed Claude CLI versions: `2.1.38 ..= 2.1.61`.
    V2_1_38To2_1_61,
    /// Claude transcript family observed immediately before the modern exact baseline window.
    ///
    /// Observed Claude CLI versions: `2.1.62 ..= 2.1.83`.
    V2_1_62To2_1_83,
    /// Modern Claude transcript family beginning at the exact baseline window and ending before
    /// the top-level `attachment` drift.
    ///
    /// Observed Claude CLI versions: `2.1.84 ..= 2.1.89`.
    V2_1_84To2_1_89,
    /// Current late-modern Claude transcript family beginning at the refined `attachment` drift.
    ///
    /// Observed Claude CLI versions: `>= 2.1.90`.
    ///
    /// Versions newer than the latest exact-supported release currently map here in
    /// `BestEffortForward` mode until a narrower modern family is carved out.
    V2_1_90ToLatest,
}

impl ClaudeSchemaEpoch {
    /// Returns the stable string stored on parsed Claude rollouts for one session kind.
    pub(crate) const fn schema_id(self, session_kind: ClaudeSessionKind) -> &'static str {
        match session_kind {
            ClaudeSessionKind::Primary => match self {
                Self::V1_0_88To2_0_5 => "claude.primary_transcript.1_0_88_to_2_0_5",
                Self::V2_0_8To2_0_28 => "claude.primary_transcript.2_0_8_to_2_0_28",
                Self::V2_0_29To2_0_52 => "claude.primary_transcript.2_0_29_to_2_0_52",
                Self::V2_0_53To2_0_72 => "claude.primary_transcript.2_0_53_to_2_0_72",
                Self::V2_0_73To2_1_15 => "claude.primary_transcript.2_0_73_to_2_1_15",
                Self::V2_1_16To2_1_37 => "claude.primary_transcript.2_1_16_to_2_1_37",
                Self::V2_1_38To2_1_61 => "claude.primary_transcript.2_1_38_to_2_1_61",
                Self::V2_1_62To2_1_83 => "claude.primary_transcript.2_1_62_to_2_1_83",
                Self::V2_1_84To2_1_89 => "claude.primary_transcript.2_1_84_to_2_1_89",
                Self::V2_1_90ToLatest => "claude.primary_transcript.2_1_90_to_latest",
            },
            ClaudeSessionKind::Subagent => match self {
                Self::V1_0_88To2_0_5 => "claude.subagent_transcript.1_0_88_to_2_0_5",
                Self::V2_0_8To2_0_28 => "claude.subagent_transcript.2_0_8_to_2_0_28",
                Self::V2_0_29To2_0_52 => "claude.subagent_transcript.2_0_29_to_2_0_52",
                Self::V2_0_53To2_0_72 => "claude.subagent_transcript.2_0_53_to_2_0_72",
                Self::V2_0_73To2_1_15 => "claude.subagent_transcript.2_0_73_to_2_1_15",
                Self::V2_1_16To2_1_37 => "claude.subagent_transcript.2_1_16_to_2_1_37",
                Self::V2_1_38To2_1_61 => "claude.subagent_transcript.2_1_38_to_2_1_61",
                Self::V2_1_62To2_1_83 => "claude.subagent_transcript.2_1_62_to_2_1_83",
                Self::V2_1_84To2_1_89 => "claude.subagent_transcript.2_1_84_to_2_1_89",
                Self::V2_1_90ToLatest => "claude.subagent_transcript.2_1_90_to_latest",
            },
        }
    }

    /// Returns whether this epoch relies on historical text-only completion fallback.
    pub(crate) const fn uses_text_completion_fallback(self) -> bool {
        !matches!(self, Self::V2_1_84To2_1_89 | Self::V2_1_90ToLatest)
    }

    /// Returns whether this epoch recognizes top-level `attachment` lines natively.
    pub(crate) const fn supports_attachment_line(self) -> bool {
        matches!(self, Self::V2_1_90ToLatest)
    }
}

/// Describes the selected Claude schema resolution for one rollout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClaudeSchemaResolution {
    pub(crate) epoch: ClaudeSchemaEpoch,
    pub(crate) determinism: ParseDeterminism,
}

/// Returns the earliest Claude CLI version observed well enough to anchor transcript epochs.
pub(crate) const fn earliest_observed_claude_cli_version() -> ClaudeCliVersion {
    ClaudeCliVersion::stable(1, 0, 88)
}

/// Returns the highest Claude CLI version covered exactly by darc.
pub const fn latest_exact_supported_claude_cli_version() -> ClaudeCliVersion {
    exact_supported_claude_version(EXACT_SUPPORTED_CLAUDE_STABLE_VERSIONS.len() - 1)
}

/// Resolves the parse determinism expected for one Claude CLI version.
pub fn resolve_claude_parse_determinism(cli_version: Option<&str>) -> ParseDeterminism {
    resolve_claude_schema(cli_version).determinism
}

/// Resolves the parser epoch for one Claude CLI version string.
pub(crate) fn resolve_claude_schema(cli_version: Option<&str>) -> ClaudeSchemaResolution {
    let Some(cli_version) = cli_version else {
        return ClaudeSchemaResolution {
            epoch: ClaudeSchemaEpoch::V2_1_90ToLatest,
            determinism: ParseDeterminism::BestEffortForward,
        };
    };

    let Ok(version) = ClaudeCliVersion::parse(cli_version) else {
        return ClaudeSchemaResolution {
            epoch: ClaudeSchemaEpoch::V2_1_90ToLatest,
            determinism: ParseDeterminism::BestEffortForward,
        };
    };

    let epoch = resolve_claude_epoch(&version);
    let determinism = if version < earliest_observed_claude_cli_version() {
        ParseDeterminism::BestEffortForward
    } else if is_exact_supported_claude_version(&version) {
        ParseDeterminism::Exact
    } else {
        ParseDeterminism::BestEffortForward
    };

    ClaudeSchemaResolution { epoch, determinism }
}

/// Maps one parsed Claude version onto the coarse transcript epoch families.
fn resolve_claude_epoch(version: &ClaudeCliVersion) -> ClaudeSchemaEpoch {
    if version < &ClaudeCliVersion::stable(2, 0, 8) {
        ClaudeSchemaEpoch::V1_0_88To2_0_5
    } else if version < &ClaudeCliVersion::stable(2, 0, 29) {
        ClaudeSchemaEpoch::V2_0_8To2_0_28
    } else if version < &ClaudeCliVersion::stable(2, 0, 53) {
        ClaudeSchemaEpoch::V2_0_29To2_0_52
    } else if version < &ClaudeCliVersion::stable(2, 0, 73) {
        ClaudeSchemaEpoch::V2_0_53To2_0_72
    } else if version < &ClaudeCliVersion::stable(2, 1, 16) {
        ClaudeSchemaEpoch::V2_0_73To2_1_15
    } else if version < &ClaudeCliVersion::stable(2, 1, 38) {
        ClaudeSchemaEpoch::V2_1_16To2_1_37
    } else if version < &ClaudeCliVersion::stable(2, 1, 62) {
        ClaudeSchemaEpoch::V2_1_38To2_1_61
    } else if version < &ClaudeCliVersion::stable(2, 1, 84) {
        ClaudeSchemaEpoch::V2_1_62To2_1_83
    } else if version < &ClaudeCliVersion::stable(2, 1, 90) {
        ClaudeSchemaEpoch::V2_1_84To2_1_89
    } else {
        ClaudeSchemaEpoch::V2_1_90ToLatest
    }
}

/// Lists stable Claude Code releases covered exactly by checked or live audit fixtures.
const EXACT_SUPPORTED_CLAUDE_STABLE_VERSIONS: &[(u32, u32, u32)] = &[
    (1, 0, 91),
    (1, 0, 105),
    (1, 0, 115),
    (1, 0, 126),
    (2, 0, 10),
    (2, 0, 21),
    (2, 0, 22),
    (2, 0, 31),
    (2, 0, 44),
    (2, 1, 7),
    (2, 1, 18),
    (2, 1, 29),
    (2, 1, 40),
    (2, 1, 52),
    (2, 1, 64),
    (2, 1, 75),
    (2, 1, 81),
    (2, 1, 84),
    (2, 1, 85),
    (2, 1, 86),
    (2, 1, 87),
    (2, 1, 100),
    (2, 1, 113),
    (2, 1, 124),
    (2, 1, 126),
    (2, 1, 128),
];

/// Returns one exact-supported Claude version from the canonical table.
const fn exact_supported_claude_version(index: usize) -> ClaudeCliVersion {
    let (major, minor, patch) = EXACT_SUPPORTED_CLAUDE_STABLE_VERSIONS[index];
    ClaudeCliVersion::stable(major, minor, patch)
}

/// Returns whether one Claude version is covered exactly by observed transcript fixtures.
fn is_exact_supported_claude_version(version: &ClaudeCliVersion) -> bool {
    version.prerelease.is_none()
        && EXACT_SUPPORTED_CLAUDE_STABLE_VERSIONS
            .iter()
            .any(|&(major, minor, patch)| {
                (version.major, version.minor, version.patch) == (major, minor, patch)
            })
}

impl Ord for ClaudeCliVersion {
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

impl PartialOrd for ClaudeCliVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for ClaudeCliVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(prerelease) = &self.prerelease {
            write!(f, "-{prerelease}")?;
        }
        Ok(())
    }
}

/// Parses one required numeric version segment.
fn parse_numeric_part(part: Option<&str>, raw_version: &str, label: &'static str) -> Result<u32> {
    let Some(part) = part else {
        return Err(ClaudeCliVersionParseError::InvalidFormat {
            raw_version: raw_version.to_owned(),
        });
    };
    part.parse().map_err(
        |source| ClaudeCliVersionParseError::InvalidNumericComponent {
            raw_version: raw_version.to_owned(),
            label,
            source,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ClaudeCliVersion, ClaudeSchemaEpoch, earliest_observed_claude_cli_version,
        latest_exact_supported_claude_cli_version, resolve_claude_parse_determinism,
        resolve_claude_schema,
    };
    use crate::ParseDeterminism;

    #[test]
    fn parses_and_orders_claude_versions() {
        let v1088 = ClaudeCliVersion::parse("1.0.88").unwrap();
        let v287 = ClaudeCliVersion::parse("2.1.87").unwrap();
        let v2126 = ClaudeCliVersion::parse("2.1.126").unwrap();

        assert!(v1088 < v287);
        assert!(v287 < v2126);
        assert!(ClaudeCliVersion::parse("2.1.87-beta.1").unwrap() < v287);
    }

    #[test]
    fn resolves_provisional_claude_schema_epochs() {
        assert_eq!(
            resolve_claude_schema(Some("1.0.88")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V1_0_88To2_0_5,
                determinism: ParseDeterminism::BestEffortForward,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("1.0.91")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V1_0_88To2_0_5,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.0.28")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_0_8To2_0_28,
                determinism: ParseDeterminism::BestEffortForward,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.0.22")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_0_8To2_0_28,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.0.52")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_0_29To2_0_52,
                determinism: ParseDeterminism::BestEffortForward,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.0.55")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_0_53To2_0_72,
                determinism: ParseDeterminism::BestEffortForward,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.1.15")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_0_73To2_1_15,
                determinism: ParseDeterminism::BestEffortForward,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.1.83")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_1_62To2_1_83,
                determinism: ParseDeterminism::BestEffortForward,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.1.84")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_1_84To2_1_89,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.1.87")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_1_84To2_1_89,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.1.85")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_1_84To2_1_89,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.1.86")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_1_84To2_1_89,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.1.89")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_1_84To2_1_89,
                determinism: ParseDeterminism::BestEffortForward,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.1.92")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_1_90ToLatest,
                determinism: ParseDeterminism::BestEffortForward,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.1.100")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_1_90ToLatest,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.1.126")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_1_90ToLatest,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("2.1.128")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_1_90ToLatest,
                determinism: ParseDeterminism::Exact,
            }
        );
        assert_eq!(
            resolve_claude_schema(Some("bad-version")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_1_90ToLatest,
                determinism: ParseDeterminism::BestEffortForward,
            }
        );
    }

    #[test]
    fn exposes_epoch_capabilities() {
        assert!(ClaudeSchemaEpoch::V1_0_88To2_0_5.uses_text_completion_fallback());
        assert!(!ClaudeSchemaEpoch::V2_1_84To2_1_89.uses_text_completion_fallback());
        assert!(!ClaudeSchemaEpoch::V2_1_90ToLatest.uses_text_completion_fallback());
        assert!(!ClaudeSchemaEpoch::V2_1_62To2_1_83.supports_attachment_line());
        assert!(!ClaudeSchemaEpoch::V2_1_84To2_1_89.supports_attachment_line());
        assert!(ClaudeSchemaEpoch::V2_1_90ToLatest.supports_attachment_line());
    }

    #[test]
    fn exposes_exact_coverage_boundaries() {
        assert_eq!(earliest_observed_claude_cli_version().to_string(), "1.0.88");
        for versions in super::EXACT_SUPPORTED_CLAUDE_STABLE_VERSIONS.windows(2) {
            let previous = ClaudeCliVersion::stable(versions[0].0, versions[0].1, versions[0].2);
            let current = ClaudeCliVersion::stable(versions[1].0, versions[1].1, versions[1].2);
            assert!(
                previous < current,
                "exact Claude versions must stay sorted: {previous} then {current}"
            );
        }
        assert_eq!(
            latest_exact_supported_claude_cli_version().to_string(),
            "2.1.128"
        );
    }

    #[test]
    fn exposes_expected_parse_determinism() {
        assert_eq!(
            resolve_claude_parse_determinism(Some("2.1.128")),
            ParseDeterminism::Exact
        );
        assert_eq!(
            resolve_claude_parse_determinism(Some("2.1.127")),
            ParseDeterminism::BestEffortForward
        );
    }
}
