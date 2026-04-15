mod digests;
mod entries;
mod errors;
mod frontmatter;
mod fs_utils;
mod ids;
mod layout;
mod merge;
mod prompt;
mod proposal;
mod registry;
mod render;
mod runs;

pub use digests::{
    DigestDetailDocument, DigestDocument, DigestFrontmatter, DigestSummary, list_digests,
    load_digest, load_digest_detail,
};
pub use entries::{
    EntryDetailDocument, EntryDocument, EntryFrontmatter, EntryStatus, EntrySummary, EntryType,
    list_entries, load_entry, load_entry_detail,
};
pub use errors::{Result, WikiError};
pub use ids::{DigestId, EntryId, RunId};
pub use layout::{CONTEXT_WIKI_DIR_NAME, ContextWikiLayout, ProjectLayout, STORAGE_VERSION};
pub use merge::{MergeDigestArtifacts, merge_digest_proposal};
pub use prompt::{DigestRuntimePrompt, build_digest_runtime_prompt};
pub use proposal::{
    DIGEST_PROPOSAL_OUTPUT_SCHEMA_JSON, DIGEST_PROPOSAL_SCHEMA, DigestProposal,
    DigestProposalEntry, DigestProposalOption, DigestProposalOptionStatus,
    DigestProposalRunSummary, ProposalEntryOperation, ProposalValidationError,
    ProposalValidationErrors, ProposalValidationOptions, ProposalValidationSummary,
    is_valid_domain_id, validate_digest_proposal,
};
pub use registry::{DEFAULT_CATEGORY_IDS, ProjectRegistry, ensure_registry, load_registry};
pub use runs::{
    RUN_STATE_FILE_NAME, RunPhase, RunState, RunStatus, RunSummary, list_runs, load_run_state,
    store_run_state,
};
