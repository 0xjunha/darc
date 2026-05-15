use anyhow::{Context, Result, anyhow, bail};
use darc_core::SourceKind;
use darc_core::query::{
    DEFAULT_MATCHED_PATH_LIMIT, DEFAULT_QUERY_PAGE_LIMIT, DEFAULT_RESOLVE_SESSION_MATCH_LIMIT,
    DEFAULT_SEARCH_MATCH_LIMIT, ProjectFilesQueryRequest, ProjectSearchTurnsQueryRequest,
    ProjectSessionBundleQueryRequest, ProjectSessionsQueryRequest, ProjectTurnsQueryRequest,
    QueryProtocolError, ResolveSessionQueryRequest, ResolvedQueryProject, ResolvedSessionMatch,
    SearchEvidenceField, SearchMode, SessionBundleView, SessionOriginScope, SessionsView,
    TurnDetailOptions, TurnsView, query_project_insight_report_for_project, query_resolve_sessions,
    query_session_files_for_project, query_turn_for_project, query_turn_insight_report_for_project,
    query_workspace, query_workspace_insight_report, resolve_query_project,
    resolve_query_search_session_id_for_project_with_scope,
    resolve_query_session_for_project_with_scope,
};
use darc_paths::resolve_query_time_bound as resolve_shared_query_time_bound;
use serde::Serialize;

use crate::args::{
    ListArgs, ListCommands, ListFilesArgs, ListSessionsArgs, ProviderArg, QueryFilesArgs,
    QueryProjectInsightsArgs, QueryResolveSessionArgs, QuerySearchTurnsArgs,
    QuerySessionBundleArgs, QuerySessionFilesArgs, QuerySessionsArgs, QueryTurnArgs,
    QueryTurnInsightsArgs, QueryTurnsArgs, QueryWorkspaceArgs, QueryWorkspaceInsightsArgs,
    ResolveArgs, ResolveCommands, SearchArgs, SearchModeArg, SessionListViewArg, SessionScopeArg,
    ShowArgs, ShowCommands, StatsArgs, StatsCommands, TurnListViewArg, ViewArg,
};
use crate::output::{
    QueryOutput, ReadValidationError, print_json_envelope, print_search_turns_json_envelope,
    print_turns_query_envelope,
};

/// Dispatches the supported canonical list commands.
pub(crate) fn run_list(args: ListArgs) -> Result<()> {
    let output = QueryOutput::new(args.color);
    let root = args.root;
    match args.command {
        ListCommands::Projects(mut args) => {
            args.root = root;
            run_query_workspace(&output, args)
        }
        ListCommands::Sessions(mut args) => {
            args.root = root;
            run_query_sessions(&output, args.into())
        }
        ListCommands::Turns(mut args) => {
            args.root = root;
            run_query_turns(&output, args)
        }
        ListCommands::Files(mut args) => {
            args.root = root;
            run_list_files(&output, args)
        }
    }
}

/// Dispatches the supported canonical show commands.
pub(crate) fn run_show(args: ShowArgs) -> Result<()> {
    let output = QueryOutput::new(args.color);
    let root = args.root;
    match args.command {
        ShowCommands::Workspace(mut args) => {
            args.root = root;
            run_query_workspace(&output, args)
        }
        ShowCommands::Session(mut args) => {
            args.root = root;
            run_query_session_bundle(&output, args)
        }
        ShowCommands::Turn(mut args) => {
            args.root = root;
            run_query_turn(&output, args)
        }
    }
}

/// Dispatches canonical turn search.
pub(crate) fn run_search(args: SearchArgs) -> Result<()> {
    let output = QueryOutput::new(args.color);
    run_query_search_turns(&output, args.into_query_search_turns_args()?)
}

/// Dispatches the supported canonical stats commands.
pub(crate) fn run_stats(args: StatsArgs) -> Result<()> {
    let output = QueryOutput::new(args.color);
    let root = args.root;
    match args.command {
        StatsCommands::Workspace(mut args) => {
            args.root = root;
            run_query_workspace_insights(&output, args)
        }
        StatsCommands::Project(mut args) => {
            args.root = root;
            run_query_project_insights(&output, args)
        }
        StatsCommands::Turn(mut args) => {
            args.root = root;
            run_query_turn_insights(&output, args)
        }
    }
}

