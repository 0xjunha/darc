mod active_project;
pub mod config;
pub(crate) mod constants;
mod index;
mod init;
mod project;
pub mod query;
mod sync;
mod wiki;

pub use config::SourceKind;
pub use darc_wiki::{
    CONTEXT_WIKI_DIR_NAME, ContextWikiLayout, DEFAULT_CATEGORY_IDS, DigestDocument,
    DigestFrontmatter, DigestId, DigestSummary, EntryDocument, EntryFrontmatter, EntryId,
    EntryStatus, EntrySummary, EntryType, ProjectLayout as WikiProjectLayout, ProjectRegistry,
    RUN_STATE_FILE_NAME, RunId, RunPhase, RunState, RunStatus, RunSummary, STORAGE_VERSION,
};
pub use index::{
    IndexOptions, IndexReport, SkippedCodexRollout, SkippedRollout, index_project_codex_turns,
    index_project_sessions,
};
pub use init::{DetectedRolloutSource, InitDraft, default_root_path, prepare_init, write_init};
pub use project::{
    LinkReport, RefreshAllReport, RefreshOptions, RefreshReport, RemoveReport, RenameReport,
    link_project, refresh_all_projects, refresh_project, remove_project, rename_project,
};
pub use sync::{SyncOptions, SyncPlan, SyncReport, execute_sync, prepare_sync};
pub use wiki::{
    DigestCancelReport, DigestStartOptions, DigestStartReport, ProjectWikiData,
    cancel_project_wiki_digest, ensure_project_wiki, load_project_wiki, load_project_wiki_run,
    run_project_wiki_digest_worker, start_project_wiki_digest, store_project_wiki_run,
};
