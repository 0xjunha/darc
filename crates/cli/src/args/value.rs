use clap::ValueEnum;
use darc_core::SourceKind;
use darc_rollout_audit::claude::ClaudeSchemaSurveyMode;

/// Represents the supported provider filters for index and sync.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ProviderArg {
    Claude,
    Codex,
}

impl From<ProviderArg> for SourceKind {
    fn from(value: ProviderArg) -> Self {
        match value {
            ProviderArg::Claude => SourceKind::Claude,
            ProviderArg::Codex => SourceKind::Codex,
        }
    }
}

/// Represents the supported search modes for machine-readable turn search.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum SearchModeArg {
    Keyword,
    Literal,
    Regex,
    FileName,
    FilePath,
    PathFragment,
}

/// Represents when query JSON output should include ANSI color.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum ColorArg {
    Auto,
    Always,
    Never,
}

/// Represents the supported session-list projections.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum SessionListViewArg {
    Compact,
    Full,
}

/// Represents the supported local/shared session query scopes.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum SessionScopeArg {
    Local,
    Shared,
    All,
}

/// Represents the supported project sharing policies.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum SharePolicyArg {
    Manual,
    All,
}

/// Represents the supported turn-list projections for machine-readable turn queries.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum TurnListViewArg {
    Full,
    Oneline,
}

/// Represents the supported turn-detail projection modes.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ViewArg {
    Full,
    Narrative,
}

/// Represents the supported Claude schema audit survey modes.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ClaudeSurveyModeArg {
    Refine,
    Coarse,
}

impl From<ClaudeSurveyModeArg> for ClaudeSchemaSurveyMode {
    fn from(value: ClaudeSurveyModeArg) -> Self {
        match value {
            ClaudeSurveyModeArg::Refine => ClaudeSchemaSurveyMode::Refine,
            ClaudeSurveyModeArg::Coarse => ClaudeSchemaSurveyMode::Coarse,
        }
    }
}