/// Dispatches the supported canonical resolver commands.
pub(crate) fn run_resolve(args: ResolveArgs) -> Result<()> {
    let output = QueryOutput::new(args.color);
    let root = args.root;
    match args.command {
        ResolveCommands::Session(mut args) => {
            args.root = root;
            run_query_resolve_session(&output, args)
        }
    }
}

/// Lists files through either project-wide or session-scoped query payloads.
pub(crate) fn run_list_files(output: &QueryOutput, args: ListFilesArgs) -> Result<()> {
    let path_selector_count =
        usize::from(args.path.is_some()) + usize::from(args.path_arg.is_some());
    let selector_count = path_selector_count
        + usize::from(args.session.is_some())
        + usize::from(args.co_touched_with.is_some());
    if selector_count > 1 {
        bail!("list files accepts at most one of PATH/--path, --session, or --co-touched-with");
    }
    let path = args.path.or(args.path_arg);
    if let Some(session_id) = args.session {
        if args.since.is_some()
            || args.until.is_some()
            || args.matched_path_limit.is_some()
            || args.include_all_matched_paths
        {
            bail!(
                "list files --session does not accept --since, --until, --matched-path-limit, or --include-all-matched-paths"
            );
        }
        return run_query_session_files(
            output,
            QuerySessionFilesArgs {
                root: args.root,
                project_id: args.project_id,
                provider: args.provider,
                shared: false,
                scope: None,
                session_id_arg: None,
                session_id: Some(session_id),
                limit: args.limit.unwrap_or(DEFAULT_QUERY_PAGE_LIMIT),
                offset: args.offset.unwrap_or(0),
            },
        );
    }
    if path.is_none() && (args.matched_path_limit.is_some() || args.include_all_matched_paths) {
        bail!("list files matched-path controls require PATH or --path");
    }
    run_query_files(
        output,
        QueryFilesArgs {
            root: args.root,
            project_id: args.project_id,
            provider: args.provider,
            path,
            path_arg: None,
            co_touched_with: args.co_touched_with,
            since: args.since,
            until: args.until,
            limit: args.limit.unwrap_or(DEFAULT_QUERY_PAGE_LIMIT),
            offset: args.offset.unwrap_or(0),
            matched_path_limit: args
                .matched_path_limit
                .unwrap_or(DEFAULT_MATCHED_PATH_LIMIT),
            include_all_matched_paths: args.include_all_matched_paths,
        },
    )
}

/// Queries the workspace/sidebar payload for one darc root.
pub(crate) fn run_query_workspace(output: &QueryOutput, args: QueryWorkspaceArgs) -> Result<()> {
    print_json_envelope(
        output,
        "darc.query.workspace.v1",
        &query_workspace(Some(args.root)),
    )
}

/// Resolves one full session id or UUID prefix into canonical matches.
pub(crate) fn run_query_resolve_session(
    output: &QueryOutput,
    args: QueryResolveSessionArgs,
) -> Result<()> {
    let data = query_resolve_sessions(
        Some(args.root),
        ResolveSessionQueryRequest {
            query: &args.input,
            project_id: args.project_id.as_deref(),
            provider: args.provider.map(provider_arg_to_source_kind),
            limit: DEFAULT_RESOLVE_SESSION_MATCH_LIMIT,
        },
    )?;
    if !args.pick_one {
        if data.matches.is_empty() && is_full_uuid_text(&data.query) {
            return Err(QueryProtocolError::unknown_resolve_session(&data.query, false).into());
        }
        return print_json_envelope(output, "darc.query.resolve_session.v1", &data);
    }

    match data.matches.as_slice() {
        [] => Err(QueryProtocolError::unknown_resolve_session(
            &data.query,
            !is_full_uuid_text(&data.query),
        )
        .into()),
        [resolved] => print_json_envelope(
            output,
            "darc.query.resolve_session.v1",
            &ResolveSessionPickOneQueryData::new(&data.query, resolved.clone()),
        ),
        _ => Err(
            QueryProtocolError::ambiguous_session(&data.query, data.matches, data.truncated).into(),
        ),
    }
}

