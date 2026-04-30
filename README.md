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
- Exposes a stable machine-readable `darc query` protocol for workspace, session, turn, file-pivot, search, and
  insights data.
- Derives indexed insights at workspace, project, and turn scope without requiring clients to open `index.sqlite`
  directly.
- Surfaces best-effort per-turn and per-session stats such as model, token usage, effective agent runtime, and
  observed patch-line counts through the query protocol.
- Preserves project continuity across checkout moves, worktrees, merges, and renames with built-in linking and
  rename workflows.

## Workspace Architecture

Darc is organized as a small workspace of focused crates with `darc-core` kept as a thin facade and orchestration layer.

- `darc-agent`: external agent runtime command preparation for future worker-backed derived context features.
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

Keep the shared workspace fresh in the foreground:

```bash
darc refresh --watch --all
```

On macOS, manage the same watcher as a user LaunchAgent:

```bash
darc service enable
darc service start
darc service status
```

## Commands

- `darc init` detects local sources and creates the shared Darc config.
- `darc refresh` is the daily happy path. It runs `sync` then `index` for the active project, or every registered
  project with `--all`. Add `--watch` to keep the same refresh workflow running in the foreground.
- `darc status` shows human-readable health for the active project. Add `--workspace` to summarize every configured
  project, and `--check` to run sync planning without writing manifests, config, archives, or SQLite.
- `darc sync` archives matching Claude and Codex sessions for the active project.
- `darc index` indexes archived sessions into SQLite.
- `darc query` exposes the machine-readable read protocol for workspace, session, turn, file-pivot, search, and
  insights data. Query commands emit JSON by default; see [Query protocol](docs/query-protocol.md).
- `darc link`, `darc remove`, and `darc rename-from` manage renamed or merged projects.
- `darc service` manages the background refresh service. Service management is currently macOS-only; see
  [Background refresh service](docs/service.md).

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

Darc's read-side query surface now covers project-scoped search, compact turn skims, file/session pivots, and
single-call session bundles.

- `darc query search turns <query>` defaults to keyword search and also supports literal, regex, file-name,
  glob-compatible file-path, and path-fragment modes with optional provider/session/time filters. Literal and regex
  search skip bulky tool outputs by default; add `--include-tool-output` for forensic searches over command output,
  logs, or stack traces, use `--field` / `--exclude-field` to narrow exact evidence fields, and use `--match-limit`
  to cap nested evidence matches per returned turn hit.
- project-scoped `darc query` commands accept optional `--project-id`; when omitted, Darc resolves the configured
  project from the current directory. Project pivots such as `sessions`, `files`, and `insights project` also accept
  `--provider` when a corpus mixes Codex and Claude history.
- `darc query sessions` defaults to compact first-prompt and final-message previews for browsing; pass `--view full`
  when you need the full text pair. Preview rows include returned and total character counts, and edited file lists are
  deduplicated and always complete for each returned session.
- `darc query turns` lists one known session by full UUID, inferring the provider unless the id is cross-provider
  ambiguous; content discovery lives under `darc query search turns`.
- `darc query files` ranks most-touched files for initial discovery. `darc query files <path>`,
  `darc query session-files <session-id>`, and `darc query session-bundle <session-id>` let clients pivot between
  matched files, touched sessions, per-session file summaries, and bounded one-call session detail bundles. File pivots
  accept provider and time filters in most-touched, path, and co-touch modes. Session bundles cap embedded
  `session_files` previews at 100 rows; use `darc query session-files` for the standalone full list. Turn detail and
  session bundle reads default to narrative payloads and bounded step pages; pass `--view full`, `--step-limit`, or
  `--include-raw` only when more detail is needed.
- Broad file/path queries cap each row's `matched_paths` preview by default; use `--matched-path-limit` or
  `--include-all-matched-paths` when you need more path evidence per result. Search/file payloads expose count fields
  such as `matched_paths_count`, `matches_count`, and `session_file_count` so agents can estimate returned context.
- `darc query resolve-session` explicitly expands a UUID prefix before you call session-scoped data commands and
  includes `project_id` with each match for multi-project roots.

For compact-first agent exploration, start with small list/search pages, then drill down:

```bash
darc query sessions --limit 5
darc query files --limit 10
darc query search turns "panic unwrap" --limit 5
darc query turns "$ID" --view oneline --limit 10
darc query turn "$ID" 0 --step-limit 10
darc query session-bundle "$ID" --turn-limit 5 --step-limit 10
```

Examples:

```bash
darc query search turns \
  --project-id repo-abc123 \
  "panic unwrap"
```

```bash
darc query search turns \
  --project-id repo-abc123 \
  --mode literal \
  --query "--output-last-message" \
  --exclude-field tool-arguments \
  --since 14d
```

```bash
darc query search turns \
  --project-id repo-abc123 \
  --mode regex \
  "panic: .*" \
  --include-tool-output
```

```bash
darc query files \
  --project-id repo-abc123 \
  --since 30d \
  --limit 20
```

```bash
darc query files \
  --project-id repo-abc123 \
  "src/components/**/*.rs" \
  --since 30d
```

```bash
darc query files \
  --project-id repo-abc123 \
  --co-touched-with src/components/planner.rs \
  --since 30d
```

```bash
ID=$(darc query resolve-session 11111111 --pick-one | jq -r '.data.match.session_id')

darc query session-bundle \
  --project-id repo-abc123 \
  "$ID" \
  --turn-limit 20 \
  --step-limit 20
```

See [Query protocol](docs/query-protocol.md) for the full command matrix, payload contracts, and filter semantics.

Run `darc --help` for the visible CLI surface. Hidden maintainer commands are documented separately.

## Documentation

- [Documentation index](docs/README.md)
- [Query protocol](docs/query-protocol.md)
- [Project rename and linking](docs/project-rename.md)
- [Schema audits](docs/schema-audits.md)
- [Claude support](docs/claude-support.md)
- [Backlog](docs/todo.md)
