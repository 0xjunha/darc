use anyhow::{Context, Result};
use rusqlite::Connection;

/// Stores one vetted SQLite table identifier used by index schema helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchemaTable {
    Turns,
    ToolCalls,
    FileAccesses,
    TurnSearch,
    TurnSearchFts,
    CodexSessions,
    CodexTurns,
}

impl SchemaTable {
    /// Returns the stable SQLite table name for one vetted table identifier.
    pub(crate) fn sql_name(self) -> &'static str {
        match self {
            Self::Turns => "turns",
            Self::ToolCalls => "tool_calls",
            Self::FileAccesses => "file_accesses",
            Self::TurnSearch => "turn_search",
            Self::TurnSearchFts => "turn_search_fts",
            Self::CodexSessions => "codex_sessions",
            Self::CodexTurns => "codex_turns",
        }
    }
}

/// Stores one vetted SQLite column definition appended by compatibility migrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableColumn {
    pub(crate) name: &'static str,
    pub(crate) sql_type: &'static str,
}

/// Stores one table plus the additive columns compatibility migrations must preserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompatColumnSet {
    pub(crate) table: SchemaTable,
    pub(crate) label: &'static str,
    pub(crate) columns: &'static [TableColumn],
}

/// Lists the legacy Codex-session columns required by the current parser schema.
pub(crate) const CODEX_SESSION_COMPAT_COLUMNS: &[TableColumn] = &[
    TableColumn {
        name: "cli_version",
        sql_type: "TEXT",
    },
    TableColumn {
        name: "schema_id",
        sql_type: "TEXT",
    },
    TableColumn {
        name: "determinism",
        sql_type: "TEXT",
    },
    TableColumn {
        name: "source_size",
        sql_type: "INTEGER",
    },
    TableColumn {
        name: "source_mtime_ms",
        sql_type: "INTEGER",
    },
];

/// Lists the derived turn analytics columns required by the current schema.
pub(crate) const TURN_ANALYTICS_COLUMNS: &[TableColumn] = &[
    TableColumn {
        name: "step_count",
        sql_type: "INTEGER NOT NULL DEFAULT 0",
    },
    TableColumn {
        name: "tool_call_count",
        sql_type: "INTEGER NOT NULL DEFAULT 0",
    },
    TableColumn {
        name: "tool_output_count",
        sql_type: "INTEGER NOT NULL DEFAULT 0",
    },
    TableColumn {
        name: "attachment_count",
        sql_type: "INTEGER NOT NULL DEFAULT 0",
    },
    TableColumn {
        name: "delegation_count",
        sql_type: "INTEGER NOT NULL DEFAULT 0",
    },
    TableColumn {
        name: "hook_summary_count",
        sql_type: "INTEGER NOT NULL DEFAULT 0",
    },
    TableColumn {
        name: "has_final_answer",
        sql_type: "INTEGER NOT NULL DEFAULT 0",
    },
    TableColumn {
        name: "duration_ms",
        sql_type: "INTEGER",
    },
    TableColumn {
        name: "effective_agent_runtime_ms",
        sql_type: "INTEGER",
    },
    TableColumn {
        name: "total_token_count",
        sql_type: "INTEGER",
    },
    TableColumn {
        name: "primary_model",
        sql_type: "TEXT",
    },
    TableColumn {
        name: "changed_file_count",
        sql_type: "INTEGER NOT NULL DEFAULT 0",
    },
    TableColumn {
        name: "added_line_count",
        sql_type: "INTEGER NOT NULL DEFAULT 0",
    },
    TableColumn {
        name: "removed_line_count",
        sql_type: "INTEGER NOT NULL DEFAULT 0",
    },
];

/// Lists the file-access columns required by the current derived analytics schema.
pub(crate) const FILE_ACCESS_COLUMNS: &[TableColumn] = &[TableColumn {
    name: "file_name",
    sql_type: "TEXT",
}];

/// Lists the additive columns that older schema snapshots may need during reopen.
pub(crate) const COMPAT_COLUMN_SETS: &[CompatColumnSet] = &[
    CompatColumnSet {
        table: SchemaTable::Turns,
        label: "turns",
        columns: TURN_ANALYTICS_COLUMNS,
    },
    CompatColumnSet {
        table: SchemaTable::FileAccesses,
        label: "file_accesses",
        columns: FILE_ACCESS_COLUMNS,
    },
    CompatColumnSet {
        table: SchemaTable::CodexSessions,
        label: "codex_sessions",
        columns: CODEX_SESSION_COMPAT_COLUMNS,
    },
];

