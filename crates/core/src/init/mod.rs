mod config_io;
mod detect;
mod project_config;
#[cfg(test)]
mod tests;
mod types;
mod workflow;

pub(crate) use project_config::{normalize_project_config, project_id_from_path};
pub use types::{DetectedRolloutSource, InitDraft};
pub use workflow::{default_root_path, prepare_init, write_init};