/// Queries the session list for one configured project.
pub(crate) fn run_query_sessions(output: &QueryOutput, args: QuerySessionsArgs) -> Result<()> {
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let since = args
        .since
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let until = args
        .until
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let data = project.query_sessions(ProjectSessionsQueryRequest {
        provider: args.provider.map(provider_arg_to_source_kind),
        since: since.as_deref(),
        until: until.as_deref(),
        touched_path: args.touched_path.as_deref(),
        origin_scope: query_origin_scope(args.shared, args.scope, args.author.as_deref()),
        author: args.author.as_deref(),
        view: session_list_view_arg_to_view(args.view),
        limit: args.limit,
        offset: args.offset,
    })?;
    print_json_envelope(output, "darc.query.sessions.v1", &data)
}

/// Lists most-touched files or pivots from one file selector for one configured project.
pub(crate) fn run_query_files(output: &QueryOutput, args: QueryFilesArgs) -> Result<()> {
    let path = optional_named_or_positional(
        "file path",
        "--path",
        args.path.as_deref(),
        "PATH",
        args.path_arg.as_deref(),
    )?;
    if path.is_some() && args.co_touched_with.is_some() {
        bail!("list files accepts either PATH/--path or --co-touched-with, not both");
    }
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let since = args
        .since
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let until = args
        .until
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let data = project.query_files(ProjectFilesQueryRequest {
        provider: args.provider.map(provider_arg_to_source_kind),
        path,
        co_touched_with: args.co_touched_with.as_deref(),
        since: since.as_deref(),
        until: until.as_deref(),
        limit: args.limit,
        offset: args.offset,
        matched_path_limit: matched_path_limit_arg(
            args.include_all_matched_paths,
            args.matched_path_limit,
        ),
    })?;
    print_json_envelope(output, "darc.query.files.v1", &data)
}

/// Queries one session-scoped per-file access summary payload.
pub(crate) fn run_query_session_files(
    output: &QueryOutput,
    args: QuerySessionFilesArgs,
) -> Result<()> {
    let session_id = required_named_or_positional(
        "session id",
        "--session-id",
        args.session_id.as_deref(),
        "SESSION_ID",
        args.session_id_arg.as_deref(),
    )?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let origin_scope = query_origin_scope(args.shared, args.scope, None);
    let session = resolve_query_session_for_project_with_scope(
        &project,
        args.provider.map(provider_arg_to_source_kind),
        session_id,
        origin_scope,
    )?;
    let data = query_session_files_for_project(
        &project,
        session.provider,
        &session.session_id,
        args.limit,
        args.offset,
    )?;
    print_json_envelope(output, "darc.query.session_files.v1", &data)
}

/// Queries one composite session bundle payload.
pub(crate) fn run_query_session_bundle(
    output: &QueryOutput,
    args: QuerySessionBundleArgs,
) -> Result<()> {
    let session_id = required_named_or_positional(
        "session id",
        "--session-id",
        args.session_id.as_deref(),
        "SESSION_ID",
        args.session_id_arg.as_deref(),
    )?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let origin_scope = query_origin_scope(args.shared, args.scope, None);
    let session = resolve_query_session_for_project_with_scope(
        &project,
        args.provider.map(provider_arg_to_source_kind),
        session_id,
        origin_scope,
    )?;
    let data = project.query_session_bundle(ProjectSessionBundleQueryRequest {
        provider: session.provider,
        session_id: &session.session_id,
        session_view: session_list_view_arg_to_view(args.session_view),
        view: view_arg_to_session_bundle_view(args.view),
        turn_limit: args.turn_limit,
        turn_offset: args.turn_offset,
        step_limit: args.step_limit,
        step_offset: args.step_offset,
    })?;
    print_json_envelope(output, "darc.query.session_bundle.v1", &data)
}