/// Lists the derived analytics tables that can be rebuilt from canonical turn rows.
pub(crate) const DERIVED_ANALYTICS_TABLES: &[SchemaTable] = &[
    SchemaTable::ToolCalls,
    SchemaTable::FileAccesses,
    SchemaTable::TurnSearch,
    SchemaTable::TurnSearchFts,
];

#[cfg(test)]
/// Stores one managed secondary schema object recreated after compatibility repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SchemaObject {
    pub(crate) kind: SchemaObjectKind,
    pub(crate) name: &'static str,
}

#[cfg(test)]
/// Stores the SQLite object type for one managed secondary schema object.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchemaObjectKind {
    Table,
    Index,
    Trigger,
}

#[cfg(test)]
impl SchemaObjectKind {
    /// Returns the SQLite master-table type for one managed secondary schema object.
    pub(crate) fn sqlite_master_type(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Index => "index",
            Self::Trigger => "trigger",
        }
    }

    /// Returns the DROP statement prefix for one managed secondary schema object.
    pub(crate) fn drop_statement_prefix(self) -> &'static str {
        match self {
            Self::Table => "DROP TABLE IF EXISTS",
            Self::Index => "DROP INDEX IF EXISTS",
            Self::Trigger => "DROP TRIGGER IF EXISTS",
        }
    }
}

#[cfg(test)]
/// Lists the managed secondary schema objects that reopen should recreate.
pub(crate) const SUPPLEMENTAL_SCHEMA_OBJECTS: &[SchemaObject] = &[
    SchemaObject {
        kind: SchemaObjectKind::Index,
        name: "turns_project_provider_started_idx",
    },
    SchemaObject {
        kind: SchemaObjectKind::Index,
        name: "turns_started_idx",
    },
    SchemaObject {
        kind: SchemaObjectKind::Index,
        name: "turns_project_started_idx",
    },
    SchemaObject {
        kind: SchemaObjectKind::Index,
        name: "tool_calls_project_tool_idx",
    },
    SchemaObject {
        kind: SchemaObjectKind::Index,
        name: "tool_calls_project_session_turn_idx",
    },
    SchemaObject {
        kind: SchemaObjectKind::Index,
        name: "tool_calls_project_timestamp_idx",
    },
    SchemaObject {
        kind: SchemaObjectKind::Index,
        name: "file_accesses_project_access_path_idx",
    },
    SchemaObject {
        kind: SchemaObjectKind::Index,
        name: "file_accesses_project_file_name_idx",
    },
    SchemaObject {
        kind: SchemaObjectKind::Index,
        name: "file_accesses_project_repo_relative_path_idx",
    },
    SchemaObject {
        kind: SchemaObjectKind::Index,
        name: "file_accesses_project_session_turn_idx",
    },
    SchemaObject {
        kind: SchemaObjectKind::Index,
        name: "sessions_project_provider_schema_idx",
    },
    SchemaObject {
        kind: SchemaObjectKind::Table,
        name: "turn_search",
    },
    SchemaObject {
        kind: SchemaObjectKind::Table,
        name: "turn_search_fts",
    },
    SchemaObject {
        kind: SchemaObjectKind::Trigger,
        name: "turn_search_ai",
    },
    SchemaObject {
        kind: SchemaObjectKind::Trigger,
        name: "turn_search_ad",
    },
    SchemaObject {
        kind: SchemaObjectKind::Trigger,
        name: "turn_search_au",
    },
];

