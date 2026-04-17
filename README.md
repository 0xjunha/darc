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
- Exposes a stable machine-readable `darc query` protocol for workspace, session, turn, file-pivot, search, wiki,
  and insights data.
- Provides an experimental Context Wiki workflow with read-side wiki queries plus agent-backed digest runs that
  validate structured proposal artifacts, merge canonical wiki artifacts, and persist durable run logs under
  `~/.darc/context-wiki/`.
- Derives indexed insights at workspace, project, and turn scope without requiring clients to open `index.sqlite`
  directly.
- Surfaces best-effort per-turn and per-session stats such as model, token usage, effective agent runtime, and
  observed patch-line counts through the query protocol.
- Preserves project continuity across checkout moves, worktrees, merges, and renames with built-in linking and
  rename workflows.

## Workspace Architecture

Darc is organized as a small workspace of focused crates with `darc-core` kept as a thin facade and orchestration layer.

- `darc-agent`: external agent runtime command preparation for Context Wiki digest workers.
- `darc-cli`: CLI entrypoint and command surface.
- `darc-core`: stable public API and top-level orchestration across Darc workflows.
- `darc-index`: normalized session ingestion, SQLite schema/migrations, and indexing metrics.
- `darc-paths`: shared path normalization, source-kind modeling, and project/worktree discovery helpers used across
  lower-level crates.
- `darc-query`: read-only query and reporting over the indexed SQLite data.
- `darc-rollout`: rollout models, provider parsers, and schema/version logic.
- `darc-rollout-audit`: release/schema compatibility audits and other heavy maintainer-only rollout tooling.
- `darc-sync`: archive discovery, sync planning, and file copy execution.
- `darc-test-utils`: shared test fixtures and helpers for Git repositories, temporary directories, and seeded index
  data.
- `darc-wiki`: canonical Context Wiki storage, proposal validation, canonical artifact merge, and durable
  run-state models.

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
- `darc query` exposes the machine-readable read protocol for workspace, session, turn, file-pivot, search, wiki, and
  insights data. Query commands currently require `--json`; see [Query protocol](docs/query-protocol.md).
- `darc wiki` hosts the experimental imperative Context Wiki workflow, including digest start/cancel commands plus
  entry discard/restore lifecycle commands.
- `darc link`, `darc remove`, and `darc rename-from` manage renamed or merged projects.

## Session And Turn Stats

Darc now exposes best-effort session and turn stats through `darc query`.

- turn rows and turn insights can include `primary_model`, `total_token_count`, `token_usage`,
  `effective_agent_runtime_ms`, `changed_file_count`, `added_line_count`, and `removed_line_count`
- session rows roll those values up across the indexed turns in that session
- `total_token_count` is now the normalized cache-aware total when Darc could derive one from the archived rollout
- `token_usage` exposes the cross-provider bucket breakdown Darc stores for `input_uncached`, `cache_read`,
  `cache_write`, `output`, optional `reasoning`, and the provider-native total when available
- `reasoning` is a subset of output, not an extra additive bucket, and unsupported buckets stay `null`
- older archived provider versions may leave model or token fields as `null` when the transcript did not report
  stable values, or until a project is re-indexed from archived rollouts after the token-bucket upgrade
- observed diff counts come from transcript-visible patch payloads such as `apply_patch`, not from a live git diff

See [Query protocol](docs/query-protocol.md) for the exact payload contract and semantics.

## Query Workflows

Darc's read-side query surface now covers project-scoped search, compact turn skims, file/session pivots, wiki overlap
checks, and single-call session bundles.

- `darc query search turns` handles keyword, file-name, and file-path search with optional provider/session filters.
- `darc query turns` works in two modes: session-scoped lists (`--provider --session-id`) and grep-scoped matches
  (`--grep`) with role, context, time, touched-path, and compact `--view oneline` options.
- `darc query files`, `darc query session-files`, and `darc query session-bundle` let clients pivot between matched
  files, touched sessions, per-session file summaries, and one-call session detail bundles.
- `darc query wiki entries` adds grep, evidence-reference, and session-coverage filters so digest prep can check
  existing wiki coverage before proposing new entries.

Examples:

```bash
darc query search turns \
  --project-id repo-abc123 \
  --mode keyword \
  --query "panic unwrap" \
  --json
```

```bash
darc query turns \
  --project-id repo-abc123 \
  --grep "staged init" \
  --role user \
  --context 1 \
  --since 14d \
  --json
```

```bash
darc query files \
  --project-id repo-abc123 \
  --path "crates/wiki/**/*.rs" \
  --since 30d \
  --json
```

```bash
darc query session-bundle \
  --project-id repo-abc123 \
  --provider codex \
  --session-id session-1 \
  --view narrative \
  --json
```

```bash
darc query wiki entries \
  --project-id repo-abc123 \
  --covers-session codex:session-1 \
  --evidence-ref codex:session-1#4 \
  --json
```

See [Query protocol](docs/query-protocol.md) for the full command matrix, payload contracts, and filter semantics.

## Context Wiki

Darc includes an experimental backend-owned Context Wiki workflow under `~/.darc/context-wiki/`.

- Use `darc query wiki ... --json` for read-side access to registry, entries, digests, and runs.
- Use the read-side `darc query` surface to investigate history before or alongside digest work. In particular,
  `darc query turns --grep ...`, `darc query files ...`, `darc query session-files ...`,
  `darc query session-bundle ...`, and `darc query sessions --touched-path ...` are the intended project-scoped
  primitives for narrowing evidence and reviewing one candidate session in full.
- Use `darc wiki digest start` to assemble selected session context, invoke an external Codex or Claude Code CLI,
  validate the returned structured proposal artifact, and merge the validated result into canonical wiki artifacts.
- Use `darc wiki digest cancel` to request cancellation for an in-flight run.
- Use `darc wiki entry discard` and `darc wiki entry restore` to change entry lifecycle state without deleting the
  canonical Markdown artifact.
- Successful digest runs persist a digest report plus terminal run metadata and either create or update canonical
  decision-trace entries, or record a zero-entry digest when no durable decisions were extracted.

Example:

```bash
darc wiki digest start \
  --project-id repo-abc123 \
  --session-ref codex:session-1 \
  --agent codex \
  --runtime external-cli \
  --model gpt-5.4 \
  --target-domain query-protocol \
  --json
```

See [Context Wiki](docs/context-wiki.md) for workflow details, runtime requirements, run artifacts, and current
limitations.

Run `darc --help` for the visible CLI surface. Hidden maintainer commands are documented separately.

## Documentation

- [Documentation index](docs/README.md)
- [Context Wiki](docs/context-wiki.md)
- [Query protocol](docs/query-protocol.md)
- [Project rename and linking](docs/project-rename.md)
- [Schema audits](docs/schema-audits.md)
- [Claude support](docs/claude-support.md)
- [Backlog](docs/todo.md)
