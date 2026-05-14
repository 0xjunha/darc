use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use darc_query::display_path_for_access;

use super::{
    FileSessionSummary, FileUsageStat, FilesQueryData, ProjectInsights, SearchTurnHit,
    SearchTurnsQueryData, SessionBundleQueryData, SessionFileSummary, SessionFilesQueryData,
    SessionSummary, SessionsQueryData, TurnDetail, TurnDetailInsights, TurnInsights,
};
use crate::config::ProjectConfig;

/// Normalizes query output paths against every configured root for one project.
pub(super) struct PathDisplayNormalizer<'a> {
    project: &'a ProjectConfig,
}

impl<'a> PathDisplayNormalizer<'a> {
    /// Creates one normalizer bound to a configured project.
    pub(super) fn new(project: &'a ProjectConfig) -> Self {
        Self { project }
    }

    /// Normalizes session-list file paths against every configured project root.
    pub(super) fn normalize_sessions(&self, data: &mut SessionsQueryData) {
        for session in &mut data.sessions {
            self.normalize_session_summary(session);
        }
    }

    /// Normalizes file-query output paths against every configured project root.
    pub(super) fn normalize_files(&self, data: &mut FilesQueryData) {
        for file in &mut data.files {
            file.path = self.normalize_known_project_path(&file.path);
        }
        for session in &mut data.sessions {
            self.normalize_file_session_summary(session);
        }
    }

    /// Normalizes one session-files payload against every configured project root.
    pub(super) fn normalize_session_files(&self, data: &mut SessionFilesQueryData) {
        for file in &mut data.files {
            self.normalize_session_file_summary(file);
        }
    }

    /// Normalizes one session-bundle payload against every configured project root.
    pub(super) fn normalize_session_bundle(&self, data: &mut SessionBundleQueryData) {
        self.normalize_session_summary(&mut data.session);
        self.normalize_session_files(&mut data.session_files);
        for turn in &mut data.turns {
            self.normalize_turn_detail(turn);
        }
    }

    /// Normalizes one turn-detail payload against every configured project root.
    pub(super) fn normalize_turn_detail(&self, data: &mut TurnDetail) {
        if let Some(insights) = data.insights.as_mut() {
            self.normalize_turn_detail_insights(insights);
        }
    }

    /// Normalizes one turn-insights payload against every configured project root.
    pub(super) fn normalize_turn_insights(&self, data: &mut TurnInsights) {
        self.normalize_file_usage(&mut data.files);
    }

    /// Normalizes search matched paths against every configured project root.
    pub(super) fn normalize_search(&self, data: &mut SearchTurnsQueryData) {
        for hit in &mut data.hits {
            self.normalize_search_hit(hit);
        }
    }

    /// Normalizes one project-insights payload against every configured project root.
    pub(super) fn normalize_project_insights(&self, data: &mut ProjectInsights) {
        self.normalize_file_usage(&mut data.most_read_files);
        self.normalize_file_usage(&mut data.most_written_files);
    }

    /// Normalizes one session summary's edited file paths against every configured project root.
    fn normalize_session_summary(&self, session: &mut SessionSummary) {
        session.edited_files = self.normalize_path_list(std::mem::take(&mut session.edited_files));
    }

    /// Normalizes one file-session summary's matched paths against every configured project root.
    fn normalize_file_session_summary(&self, session: &mut FileSessionSummary) {
        session.matched_paths =
            self.normalize_path_list(std::mem::take(&mut session.matched_paths));
        if !session.matched_paths_truncated {
            session.matched_paths_count =
                u64::try_from(session.matched_paths.len()).unwrap_or(u64::MAX);
        }
    }

    /// Normalizes one session file summary against every configured project root.
    fn normalize_session_file_summary(&self, file: &mut SessionFileSummary) {
        let original = file.path.clone();
        let normalized = self.normalize_known_project_path(&original);
        if file.repo_relative_path.is_none() && normalized != original {
            file.repo_relative_path = Some(normalized.clone());
        }
        file.path = normalized;
        if let Some(repo_relative_path) = file.repo_relative_path.take() {
            file.repo_relative_path = Some(self.normalize_known_project_path(&repo_relative_path));
        }
    }

    /// Normalizes one embedded turn-insights payload against every configured project root.
    fn normalize_turn_detail_insights(&self, insights: &mut TurnDetailInsights) {
        self.normalize_file_usage(&mut insights.files);
    }

    /// Normalizes one search hit's matched paths against every configured project root.
    fn normalize_search_hit(&self, hit: &mut SearchTurnHit) {
        hit.matched_paths = self.normalize_path_list(std::mem::take(&mut hit.matched_paths));
        if !hit.matched_paths_truncated {
            hit.matched_paths_count = u64::try_from(hit.matched_paths.len()).unwrap_or(u64::MAX);
        }
    }

    /// Normalizes file-usage paths against every configured project root.
    fn normalize_file_usage(&self, files: &mut [FileUsageStat]) {
        for file in files {
            file.path = self.normalize_known_project_path(&file.path);
        }
    }

    /// Normalizes and deduplicates one path list against every configured project root.
    fn normalize_path_list(&self, paths: Vec<String>) -> Vec<String> {
        paths
            .into_iter()
            .map(|path| self.normalize_known_project_path(&path))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Converts an absolute path under one configured project root to project-relative text.
    fn normalize_known_project_path(&self, path: &str) -> String {
        let trimmed = path.trim();
        self.project_path_roots()
            .find_map(|root| display_path_for_access(Some(root), None, trimmed))
            .unwrap_or_else(|| trimmed.to_owned())
    }

    /// Iterates over every configured root that can identify this project.
    fn project_path_roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.project.local_path.as_path())
            .chain(self.project.known_paths.iter().map(PathBuf::as_path))
    }
}