const CREATE_BASE_SCHEMA_SQL: &str = "
    PRAGMA foreign_keys = ON;

    CREATE TABLE IF NOT EXISTS sessions (
        project_id TEXT NOT NULL,
        provider TEXT NOT NULL,
        session_id TEXT NOT NULL,
        parent_session_id TEXT,
        session_kind TEXT NOT NULL,
        archive_path TEXT NOT NULL,
        cwd TEXT NOT NULL,
        cli_version TEXT,
        schema_id TEXT,
        determinism TEXT,
        source_size INTEGER,
        source_mtime_ms INTEGER,
        PRIMARY KEY (project_id, provider, session_id),
        UNIQUE (project_id, archive_path)
    );

    CREATE TABLE IF NOT EXISTS turns (
        project_id TEXT NOT NULL,
        provider TEXT NOT NULL,
        session_id TEXT NOT NULL,
        turn_ordinal INTEGER NOT NULL,
        turn_id TEXT,
        started_at TEXT NOT NULL,
        completed_at TEXT,
        status TEXT NOT NULL,
        user_message TEXT NOT NULL,
        final_answer_at TEXT,
        final_answer_text TEXT,
        steps_json TEXT NOT NULL,
        step_count INTEGER NOT NULL DEFAULT 0,
        tool_call_count INTEGER NOT NULL DEFAULT 0,
        tool_output_count INTEGER NOT NULL DEFAULT 0,
        attachment_count INTEGER NOT NULL DEFAULT 0,
        delegation_count INTEGER NOT NULL DEFAULT 0,
        hook_summary_count INTEGER NOT NULL DEFAULT 0,
        has_final_answer INTEGER NOT NULL DEFAULT 0,
        duration_ms INTEGER,
        effective_agent_runtime_ms INTEGER,
        total_token_count INTEGER,
        primary_model TEXT,
        changed_file_count INTEGER NOT NULL DEFAULT 0,
        added_line_count INTEGER NOT NULL DEFAULT 0,
        removed_line_count INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (project_id, provider, session_id, turn_ordinal),
        FOREIGN KEY (project_id, provider, session_id)
            REFERENCES sessions(project_id, provider, session_id)
            ON DELETE CASCADE
    );

    CREATE TABLE IF NOT EXISTS tool_calls (
        project_id TEXT NOT NULL,
        provider TEXT NOT NULL,
        session_id TEXT NOT NULL,
        turn_ordinal INTEGER NOT NULL,
        call_ordinal INTEGER NOT NULL,
        call_id TEXT NOT NULL,
        timestamp TEXT NOT NULL,
        tool_name TEXT,
        arguments_text TEXT,
        output_text TEXT,
        status TEXT,
        is_error INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (project_id, provider, session_id, turn_ordinal, call_ordinal),
        FOREIGN KEY (project_id, provider, session_id, turn_ordinal)
            REFERENCES turns(project_id, provider, session_id, turn_ordinal)
            ON DELETE CASCADE
    );

    CREATE TABLE IF NOT EXISTS file_accesses (
        project_id TEXT NOT NULL,
        provider TEXT NOT NULL,
        session_id TEXT NOT NULL,
        turn_ordinal INTEGER NOT NULL,
        call_ordinal INTEGER NOT NULL,
        call_id TEXT NOT NULL,
        timestamp TEXT NOT NULL,
        tool_name TEXT NOT NULL,
        access_type TEXT NOT NULL,
        path TEXT NOT NULL,
        repo_relative_path TEXT,
        file_name TEXT,
        PRIMARY KEY (
            project_id,
            provider,
            session_id,
            turn_ordinal,
            call_ordinal,
            access_type,
            path
        ),
        FOREIGN KEY (project_id, provider, session_id, turn_ordinal, call_ordinal)
            REFERENCES tool_calls(
                project_id,
                provider,
                session_id,
                turn_ordinal,
                call_ordinal
            )
            ON DELETE CASCADE
    );
";

