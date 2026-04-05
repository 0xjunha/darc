mod path_util;

pub mod claude;
pub mod codex;
pub mod model;

use serde::Serialize;

/// Describes whether one parsed rollout used an exact schema match or a forward-compatible fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseDeterminism {
    Exact,
    BestEffortForward,
}

impl ParseDeterminism {
    /// Returns the stable SQLite string value for one rollout determinism level.
    pub const fn as_sql_text(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::BestEffortForward => "best_effort_forward",
        }
    }

    /// Returns whether the selected schema was an exact version match.
    pub(crate) fn is_exact(self) -> bool {
        matches!(self, Self::Exact)
    }
}