/// Queries the turn list for one session.
pub(crate) fn run_query_turns(output: &QueryOutput, args: QueryTurnsArgs) -> Result<()> {
    let session_id = required_named_or_positional(
        "session id",
        "--session-id",
        args.session_id.as_deref(),
        "SESSION_ID",
        args.session_id_arg.as_deref(),
    )?;
    let since = args
        .since
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let until = args
        .until
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let origin_scope = query_origin_scope(args.shared, args.scope, None);
    let session = resolve_query_session_for_project_with_scope(
        &project,
        args.provider.map(provider_arg_to_source_kind),
        session_id,
        origin_scope,
    )?;
    let data = project.query_turns(ProjectTurnsQueryRequest {
        provider: session.provider,
        session_id: &session.session_id,
        since: since.as_deref(),
        until: until.as_deref(),
        view: turn_list_view_arg_to_view(args.view),
        limit: args.limit,
        offset: args.offset,
    })?;
    print_turns_query_envelope(output, &data)
}

/// Queries one turn detail payload.
pub(crate) fn run_query_turn(output: &QueryOutput, args: QueryTurnArgs) -> Result<()> {
    let (session_id, turn_ordinal) = resolve_turn_identity_args(
        args.session_id.as_deref(),
        args.turn_ordinal,
        args.session_id_arg.as_deref(),
        args.turn_ordinal_arg.as_deref(),
    )?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let origin_scope = query_origin_scope(args.shared, args.scope, None);
    let session = resolve_query_session_for_project_with_scope(
        &project,
        args.provider.map(provider_arg_to_source_kind),
        session_id,
        origin_scope,
    )?;
    let view = match (args.view, args.include_raw) {
        (Some(ViewArg::Narrative), true) => {
            bail!("--include-raw requires --view full; omit --view to let --include-raw imply full")
        }
        (Some(view), _) => view,
        (None, true) => ViewArg::Full,
        (None, false) => ViewArg::Narrative,
    };
    let data = query_turn_for_project(
        &project,
        session.provider,
        &session.session_id,
        turn_ordinal,
        TurnDetailOptions {
            include_raw: args.include_raw,
            include_insights: args.include_insights,
            narrative: matches!(view, ViewArg::Narrative),
            step_limit: args.step_limit,
            step_offset: args.step_offset,
        },
    )?;
    print_json_envelope(output, "darc.query.turn.v1", &data)
}

/// Queries one paginated turn-search payload.
pub(crate) fn run_query_search_turns(
    output: &QueryOutput,
    args: QuerySearchTurnsArgs,
) -> Result<()> {
    let query = required_named_or_positional(
        "query text",
        "--query",
        args.query.as_deref(),
        "QUERY",
        args.query_arg.as_deref(),
    )?;
    let since = args
        .since
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let until = args
        .until
        .as_deref()
        .map(resolve_query_time_bound)
        .transpose()?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let origin_scope = query_origin_scope(args.shared, args.scope, args.author.as_deref());
    let session_id = args
        .session_id
        .as_deref()
        .map(|session_id| {
            resolve_query_search_session_id_for_project_with_scope(
                &project,
                args.provider.map(provider_arg_to_source_kind),
                session_id,
                origin_scope,
            )
        })
        .transpose()?;
    let mode = search_mode_arg_to_search_mode(args.mode);
    let data = project.query_search_turns(ProjectSearchTurnsQueryRequest {
        mode,
        query,
        include_tool_output: args.include_tool_output,
        fields: &args.fields,
        excluded_fields: &args.excluded_fields,
        provider: args.provider.map(provider_arg_to_source_kind),
        session_id: session_id.as_deref(),
        since: since.as_deref(),
        until: until.as_deref(),
        origin_scope,
        author: args.author.as_deref(),
        limit: args.limit,
        offset: args.offset,
        matched_path_limit: matched_path_limit_arg(
            args.include_all_matched_paths,
            args.matched_path_limit,
        ),
        match_limit: args.match_limit,
    })?;
    print_search_turns_json_envelope(output, &data)
}

/// Queries the workspace insights payload for one rolling host-local day window.
pub(crate) fn run_query_workspace_insights(
    output: &QueryOutput,
    args: QueryWorkspaceInsightsArgs,
) -> Result<()> {
    let data = query_workspace_insight_report(
        Some(args.root),
        args.window_days,
        args.recent_session_limit,
        args.recent_session_offset,
    )?;
    print_json_envelope(output, "darc.query.insights.workspace.v1", &data)
}

