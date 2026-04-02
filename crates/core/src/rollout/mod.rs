use serde::Serialize;

pub(crate) mod codex;

/// Describes whether one parsed rollout used an exact schema match or a forward-compatible fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseDeterminism {
    Exact,
    BestEffortForward,
}

impl ParseDeterminism {
    /// Returns whether the selected schema was an exact version match.
    pub(crate) fn is_exact(self) -> bool {
        matches!(self, Self::Exact)
    }
}
