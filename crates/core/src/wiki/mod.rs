mod api;
mod artifacts;
mod context;
mod models;
mod runtime;
mod state;
mod worker;

use std::time::Duration;

pub use api::{
    cancel_project_wiki_digest, ensure_project_wiki, fail_project_wiki_digest_start,
    load_project_wiki, load_project_wiki_run, mark_project_wiki_digest_started,
    prepare_project_wiki_digest_start, run_project_wiki_digest_worker, store_project_wiki_run,
};
pub use models::{
    DigestCancelReport, DigestStartOptions, DigestStartReport, PreparedDigestRun, ProjectWikiData,
};
pub(crate) use state::visible_run_summary;

const RUN_REQUEST_SCHEMA: &str = "darc.wiki.digest.request.v1";
const RUN_CONTEXT_SCHEMA: &str = "darc.wiki.digest.context.v1";
const RUN_RESULT_SCHEMA: &str = "darc.wiki.digest.result.v1";
const RUN_EVENT_LEVEL_INFO: &str = "info";
const RUN_EVENT_LEVEL_WARN: &str = "warn";
const DEFAULT_REQUESTED_BY: &str = "cli";
const RUN_POLL_INTERVAL: Duration = Duration::from_millis(200);
const RUN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const RUN_STALE_TIMEOUT: Duration = Duration::from_secs(5);
const RUNTIME_CANCEL_GRACE_PERIOD: Duration = Duration::from_secs(5);
const WORKER_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_REGISTRATION_POLL_INTERVAL: Duration = Duration::from_millis(50);
const PROPOSAL_SCHEMA_FILE_NAME: &str = "proposal.schema.json";

#[cfg(test)]
mod tests;