/// Queries the project insights payload for one configured project.
pub(crate) fn run_query_project_insights(
    output: &QueryOutput,
    args: QueryProjectInsightsArgs,
) -> Result<()> {
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let data = query_project_insight_report_for_project(
        &project,
        args.provider.map(provider_arg_to_source_kind),
        args.turn_limit,
    )?;
    print_json_envelope(output, "darc.query.insights.project.v1", &data)
}

/// Queries the turn insights payload for one session turn.
pub(crate) fn run_query_turn_insights(
    output: &QueryOutput,
    args: QueryTurnInsightsArgs,
) -> Result<()> {
    let (session_id, turn_ordinal) = resolve_turn_identity_args(
        args.session_id.as_deref(),
        args.turn_ordinal,
        args.session_id_arg.as_deref(),
        args.turn_ordinal_arg.as_deref(),
    )?;
    let project = resolve_database_query_project_target(&args.root, args.project_id.as_deref())?;
    let origin_scope = query_origin_scope(args.shared, args.scope, None);
    let session = resolve_query_session_for_project_with_scope(
        &project,
        args.provider.map(provider_arg_to_source_kind),
        session_id,
        origin_scope,
    )?;
    let data = query_turn_insight_report_for_project(
        &project,
        session.provider,
        &session.session_id,
        turn_ordinal,
    )?;
    print_json_envelope(output, "darc.query.insights.turn.v1", &data)
}

/// Resolves one project-scoped query target from an explicit id or the active project.
pub(crate) fn resolve_database_query_project_target(
    root: &std::path::Path,
    project_id: Option<&str>,
) -> Result<ResolvedQueryProject> {
    resolve_query_project(Some(root.to_path_buf()), project_id)
}

/// Resolves one optional value supplied either as a flag or a positional argument.
pub(crate) fn optional_named_or_positional<'a>(
    value_label: &str,
    flag_name: &str,
    flag_value: Option<&'a str>,
    positional_name: &str,
    positional_value: Option<&'a str>,
) -> Result<Option<&'a str>> {
    match (flag_value, positional_value) {
        (Some(_), Some(_)) => Err(ReadValidationError::conflicting_identity_arguments(
            format!("pass {value_label} either as {positional_name} or {flag_name}, not both"),
            &[value_label, positional_name, flag_name],
        )
        .into()),
        (Some(value), None) | (None, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

/// Resolves one required value supplied either as a flag or a positional argument.
pub(crate) fn required_named_or_positional<'a>(
    value_label: &str,
    flag_name: &str,
    flag_value: Option<&'a str>,
    positional_name: &str,
    positional_value: Option<&'a str>,
) -> Result<&'a str> {
    optional_named_or_positional(
        value_label,
        flag_name,
        flag_value,
        positional_name,
        positional_value,
    )?
    .ok_or_else(|| {
        ReadValidationError::missing_required_identity(value_label, flag_name, positional_name)
            .into()
    })
}

/// Returns the matched-path preview limit selected by CLI flags.
pub(crate) fn matched_path_limit_arg(
    include_all_matched_paths: bool,
    matched_path_limit: usize,
) -> Option<usize> {
    (!include_all_matched_paths).then_some(matched_path_limit)
}

