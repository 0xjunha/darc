# Darc

[![License](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)
[![CI](https://github.com/0xjunha/darc/actions/workflows/ci.yml/badge.svg)](https://github.com/0xjunha/darc/actions/workflows/ci.yml)
[![codecov](https://codecov.io/github/0xjunha/darc/graph/badge.svg?token=J5ZVVBJ3U9)](https://codecov.io/github/0xjunha/darc)

Darc organizes your local Claude Code and Codex session history by project and keeps a durable archive of it.
It builds a queryable SQLite index for insights, analytics, downstream tools, and direct session inspection.

The daily happy path is `darc refresh`, which runs `sync` and `index` together.

## What it does

- Registers local projects in a shared `~/.darc` workspace and resolves the active project from the current checkout.
- Archives matching Claude and Codex session history into a per-project rollout archive.
- Rebuilds a normalized SQLite index from archived rollouts for insights, reporting, and downstream tooling.
- Exposes a stable machine-readable `darc query` protocol for workspace, session, turn, search, and insights data.
- Derives indexed insights at workspace, project, and turn scope without requiring clients to open `index.sqlite`
  directly.
- Preserves project continuity across checkout moves, worktrees, merges, and renames with built-in linking and
  rename workflows.

## Workspace Architecture

Darc is organized as a small workspace of focused crates with `darc-core` kept as a thin facade and orchestration layer.

- `darc-cli`: CLI entrypoint and command surface.
- `darc-core`: stable public API plus project/workspace orchestration such as `init`, `refresh`, `link`, and
  `rename-from`.
- `darc-index`: normalized session ingestion, SQLite schema/migrations, and indexing metrics.
- `darc-paths`: shared path normalization, source-kind modeling, and project/worktree discovery helpers used across
  lower-level crates.
- `darc-query`: read-only query and reporting over the indexed SQLite data.
- `darc-rollout`: rollout models, provider parsers, and schema/version logic.
- `darc-rollout-audit`: release/schema compatibility audits and other heavy maintainer-only rollout tooling.
- `darc-sync`: archive discovery, sync planning, and file copy execution.
- `darc-test-utils`: shared test fixtures and helpers for Git repositories, temporary directories, and seeded index
  data.

#### Crate boundaries follow three rules:

1. Keep each crate cohesive around one dominant capability.
2. Keep dependency direction acyclic.
3. Extract shared models/utilities downward instead of letting lower-level code depend on orchestration
   crates.

## Quickstart

Install the CLI from this repository:

```bash
cargo install --path crates/cli
```

Initialize Darc for a project, then use the daily refresh path:

```bash
cd /path/to/project
darc init
darc refresh
```

Use provider filters when you only want one source:

```bash
darc refresh --provider claude
```

Refresh every registered project in the shared Darc workspace:

```bash
darc refresh --all
```

## Commands

- `darc init` detects local sources and creates the shared Darc config.
- `darc refresh` is the daily happy path. It runs `sync` then `index` for the active project, or every registered
  project with `--all`.
- `darc sync` archives matching Claude and Codex sessions for the active project.
- `darc index` indexes archived sessions into SQLite.
- `darc query` exposes the machine-readable read protocol for workspace, session, turn, search, and insights data.
  Query commands currently require `--json`; see [Query protocol](docs/query-protocol.md).
- `darc link`, `darc remove`, and `darc rename-from` manage renamed or merged projects.

## Search

Darc now supports project-scoped turn search through the query protocol.

- keyword search uses SQLite FTS5 over derived turn text
- file-name search uses derived basenames from indexed `file_accesses`
- file-path search uses derived repo-relative or raw paths from indexed `file_accesses`
- file-name and file-path search currently rank exact matches first, then prefix matches, then substring matches

Example:

```bash
darc query search turns \
  --project-id repo-abc123 \
  --mode keyword \
  --query "panic unwrap" \
  --json
```

See [Query protocol](docs/query-protocol.md) for the full search payload contract and filters.

Run `darc --help` for the visible CLI surface. Hidden maintainer commands are documented separately.

## Documentation

- [Documentation index](docs/README.md)
- [Query protocol](docs/query-protocol.md)
- [Project rename and linking](docs/project-rename.md)
- [Schema audits](docs/schema-audits.md)
- [Claude support](docs/claude-support.md)
- [Backlog](docs/todo.md)
