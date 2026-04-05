# Darc

[![License](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)
[![CI](https://github.com/0xjunha/darc/actions/workflows/ci.yml/badge.svg)](https://github.com/0xjunha/darc/actions/workflows/ci.yml)
[![codecov](https://codecov.io/github/0xjunha/darc/graph/badge.svg?token=J5ZVVBJ3U9)](https://codecov.io/github/0xjunha/darc)

Darc archives local Claude and Codex session history by project, then indexes the archived rollouts into a normalized SQLite index for inspection, analytics, and downstream tooling.

## What it does

- Detects and registers local projects in a shared `~/.darc` workspace.
- Syncs matching Claude and Codex session data into a per-project archive.
- Indexes archived rollouts into a normalized SQLite index.
- Preserves project history across checkout moves, merges, and renames.

## Quickstart

Install the CLI from this repository:

```bash
cargo install --path crates/cli
```

Initialize Darc for a project, archive matching sessions, then index them:

```bash
cd /path/to/project
darc init
darc sync
darc index
```

Use provider filters when you only want one source:

```bash
darc sync --provider claude
darc index --provider claude
```

## Commands

- `darc init` detects local sources and creates the shared Darc config.
- `darc sync` archives matching Claude and Codex sessions for the active project.
- `darc index` indexes archived sessions into SQLite.
- `darc link`, `darc remove`, and `darc rename-from` manage renamed or merged projects.

Run `darc --help` for the visible CLI surface. Hidden maintainer commands are documented separately.

## Documentation

- [Documentation index](docs/README.md)
- [Project rename and linking](docs/project-rename.md)
- [Schema audits](docs/schema-audits.md)
- [Claude support and analytics](docs/claude-support.md)
- [Backlog](docs/todo.md)