/// Resolves session-id and turn-ordinal values from flag and positional forms.
pub(crate) fn resolve_turn_identity_args<'a>(
    session_id: Option<&'a str>,
    turn_ordinal: Option<u64>,
    session_id_arg: Option<&'a str>,
    turn_ordinal_arg: Option<&'a str>,
) -> Result<(&'a str, u64)> {
    match (session_id, turn_ordinal, session_id_arg, turn_ordinal_arg) {
        (Some(session_id), Some(turn_ordinal), None, None) => Ok((session_id, turn_ordinal)),
        (Some(session_id), None, Some(turn_ordinal_arg), None) => {
            Ok((session_id, parse_turn_ordinal_arg(turn_ordinal_arg)?))
        }
        (None, Some(turn_ordinal), Some(session_id_arg), None) => {
            Ok((session_id_arg, turn_ordinal))
        }
        (None, None, Some(session_id_arg), Some(turn_ordinal_arg)) => {
            Ok((session_id_arg, parse_turn_ordinal_arg(turn_ordinal_arg)?))
        }
        (Some(_), Some(_), Some(_), _) | (Some(_), Some(_), None, Some(_)) => {
            Err(ReadValidationError::conflicting_identity_arguments(
                "pass turn identity either as SESSION_ID TURN_ORDINAL or with --session-id/--turn-ordinal, not both",
                &["SESSION_ID", "TURN_ORDINAL", "--session-id", "--turn-ordinal"],
            )
            .into())
        }
        (Some(_), None, None, None) => Err(ReadValidationError::missing_turn_identity(
            "read command requires turn ordinal as TURN_ORDINAL or --turn-ordinal",
            &["turn_ordinal"],
        )
        .into()),
        (None, Some(_), None, None) => Err(ReadValidationError::missing_turn_identity(
            "read command requires session id as SESSION_ID or --session-id",
            &["session_id"],
        )
        .into()),
        (None, None, None, None) => Err(ReadValidationError::missing_turn_identity(
            "read command requires session id and turn ordinal as SESSION_ID TURN_ORDINAL or --session-id/--turn-ordinal",
            &["session_id", "turn_ordinal"],
        )
        .into()),
        _ => Err(ReadValidationError::conflicting_identity_arguments(
            "unexpected extra positional turn identity arguments",
            &["SESSION_ID", "TURN_ORDINAL", "--session-id", "--turn-ordinal"],
        )
        .into()),
    }
}

/// Parses one turn ordinal positional value.
pub(crate) fn parse_turn_ordinal_arg(value: &str) -> Result<u64> {
    value
        .parse()
        .with_context(|| format!("invalid turn ordinal `{value}`"))
}

impl From<ListSessionsArgs> for QuerySessionsArgs {
    /// Converts canonical list-session arguments into the shared query-session shape.
    fn from(args: ListSessionsArgs) -> Self {
        Self {
            root: args.root,
            project_id: args.project_id,
            provider: args.provider,
            view: args.view,
            since: args.since,
            until: args.until,
            touched_path: args.touching,
            shared: args.shared,
            scope: args.scope,
            author: args.author,
            limit: args.limit,
            offset: args.offset,
        }
    }
}

impl SearchArgs {
    /// Converts canonical search flags into the existing turn-search query shape.
    pub(crate) fn into_query_search_turns_args(self) -> Result<QuerySearchTurnsArgs> {
        let query = required_named_or_positional(
            "query text",
            "--query",
            self.query.as_deref(),
            "QUERY",
            self.query_arg.as_deref(),
        )?
        .to_owned();
        Ok(QuerySearchTurnsArgs {
            root: self.root,
            project_id: self.project_id,
            provider: self.provider,
            session_id: self.session_id,
            shared: self.shared,
            scope: self.scope,
            author: self.author,
            mode: self.mode,
            query_arg: Some(query),
            query: None,
            include_tool_output: self.include_tool_output,
            fields: self.fields,
            excluded_fields: self.excluded_fields,
            since: self.since,
            until: self.until,
            limit: self.limit,
            offset: self.offset,
            matched_path_limit: self.matched_path_limit,
            match_limit: self.match_limit,
            include_all_matched_paths: self.include_all_matched_paths,
        })
    }
}

/// Returns whether one string is a full canonical UUID text value.
pub(crate) fn is_full_uuid_text(input: &str) -> bool {
    input.len() == 36
        && input
            .chars()
            .enumerate()
            .all(|(index, ch)| matches_uuid_character(index, ch))
}

/// Returns whether one character matches the canonical UUID grammar at one fixed position.
pub(crate) fn matches_uuid_character(index: usize, ch: char) -> bool {
    match index {
        8 | 13 | 18 | 23 => ch == '-',
        _ => ch.is_ascii_hexdigit(),
    }
}

