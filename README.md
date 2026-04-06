# Darc

[![License](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)
[![CI](https://github.com/0xjunha/darc/actions/workflows/ci.yml/badge.svg)](https://github.com/0xjunha/darc/actions/workflows/ci.yml)
[![codecov](https://codecov.io/github/0xjunha/darc/graph/badge.svg?token=J5ZVVBJ3U9)](https://codecov.io/github/0xjunha/darc)

Darc archives local Claude and Codex session history by project, then indexes the archived rollouts into a normalized SQLite index for inspection, analytics, and downstream tooling.

The daily happy path is `darc refresh`, which runs `sync` and `index` together.

## What it does

- Detects and registers local projects in a shared `~/.darc` workspace.
- Syncs matching Claude and Codex session data into a per-project archive.
- Indexes archived rollouts into a normalized SQLite index.
- Preserves project history across checkout moves, merges, and renames.

## Workspace Architecture

Darc is moving toward a small workspace of focused crates with `darc-core` kept as a thin facade and orchestration layer during the split.

- `darc-cli`: CLI entrypoint and command surface.
- `darc-core`: stable public API plus project/workspace orchestration such as `init`, `refresh`, `link`, and `rename-from`.
- `darc-rollout`: rollout models, provider parsers, and schema/version logic.
- `darc-rollout-audit`: release/schema compatibility audits and other heavy maintainer-only rollout tooling.
- `darc-sync`: archive discovery, sync planning, and file copy execution.
- `darc-index`: normalized session ingestion, SQLite schema/migrations, and indexing metrics.
- `darc-query`: read-only query and reporting over the indexed SQLite data.

Crate boundaries follow three rules: keep each crate cohesive around one dominant capability, keep dependency direction acyclic, and extract shared models/utilities downward instead of letting lower-level code depend on orchestration crates.

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
- `darc refresh` is the daily happy path. It runs `sync` then `index` for the active project, or every registered project with `--all`.
- `darc sync` archives matching Claude and Codex sessions for the active project.
- `darc index` indexes archived sessions into SQLite.
- `darc link`, `darc remove`, and `darc rename-from` manage renamed or merged projects.

Run `darc --help` for the visible CLI surface. Hidden maintainer commands are documented separately.

## Documentation

- [Documentation index](docs/README.md)
- [Query protocol](docs/query-protocol.md)
- [Project rename and linking](docs/project-rename.md)
- [Schema audits](docs/schema-audits.md)
- [Claude support](docs/claude-support.md)
- [Backlog](docs/todo.md)
