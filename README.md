# Darc

[![License](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)
[![CI](https://github.com/0xjunha/darc/actions/workflows/ci.yml/badge.svg)](https://github.com/0xjunha/darc/actions/workflows/ci.yml)
[![codecov](https://codecov.io/github/0xjunha/darc/graph/badge.svg?token=J5ZVVBJ3U9)](https://codecov.io/github/0xjunha/darc)

**Darc** is **queryable cross-session memory for coding agents**: a local CLI that archives agent session rollouts,
indexes them into a SQLite database, and exposes them through structured query commands.

## What is Darc?

**Darc** turns coding agent session history into a queryable **D**ata **Arc**. Think of it as `rg` (ripgrep) for agent
history, with stable evidence handles back to the original turns.

**The core idea:** building a good agent memory system is hard. Agents are already intelligent. Instead of distilling
their work into lossy summaries, Darc gives them a tool to recover the exact context on demand: what happened, which
files were touched, and how turns link together.

Darc is the **retrieval/evidence half of agent memory**. It stores sessions as-is and indexes them for queryable
lookup. It does not summarize, consolidate or rewrite agent state, so pair it with whatever memory layer your agent
already uses
(AGENTS.md, Codex/Claude Code built-in memory, MCP-backed memory tools) for the summarization side.

Supported agents: **Claude Code**, **Codex**.

![Darc architecture: agent rollouts → sync → archive → SQLite index → query CLI](assets/darc-architecture.png)

## Why Darc

- **Bounded JSON, not log dumps.** Every read returns a small, schema-tagged envelope with sane `--limit` defaults —
  designed to fit in a context window.
- **Stable evidence handles.** `session_id`, `turn_ordinal`, file paths, and timestamps survive re-syncs and renames,
  so an agent can quote them today and resolve them next week.
- **Pivot through files.** Find sessions that touched a path, files commonly changed alongside it, and the turns
  behind both.
- **Project continuity.** History survives checkout moves, worktrees, and repository renames via stable Darc project
  ids.
- **Local-first.** Darc reads from local agent rollouts and writes archive/query state under `~/.darc`. Optional upgrade
  checks contact GitHub only for release metadata and require explicit opt-in.

## Quickstart

Install the latest release (macOS / Linux):

```sh
curl -fsSL https://github.com/0xjunha/darc/releases/latest/download/darc-installer.sh | sh
```

Run Darc from a project where you already use Claude Code or Codex:

```sh
cd /path/to/project

darc init                       # register the current project in the shared `~/.darc` workspace
darc refresh                    # sync new session rollouts into `~/.darc` and index into the SQLite DB
darc refresh --provider claude  # limit refresh to one provider
darc refresh --all              # refresh every registered project
darc refresh --watch --all      # for continuous foreground refresh

# On macOS, the same refresh workflow can run automatically in the background:
darc service enable  # turn on automatic background refresh after reboot
darc service start   # start background refresh now (auto-refresh)
darc service status  # check whether background refresh is enabled and running
```

Then run the read commands:

```sh
# Check active project state and freshness
darc status

# Browse recent activity in this project
darc list sessions --limit 5                             # recent indexed sessions
darc list files --limit 10                               # rank most-touched files
darc search "panic unwrap" --limit 5                     # keyword search across turn evidence

# Narrow with time windows, file globs, and providers
darc list sessions --since 7d --touching "src/**/*.rs"   # sessions touching Rust src this week
darc search --mode regex --query "error\s+code" \
  --include-tool-output --since 7d --limit 5             # regex into tool output, time-bounded
darc search --mode file-path "docs/**/*.md" --limit 5    # find sessions that touched matching files

# Pivot through related files
darc list files --co-touched-with src/lib.rs --limit 10  # files commonly changed alongside lib.rs

# Drill down to exact evidence using ids returned above
darc list turns <SESSION_ID> --view oneline --limit 20         # compact one-line skim of a session's turns
darc show session <SESSION_ID> --turn-limit 5 --step-limit 10  # bounded session bundle after narrowing
darc show turn <SESSION_ID> <TURN_ORDINAL> --step-limit 10     # exact turn evidence with bounded steps

# Project metrics: tools, models, active time, top files
darc stats project --turn-limit 200
```

Check for newer Darc CLI releases:

```sh
darc upgrade --check
darc upgrade --check --json
darc upgrade
```

Darc can show a short startup nudge when a newer release is available. To enable it, set
`check_for_update_on_startup = true` in `~/.darc/config.toml`. Write-oriented human commands such as `refresh`, `sync`,
`index`, and mutating project/service commands read the cached release metadata under `~/.darc/run`; when the cache is
stale, Darc refreshes it after the command completes. Read-only commands such as `status`, `search`, `list`, and
`service status` do not perform passive checks. Set `DARC_NO_UPDATE_CHECK=1` to suppress passive checks for one process.
To hide one release:

```sh
darc upgrade dismiss <VERSION>
darc upgrade dismiss --root <ROOT> <VERSION>  # custom Darc root
```

## Uninstall

If you enabled the macOS background refresh service, turn it off before removing the binaries:

```sh
darc service disable
```

Then remove the binaries installed by the release installer:

```sh
rm -f ~/.local/bin/darc ~/.local/bin/darc-update
```

If you installed Darc into a custom directory, remove both binaries from that directory instead.

Darc keeps local data under `~/.darc`. Uninstalling the binary does not delete that archive. To delete Darc data too:

```sh
rm -rf ~/.darc
```

If you used `--root <path>` with Darc, remove that custom root instead.

## Concepts

Darc keeps one local **workspace** at `~/.darc`. A workspace contains many **projects** (one per registered checkout),
each with its own archive of synced sessions; a single workspace-wide SQLite index normalizes all archives for query.
The **active project** is whichever project Darc resolves from your current working directory — most read commands
infer it automatically, so `cd` into a project and just run.

A **session** is one run of Claude Code or Codex. A **turn** is one user-message-and-response pair within a session.
Darc identifies sessions by `session_id` and turns by `turn_ordinal`. These handles are stable across re-syncs,
worktrees, and project renames, so agents can quote them across conversations.

Read commands always emit JSON envelopes tagged with a `schema` id (e.g. `darc.query.search.turns.v1`) and a `data`
payload. Pass `--color never` when piping into another program that needs guaranteed plain JSON.

## Everyday Commands

| Command                         | Use                                                                                                          |
|---------------------------------|--------------------------------------------------------------------------------------------------------------|
| `darc init`                     | Detect local coding-agent sources and register the current project.                                          |
| `darc refresh`                  | Sync then index the active project. This is the daily command.                                               |
| `darc status`                   | Check active-project or workspace health. Use `--json` for a machine-readable preflight.                     |
| `darc list sessions`            | Browse recent indexed sessions with compact prompt/final-message previews.                                   |
| `darc list turns <id>`          | List turns for one session. Use `--view oneline` for a compact skim.                                         |
| `darc list files`               | Rank touched files, find sessions touching a path, list files for one session, or pivot to co-touched files. |
| `darc list projects`            | List configured projects in the workspace.                                                                   |
| `darc search <query>`           | Search indexed turns. Each hit includes `session_id` and `turn_ordinal` for follow-up reads.                 |
| `darc show turn <id> <n>`       | Inspect one turn with bounded step output. Add `--include-insights` for derived metrics.                     |
| `darc show session <id>`        | Inspect one bounded session bundle with summary, turn page, and file preview.                                |
| `darc show workspace`           | Workspace/sidebar payload — projects and recent activity at a glance.                                        |
| `darc stats project`            | Show indexed project metrics such as active time, tools, files, models, and token fields when available.     |
| `darc stats workspace`          | Cross-project rolling-window stats for the whole workspace.                                                  |
| `darc resolve session <prefix>` | Resolve a UUID prefix into canonical session ids.                                                            |
| `darc project ...`              | Link, rename, remove, or rebuild projects after moves and renames.                                           |
| `darc service ...`              | Manage the beta macOS background refresh service.                                                            |
| `darc upgrade`                  | Check for or apply newer Darc CLI releases.                                                                  |

Run `darc --help` or `darc help <command>` for the current visible CLI surface.

## The Context-Building Loop

Darc is strongest when an agent uses it as an evidence ladder:

1. Preflight the active project with `darc status` or `darc status --json`.
2. Discover candidates with small `list`, `search`, or `stats` reads.
3. Skim a candidate session with `darc list turns <SESSION_ID> --view oneline`.
4. Drill into one turn with `darc show turn <SESSION_ID> <TURN_ORDINAL>`.
5. Pull a bounded broader view with `darc show session <SESSION_ID>` only after narrowing.
6. Pivot through touched files (`--co-touched-with`, `--touching`) to find adjacent work, tests, docs, and follow-up
   sessions.

That loop lets an agent answer "what happened here before?" without dumping an entire transcript archive into the
prompt.

## Recipes

Find exact text without regex escaping (and restrict to user prompts only):

```sh
darc search \
  --mode literal \
  --query "--output-last-message" \
  --field user-message \
  --limit 5
```

Resume from a partial UUID an agent quoted earlier:

```sh
SESSION_ID=$(darc resolve session <SESSION_PREFIX> --pick-one --color never \
  | jq -r '.data.match.session_id')
darc show session "$SESSION_ID" --turn-limit 10 --step-limit 10
```

Inspect one session's full file footprint:

```sh
darc list files --session <SESSION_ID> --limit 50
```

Enrich one turn with derived insights (tools used, files touched, token usage, duration):

```sh
darc show turn <SESSION_ID> <TURN_ORDINAL> --include-insights
```

Workspace-wide activity and inventory:

```sh
darc stats workspace --window 14d  # rolling-window cross-project stats
darc list projects                 # which projects are registered in this workspace
darc show workspace                # active project plus recent-activity sidebar payload
```

Extract ids for downstream scripting:

```sh
darc list sessions --limit 5 --color never \
  | jq -r '.data.sessions[] | [.provider, .session_id, .latest_turn_at] | @tsv'
```

## Agent-Friendly Design

Darc is built to be useful from inside an agent's context window, not just from a developer's terminal. Three design
choices follow from that:

**Bounded by default.** Every read command has small `--limit`, `--turn-limit`, and `--step-limit` defaults. A session
bundle returns a paginated turn page plus a capped file preview, not the whole session. `darc list turns --view oneline`
collapses each turn to a single preview row for fast skimming, and `darc show turn`/`show session --view narrative`
(the default) omits tool arguments, outputs, and raw payload blobs — pass `--include-raw` to opt back in. Agents should
prefer the smallest read that answers the question and only widen pagination when the previous read clearly justified
it.

**Filter aggressively, then drill.** Combine `--since`, `--touching`, `--provider`, `--mode`, `--field`, and
`--co-touched-with` to narrow before reading evidence. Each search hit returns the `session_id` and `turn_ordinal`
needed for the next call — agents should chain narrow reads instead of asking for "everything" and post-filtering in
the prompt.

**Stable, machine-readable contracts.** Read commands always emit JSON envelopes with a `schema` id and a `data`
payload. Project resolution from CWD means agents rarely need to pass `--project-id`; pass it (or
`--provider claude` / `--provider codex`) only when running outside a registered checkout or against a mixed archive.
Use `--color never` to guarantee plain JSON on stdout.

See [Query protocol](docs/query-protocol.md) for the full command matrix, payload schemas, pagination rules, search
modes, and error contracts.

## Project Moves And Renames

Darc stores stable project identity under configured project names, so history can survive checkout moves and
repository renames.

The safe bundled workflow for a renamed project is:

```sh
cd /path/to/new-project
darc project rename-from <old-project-name> --dry-run
darc project rename-from <old-project-name>          # rerun without --dry-run after reviewing
```

`rename-from` is equivalent to `link <old> + refresh + remove <old>`, run as one step. Use the lower-level commands
when you want manual control:

```sh
darc project link <old-project-name> --dry-run       # bring an old project's known paths into the current one (non-destructive)
darc project remove <old-project-name> --dry-run     # delete a configured project, its archive, and its indexed rows (destructive)
```

Always run with `--dry-run` first and rerun without it once the reported changes look right.
See [Project rename and linking](docs/project-rename.md).

## Documentation

- [Documentation index](docs/README.md)
- [Query protocol](docs/query-protocol.md)
- [Background refresh service](docs/service.md)
- [Project rename and linking](docs/project-rename.md)

## Development

Darc is split into focused Rust crates:

- `crates/cli`: command-line surface.
- `darc-core`: thin facade and orchestration layer.
- `darc-sync`: source discovery, sync planning, and archive copy execution.
- `darc-index`: normalized ingestion, SQLite schema, migrations, and indexing metrics.
- `darc-query`: read-only query, search, and stats over indexed data.
- `darc-rollout`: provider transcript models and parsers.
- `darc-paths`, `darc-agent`, `darc-rollout-audit`, and `darc-test-utils`: shared support, runtime preparation,
  maintainer audits, and tests.

Useful checks:

```sh
cargo +nightly fmt
cargo +stable clippy --locked --workspace --all-targets --all-features -- -D warnings -W clippy::all
cargo +stable test --locked --workspace
cargo +stable check --locked --workspace --all-targets --all-features --profile dist
```