/// Parses one rolling day-window argument such as `7d`.
pub(crate) fn parse_window_days(value: &str) -> Result<u32, String> {
    let Some(days) = value.strip_suffix('d') else {
        return Err("window must use the `<days>d` format, for example `7d`".to_owned());
    };
    let days = days
        .parse::<u32>()
        .map_err(|_| format!("invalid day window `{value}`"))?;
    if days == 0 {
        return Err("window must be at least 1 day".to_owned());
    }
    Ok(days)
}

/// Resolves one query time bound from relative shorthand or absolute ISO-like text.
pub(crate) fn resolve_query_time_bound(value: &str) -> Result<String> {
    resolve_shared_query_time_bound(value).map_err(|message| anyhow!(message))
}

/// Resolves one query time bound against one fixed clock for deterministic tests.
#[cfg(test)]
pub(crate) fn resolve_query_time_bound_at(
    value: &str,
    now: std::time::SystemTime,
) -> std::result::Result<String, String> {
    darc_paths::resolve_query_time_bound_at(value, now)
}

/// Parses one exact-search evidence field from snake_case or CLI kebab-case text.
pub(crate) fn parse_search_evidence_field(value: &str) -> Result<SearchEvidenceField, String> {
    SearchEvidenceField::parse_label(value).ok_or_else(|| {
        format!(
            "unsupported evidence field `{value}`; expected one of {}",
            supported_search_evidence_fields()
        )
    })
}

