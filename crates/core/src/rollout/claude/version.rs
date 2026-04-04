use std::{cmp::Ordering, fmt};

use super::ClaudeSessionKind;
use crate::rollout::ParseDeterminism;

/// Parses one Claude CLI version string into comparable components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeCliVersion {
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
    pub(crate) fn parse(value: &str) -> anyhow::Result<Self> {
        let (core, prerelease) = match value.split_once('-') {
            Some((core, prerelease)) => (core, Some(prerelease)),
            None => (value, None),
        };
        let mut parts = core.split('.');
        let major = parse_numeric_part(parts.next(), value, "major")?;
        let minor = parse_numeric_part(parts.next(), value, "minor")?;
        let patch = parse_numeric_part(parts.next(), value, "patch")?;
        if parts.next().is_some() {
            anyhow::bail!("unsupported Claude CLI version format `{value}`");
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
    pub(crate) const fn is_stable(&self) -> bool {
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

/// Returns the latest Claude CLI version covered exactly by darc.
pub(crate) const fn latest_exact_supported_claude_cli_version() -> ClaudeCliVersion {
    ClaudeCliVersion::stable(2, 1, 87)
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

/// Returns whether one Claude version is covered exactly by observed transcript fixtures.
fn is_exact_supported_claude_version(version: &ClaudeCliVersion) -> bool {
    matches!(
        (
            version.major,
            version.minor,
            version.patch,
            version.prerelease.as_deref(),
        ),
        (2, 1, 81, None) | (2, 1, 84, None) | (2, 1, 87, None)
    )
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
fn parse_numeric_part(part: Option<&str>, raw_version: &str, label: &str) -> anyhow::Result<u32> {
    let Some(part) = part else {
        anyhow::bail!("unsupported Claude CLI version format `{raw_version}`");
    };
    part.parse().map_err(|error| {
        anyhow::anyhow!("invalid Claude CLI {label} version in `{raw_version}`: {error}")
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ClaudeCliVersion, ClaudeSchemaEpoch, earliest_observed_claude_cli_version,
        latest_exact_supported_claude_cli_version, resolve_claude_schema,
    };
    use crate::rollout::ParseDeterminism;

    #[test]
    fn parses_and_orders_claude_versions() {
        let v1088 = ClaudeCliVersion::parse("1.0.88").unwrap();
        let v287 = ClaudeCliVersion::parse("2.1.87").unwrap();
        let v292 = ClaudeCliVersion::parse("2.1.92").unwrap();

        assert!(v1088 < v287);
        assert!(v287 < v292);
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
            resolve_claude_schema(Some("2.0.28")),
            super::ClaudeSchemaResolution {
                epoch: ClaudeSchemaEpoch::V2_0_8To2_0_28,
                determinism: ParseDeterminism::BestEffortForward,
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
        assert_eq!(
            latest_exact_supported_claude_cli_version().to_string(),
            "2.1.87"
        );
    }
}