const CREATE_SUPPLEMENTAL_SCHEMA_SQL: &str = "
    CREATE INDEX IF NOT EXISTS turns_project_provider_started_idx
        ON turns (project_id, provider, started_at);
    CREATE INDEX IF NOT EXISTS turns_started_idx
        ON turns (started_at);
    CREATE INDEX IF NOT EXISTS turns_project_started_idx
        ON turns (project_id, started_at);

    CREATE INDEX IF NOT EXISTS tool_calls_project_tool_idx
        ON tool_calls (project_id, tool_name);
    CREATE INDEX IF NOT EXISTS tool_calls_project_session_turn_idx
        ON tool_calls (project_id, provider, session_id, turn_ordinal);
    CREATE INDEX IF NOT EXISTS tool_calls_project_timestamp_idx
        ON tool_calls (project_id, timestamp);

    CREATE INDEX IF NOT EXISTS file_accesses_project_access_path_idx
        ON file_accesses (project_id, access_type, path);
    CREATE INDEX IF NOT EXISTS file_accesses_project_file_name_idx
        ON file_accesses (project_id, file_name COLLATE NOCASE);
    CREATE INDEX IF NOT EXISTS file_accesses_project_path_idx
        ON file_accesses (project_id, path COLLATE NOCASE);
    CREATE INDEX IF NOT EXISTS file_accesses_project_repo_relative_path_idx
        ON file_accesses (project_id, repo_relative_path COLLATE NOCASE);
    CREATE INDEX IF NOT EXISTS file_accesses_project_session_turn_idx
        ON file_accesses (project_id, provider, session_id, turn_ordinal);
    CREATE INDEX IF NOT EXISTS sessions_project_provider_schema_idx
        ON sessions (project_id, provider, schema_id, determinism);

    CREATE TABLE IF NOT EXISTS turn_search (
        project_id TEXT NOT NULL,
        provider TEXT NOT NULL,
        session_id TEXT NOT NULL,
        turn_ordinal INTEGER NOT NULL,
        user_message_text TEXT NOT NULL,
        final_answer_text TEXT NOT NULL,
        tool_text TEXT NOT NULL,
        PRIMARY KEY (project_id, provider, session_id, turn_ordinal),
        FOREIGN KEY (project_id, provider, session_id, turn_ordinal)
            REFERENCES turns(project_id, provider, session_id, turn_ordinal)
            ON DELETE CASCADE
    );

    CREATE VIRTUAL TABLE IF NOT EXISTS turn_search_fts USING fts5(
        user_message_text,
        final_answer_text,
        tool_text,
        content = 'turn_search',
        content_rowid = 'rowid',
        tokenize = 'unicode61'
    );

    CREATE TRIGGER IF NOT EXISTS turn_search_ai AFTER INSERT ON turn_search BEGIN
        INSERT INTO turn_search_fts (
            rowid,
            user_message_text,
            final_answer_text,
            tool_text
        )
        VALUES (
            new.rowid,
            new.user_message_text,
            new.final_answer_text,
            new.tool_text
        );
    END;

    CREATE TRIGGER IF NOT EXISTS turn_search_ad AFTER DELETE ON turn_search BEGIN
        INSERT INTO turn_search_fts (
            turn_search_fts,
            rowid,
            user_message_text,
            final_answer_text,
            tool_text
        )
        VALUES (
            'delete',
            old.rowid,
            old.user_message_text,
            old.final_answer_text,
            old.tool_text
        );
    END;

    CREATE TRIGGER IF NOT EXISTS turn_search_au AFTER UPDATE ON turn_search BEGIN
        INSERT INTO turn_search_fts (
            turn_search_fts,
            rowid,
            user_message_text,
            final_answer_text,
            tool_text
        )
        VALUES (
            'delete',
            old.rowid,
            old.user_message_text,
            old.final_answer_text,
            old.tool_text
        );
        INSERT INTO turn_search_fts (
            rowid,
            user_message_text,
            final_answer_text,
            tool_text
        )
        VALUES (
            new.rowid,
            new.user_message_text,
            new.final_answer_text,
            new.tool_text
        );
    END;
";