/// Formats the accepted exact-search evidence field names for CLI errors.
pub(crate) fn supported_search_evidence_fields() -> String {
    SearchEvidenceField::ALL
        .iter()
        .map(|field| field.as_str().replace('_', "-"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns help text for exact-search field inclusion.
pub(crate) fn search_evidence_field_include_help() -> String {
    "Restrict literal and regex search to an evidence field. Repeat to include multiple fields.\n\nAccepted fields:\n  messages: user-message, final-answer, commentary, reasoning-summary\n  tools: tool-name, tool-arguments, tool-output\n  other: delegation-summary, delegation-metadata, hook-summary, attachment-metadata, provider-response-item-metadata"
        .to_owned()
}

/// Returns help text for exact-search field exclusion.
pub(crate) fn search_evidence_field_exclude_help() -> String {
    "Exclude an evidence field from literal and regex search. Repeat to exclude multiple fields.\n\nAccepted fields:\n  messages: user-message, final-answer, commentary, reasoning-summary\n  tools: tool-name, tool-arguments, tool-output\n  other: delegation-summary, delegation-metadata, hook-summary, attachment-metadata, provider-response-item-metadata"
        .to_owned()
}

/// Returns help text for the literal/regex per-hit match preview cap.
pub(crate) fn search_match_limit_help() -> String {
    format!(
        "Maximum nested matches per literal/regex turn hit [default: {DEFAULT_SEARCH_MATCH_LIMIT}]"
    )
}

/// Returns help text for one default row-page limit.
pub(crate) fn default_query_page_limit_help(prefix: &str) -> String {
    format!("{prefix} [default: {DEFAULT_QUERY_PAGE_LIMIT}]")
}

/// Converts one parsed provider argument back into the shared source kind.
pub(crate) fn provider_arg_to_source_kind(provider: ProviderArg) -> SourceKind {
    match provider {
        ProviderArg::Claude => SourceKind::Claude,
        ProviderArg::Codex => SourceKind::Codex,
    }
}

/// Converts parsed shared-scope flags into the query origin scope.
fn query_origin_scope(
    shared: bool,
    scope: Option<SessionScopeArg>,
    author: Option<&str>,
) -> SessionOriginScope {
    if shared || (scope.is_none() && author.is_some()) {
        return SessionOriginScope::Shared;
    }
    match scope {
        Some(SessionScopeArg::Local) | None => SessionOriginScope::Local,
        Some(SessionScopeArg::Shared) => SessionOriginScope::Shared,
        Some(SessionScopeArg::All) => SessionOriginScope::All,
    }
}

/// Converts one parsed turn-detail view argument into the shared session-bundle view.
pub(crate) fn view_arg_to_session_bundle_view(view: ViewArg) -> SessionBundleView {
    match view {
        ViewArg::Full => SessionBundleView::Full,
        ViewArg::Narrative => SessionBundleView::Narrative,
    }
}

/// Converts one parsed search-mode argument back into the shared query enum.
pub(crate) fn search_mode_arg_to_search_mode(mode: SearchModeArg) -> SearchMode {
    match mode {
        SearchModeArg::Keyword => SearchMode::Keyword,
        SearchModeArg::Literal => SearchMode::Literal,
        SearchModeArg::Regex => SearchMode::Regex,
        SearchModeArg::FileName => SearchMode::FileName,
        SearchModeArg::FilePath => SearchMode::FilePath,
        SearchModeArg::PathFragment => SearchMode::PathFragment,
    }
}

/// Converts one parsed session-list view argument into the shared query projection enum.
pub(crate) fn session_list_view_arg_to_view(view: SessionListViewArg) -> SessionsView {
    match view {
        SessionListViewArg::Compact => SessionsView::Compact,
        SessionListViewArg::Full => SessionsView::Full,
    }
}

/// Converts one parsed turn-list view argument into the shared query projection enum.
pub(crate) fn turn_list_view_arg_to_view(view: TurnListViewArg) -> TurnsView {
    match view {
        TurnListViewArg::Full => TurnsView::Full,
        TurnListViewArg::Oneline => TurnsView::Oneline,
    }
}

/// Stores one compact row for session-scoped `darc list turns --view oneline`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TurnsOnelineTurnRow {
    pub(crate) turn_ordinal: u64,
    pub(crate) role: &'static str,
    pub(crate) user_prompt_preview: String,
    pub(crate) user_prompt_preview_chars: u64,
    pub(crate) user_prompt_total_chars: u64,
    pub(crate) agent_answer_preview: Option<String>,
    pub(crate) agent_answer_preview_chars: Option<u64>,
    pub(crate) agent_answer_total_chars: Option<u64>,
    pub(crate) step_count: u64,
    pub(crate) tool_call_count: u64,
}

/// Stores one compact top-level payload for session-scoped turn skims.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TurnsOnelineQueryData {
    pub(crate) project_id: String,
    pub(crate) provider: SourceKind,
    pub(crate) session_id: String,
    pub(crate) since: Option<String>,
    pub(crate) until: Option<String>,
    pub(crate) view: TurnsView,
    pub(crate) limit: u64,
    pub(crate) offset: u64,
    pub(crate) has_more: bool,
    pub(crate) turns: Vec<TurnsOnelineTurnRow>,
}

impl TurnsOnelineQueryData {
    /// Builds one compact session-turn payload from the full shared query result.
    pub(crate) fn from_turns_query(data: &darc_core::query::TurnsQueryData) -> Self {
        Self {
            project_id: data.project_id.clone(),
            provider: data.provider,
            session_id: data.session_id.clone(),
            since: data.since.clone(),
            until: data.until.clone(),
            view: data.view,
            limit: data.limit,
            offset: data.offset,
            has_more: data.has_more,
            turns: data
                .turns
                .iter()
                .map(|turn| TurnsOnelineTurnRow {
                    turn_ordinal: turn.turn_ordinal,
                    role: "user",
                    user_prompt_preview: turn.oneline_user_prompt_preview.clone(),
                    user_prompt_preview_chars: turn.oneline_user_prompt_preview_chars,
                    user_prompt_total_chars: turn.oneline_user_prompt_total_chars,
                    agent_answer_preview: turn.oneline_agent_answer_preview.clone(),
                    agent_answer_preview_chars: turn.oneline_agent_answer_preview_chars,
                    agent_answer_total_chars: turn.oneline_agent_answer_total_chars,
                    step_count: turn.step_count,
                    tool_call_count: turn.tool_call_count,
                })
                .collect(),
        }
    }
}

/// Stores the `--pick-one` success payload for `darc resolve session`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ResolveSessionPickOneQueryData {
    pub(crate) query: String,
    #[serde(rename = "match")]
    pub(crate) r#match: ResolvedSessionMatch,
}

impl ResolveSessionPickOneQueryData {
    /// Builds one single-match convenience payload from one resolved candidate.
    pub(crate) fn new(query: &str, r#match: ResolvedSessionMatch) -> Self {
        Self {
            query: query.to_owned(),
            r#match,
        }
    }
}