/// Stores the canonical normalized-session insert statement shared across writers and helpers.
pub(crate) const INSERT_SESSION_SQL: &str = "
    INSERT INTO sessions (
        project_id,
        provider,
        session_id,
        parent_session_id,
        session_kind,
        archive_path,
        cwd,
        cli_version,
        schema_id,
        determinism,
        source_size,
        source_mtime_ms
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
";

/// Stores the canonical normalized-turn insert statement shared across writers and helpers.
pub(crate) const INSERT_TURN_SQL: &str = "
    INSERT INTO turns (
        project_id,
        provider,
        session_id,
        turn_ordinal,
        turn_id,
        started_at,
        completed_at,
        status,
        user_message,
        final_answer_at,
        final_answer_text,
        steps_json,
        step_count,
        tool_call_count,
        tool_output_count,
        attachment_count,
        delegation_count,
        hook_summary_count,
        has_final_answer,
        duration_ms,
        effective_agent_runtime_ms,
        total_token_count,
        primary_model,
        changed_file_count,
        added_line_count,
        removed_line_count
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)
";

/// Stores the canonical derived tool-call insert statement shared across writers and helpers.
pub(crate) const INSERT_TOOL_CALL_SQL: &str = "
    INSERT INTO tool_calls (
        project_id,
        provider,
        session_id,
        turn_ordinal,
        call_ordinal,
        call_id,
        timestamp,
        tool_name,
        arguments_text,
        output_text,
        status,
        is_error
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
";

/// Stores the canonical derived file-access insert statement shared across writers and helpers.
pub(crate) const INSERT_FILE_ACCESS_SQL: &str = "
    INSERT INTO file_accesses (
        project_id,
        provider,
        session_id,
        turn_ordinal,
        call_ordinal,
        call_id,
        timestamp,
        tool_name,
        access_type,
        path,
        repo_relative_path,
        file_name
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
";

/// Stores the canonical turn-search insert statement shared across writers and helpers.
pub(crate) const INSERT_TURN_SEARCH_SQL: &str = "
    INSERT INTO turn_search (
        project_id,
        provider,
        session_id,
        turn_ordinal,
        user_message_text,
        final_answer_text,
        tool_text
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
";

/// Stores the derived-analytics clear batch used before full rebuilds.
pub(crate) const DELETE_DERIVED_ANALYTICS_SQL: &str = "
    DELETE FROM turn_search;
    DELETE FROM file_accesses;
    DELETE FROM tool_calls;
";

/// Stores the turn scan query used while rebuilding derived analytics tables.
pub(crate) const SELECT_DERIVED_ANALYTICS_REBUILD_ROWS_SQL: &str = "
    SELECT
        project_id,
        provider,
        session_id,
        turn_ordinal,
        steps_json,
        user_message,
        final_answer_text
    FROM turns
    ORDER BY project_id ASC, provider ASC, session_id ASC, turn_ordinal ASC
";

/// Creates the normalized base tables when they are missing.
pub(crate) fn initialize_base_schema(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(CREATE_BASE_SCHEMA_SQL)
        .context("failed to initialize index database base schema")?;
    Ok(())
}

/// Creates the managed secondary schema objects once their dependencies exist.
pub(crate) fn initialize_supplemental_schema(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(CREATE_SUPPLEMENTAL_SCHEMA_SQL)
        .context("failed to initialize index database supplemental schema")?;
    Ok(())
}

/// Returns the `ALTER TABLE ... ADD COLUMN ...` SQL for one vetted table and column.
pub(crate) fn alter_table_add_column_sql(table: SchemaTable, column: TableColumn) -> String {
    format!(
        "ALTER TABLE {} ADD COLUMN {} {}",
        table.sql_name(),
        column.name,
        column.sql_type
    )
}

/// Returns the `PRAGMA table_info(...)` SQL for one vetted table identifier.
pub(crate) fn table_info_sql(table: SchemaTable) -> String {
    format!("PRAGMA table_info({})", table.sql_name())
}

/// Returns whether one vetted table already contains a named column.
pub(crate) fn table_has_column(
    connection: &Connection,
    table: SchemaTable,
    column: &str,
) -> Result<bool> {
    let mut statement = connection
        .prepare(&table_info_sql(table))
        .with_context(|| {
            format!(
                "failed to inspect SQLite schema for table `{}`",
                table.sql_name()
            )
        })?;
    let mut rows = statement.query([]).with_context(|| {
        format!(
            "failed to query SQLite schema for table `{}`",
            table.sql_name()
        )
    })?;
    while let Some(row) = rows.next().context("failed to read SQLite schema row")? {
        let existing: String = row.get(1).context("failed to read SQLite column name")?;
        if existing == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
/// Prepares and runs the current-schema SQL statements to smoke test SQLite parsing.
pub(super) fn smoke_test_sql(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(CREATE_BASE_SCHEMA_SQL)
        .context("failed to run base schema smoke test")?;
    connection
        .execute_batch(CREATE_SUPPLEMENTAL_SCHEMA_SQL)
        .context("failed to run supplemental schema smoke test")?;
    for (label, sql) in [
        ("session insert", INSERT_SESSION_SQL),
        ("turn insert", INSERT_TURN_SQL),
        ("tool call insert", INSERT_TOOL_CALL_SQL),
        ("file access insert", INSERT_FILE_ACCESS_SQL),
        ("turn search insert", INSERT_TURN_SEARCH_SQL),
        (
            "derived analytics rebuild select",
            SELECT_DERIVED_ANALYTICS_REBUILD_ROWS_SQL,
        ),
    ] {
        connection
            .prepare(sql)
            .with_context(|| format!("failed to prepare {label}"))?;
    }
    connection
        .execute_batch(DELETE_DERIVED_ANALYTICS_SQL)
        .context("failed to run derived analytics clear smoke test")?;
    Ok(())
}
