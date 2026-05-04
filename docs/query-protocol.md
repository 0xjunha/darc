# Darc Query Protocol

Darc's machine-readable CLI surfaces emit JSON envelopes for coding agents, desktop, and other clients. The callable
read surface is `darc list`, `darc show`, `darc search`, `darc stats`, `darc resolve`, and `darc status --json`; the
release-check surface is `darc upgrade --check --json`. Most read schema ids retain the `darc.query.*` prefix because
they describe payload contracts, not the CLI namespace; status uses `darc.status.*` schemas.

Use these JSON read commands instead of:

- opening `index.sqlite` directly
- parsing human-oriented command output
- deriving analytics from raw `steps_json` outside `darc`

## Canonical JSON Commands

The canonical JSON commands emit the same schema ids documented below:

- `darc list projects` and `darc show workspace` emit `darc.query.workspace.v1`
- `darc status --json` emits `darc.status.project.v1` for the active cwd-resolved project
- `darc status --workspace --json` emits `darc.status.workspace.v1`
- `darc list sessions` emits `darc.query.sessions.v1`
- `darc list turns <session-id-or-prefix>` emits `darc.query.turns.v1`
- `darc list files` emits `darc.query.files.v1` in most-touched mode
- `darc list files <path-or-glob>` and `darc list files --path <path-or-glob>` emit `darc.query.files.v1` in path
  mode
- `darc list files --session <session-id-or-prefix>` emits `darc.query.session_files.v1`
- `darc list files --co-touched-with <path>` emits `darc.query.files.v1` in co-touch mode
- `darc show session <session-id-or-prefix>` emits `darc.query.session_bundle.v1`
- `darc show turn <session-id-or-prefix> <turn-ordinal>` emits `darc.query.turn.v1`
- `darc search <query>` searches indexed turns and emits `darc.query.search.turns.v1`; every hit includes
  `session_id` and `turn_ordinal`
- `darc stats workspace`, `darc stats project`, and `darc stats turn <session-id-or-prefix> <turn-ordinal>` emit the
  corresponding `darc.query.insights.*.v1` payloads
- `darc resolve session <uuid-or-prefix>` emits `darc.query.resolve_session.v1`
- `darc upgrade --check --json` emits `darc.upgrade.check.v1`

Each canonical query command accepts `--color <auto|always|never>` and `--root <path>` before or after its subcommand
or arguments. Color is terminal-only presentation; the default is `--color auto`. `darc status --json` emits plain
JSON and accepts `--root`.

The old `darc query ...` CLI namespace is not callable on the current surface. The `darc.query.*` names below are
schema ids retained for machine-readable payload compatibility.

## Command Matrix

Query commands emit pretty-printed JSON envelopes on stdout. `--color <auto|always|never>` controls terminal-only ANSI
presentation; the default is `--color auto`. Status and upgrade-check JSON emit the same envelope shape without ANSI
color.

- `darc list projects [--root <path>]`
- `darc show workspace [--root <path>]`
- `darc status --json [--root <path>] [--check]`
- `darc status --workspace --json [--root <path>] [--check]`
- `darc resolve session <uuid-or-prefix> [--root <path>] [--project-id <id>] [--provider <provider>] [--pick-one]`
- `darc list sessions [--root <path>] [--project-id <id>] [--provider <provider>] [--view <compact|full>] [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--touching <glob>] [--limit <n>] [--offset <n>]`
- `darc list files [--root <path>] [--project-id <id>] [--provider <provider>] [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] [--offset <n>]`
- `darc list files [--root <path>] [--project-id <id>] [--provider <provider>] <path-or-glob> [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] [--offset <n>] [--matched-path-limit <n>|--include-all-matched-paths]`
- `darc list files [--root <path>] [--project-id <id>] [--provider <provider>] --path <path-or-glob> [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] [--offset <n>] [--matched-path-limit <n>|--include-all-matched-paths]`
- `darc list files [--root <path>] [--project-id <id>] [--provider <provider>] --session <session-id-or-prefix> [--limit <n>] [--offset <n>]`
- `darc list files [--root <path>] [--project-id <id>] [--provider <provider>] --co-touched-with <path> [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] [--offset <n>]`
- `darc show session [--root <path>] [--project-id <id>] [--provider <provider>] <session-id-or-prefix> [--session-view <compact|full>] [--view <full|narrative>] [--turn-limit <n>] [--turn-offset <n>] [--step-limit <n>] [--step-offset <n>]`
- `darc list turns [--root <path>] [--project-id <id>] [--provider <provider>] <session-id-or-prefix> [--view <full|oneline>] [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] [--offset <n>]`
- `darc show turn [--root <path>] [--project-id <id>] [--provider <provider>] <session-id-or-prefix> <turn-ordinal> [--view <full|narrative>] [--step-limit <n>] [--step-offset <n>] [--include-raw] [--include-insights]`
- `darc search [--root <path>] [--project-id <id>] [--provider <provider>] [--session-id <id-or-prefix>] <query> [--mode <keyword|literal|regex|file-name|file-path|path-fragment>] [--include-tool-output] [--field <field>] [--exclude-field <field>] [--match-limit <n>] [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] [--offset <n>] [--matched-path-limit <n>|--include-all-matched-paths]`
- `darc search [--root <path>] [--project-id <id>] [--provider <provider>] [--session-id <id-or-prefix>] --query <query> [--mode <keyword|literal|regex|file-name|file-path|path-fragment>] [--include-tool-output] [--field <field>] [--exclude-field <field>] [--match-limit <n>] [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] [--offset <n>] [--matched-path-limit <n>|--include-all-matched-paths]`
- `darc stats workspace [--root <path>] [--window <days>d] [--recent-session-limit <n>] [--recent-session-offset <n>]`
- `darc stats project [--root <path>] [--project-id <id>] [--provider <provider>] [--turn-limit <n>]`
- `darc stats turn [--root <path>] [--project-id <id>] [--provider <provider>] <session-id-or-prefix> <turn-ordinal>`
- `darc stats turn [--root <path>] [--project-id <id>] [--provider <provider>] --session-id <session-id-or-prefix> --turn-ordinal <n>`
- `darc upgrade --check --json`

## Performance Expectations

The read surface is designed for large indexed histories by making exploratory calls explicitly bounded and by keeping
the expensive pivots inside SQLite where possible.

Committed benchmark coverage is synthetic-only. Run the repeatable CLI benchmark suite with:

```bash
scripts/bench_cli_reads.sh
```

The benchmark creates a temporary Darc root with deterministic synthetic sessions, turns, search evidence, and file
touches, then repeatedly times the canonical `list`, `show`, and `search` commands. Scale and repeat count can be
changed without touching committed fixtures:

```bash
DARC_BENCH_SESSIONS=240 DARC_BENCH_TURNS=24 DARC_BENCH_REPEAT=7 scripts/bench_cli_reads.sh
```

Benchmark scenarios cover broad and narrow queries, pagination and offsets, file/path pivots, exact literal and regex
search, no-match cases, large output cases, and repeated-run timing. Real local Darc data is appropriate for ad hoc
live measurement only; do not copy local session ids, paths, or transcript content into committed fixtures.

Expected scale shape:

- default `list` and `search` pages return 10 rows and report `has_more`; `--limit 0` is a cheap existence probe
- `darc show session` returns a bounded bundle by default: 5 turns, 10 steps per turn, and a 100-row session-file
  preview
- `darc show turn` and `darc show session` should stay proportional to requested turn and step bounds unless callers
  explicitly request larger pages or raw payloads
- `darc search --mode keyword` uses the indexed FTS table and paginates in SQL
- `darc search --mode literal` uses SQLite substring prefiltering over selected exact evidence fields before building
  capped per-hit match previews
- `darc search --mode regex` may scan selected evidence rows in Rust; it skips bulky `tool-output` evidence unless
  `--include-tool-output` is passed
- file-name, file-path, path-fragment, touched-path, and co-touch pivots use indexed file-access rows with explicit
  result pagination; broad co-touch ranking is computed from distinct session/path rows in SQLite instead of
  materializing full per-file summaries first
- response size grows with explicit caller-controlled bounds: `--limit`, `--turn-limit`, `--step-limit`,
  `--matched-path-limit`, and `--match-limit`

## Argument rules

- project-scoped queries accept optional `--project-id`; when omitted, Darc resolves the configured project from the current directory
- `darc show workspace` and `darc list projects` include nullable `active_project` with the cwd-resolved project id, name, and current root when the current directory matches a configured project; a neutral cwd returns `active_project: null` without adding a root issue
- `darc status --json` reports the same active-project status as human `darc status`; add `--check` to include a non-mutating sync plan under `data.project.sync_check`; failed JSON status checks write the status report to stdout, return non-zero, and write a `darc.error.v1` envelope to stderr
- `darc upgrade --check --json` contacts GitHub Releases, writes `darc.upgrade.check.v1` to stdout on success, and
  writes a `darc.error.v1` envelope to stderr on argument, network, HTTP, or release-metadata parse failures
- canonical read commands accept shared `--root` and `--color` options before or after nested subcommands, so both
  `darc list --root ~/.darc sessions` and `darc list sessions --root ~/.darc` are valid
- `--color auto` adds ANSI syntax color only when stdout is a terminal, `NO_COLOR` is unset, and `TERM` is not `dumb`; piped, redirected, and captured output remains plain JSON by default
- use `--color always` for terminal pagers such as `less -R`, or `--color never` for plain JSON in every environment
- colored search output may highlight mode-specific preview fields: `keyword` highlights visible terms inside `data.hits[*].snippet`, `literal` and `regex` highlight matched substrings inside `data.hits[*].matches[*].snippet`, `file-name` / `path-fragment` highlight matched substrings inside `data.hits[*].matched_paths[*]`, and `file-path` highlights each matched path item as a whole; this is terminal presentation only and does not add response fields
- project-wide provider filters default to all providers when `--provider` is omitted
- `darc list sessions` defaults to `--view compact`; pass `--view full` for full `first_user_prompt` and `final_agent_message` text. Preview fields include returned and total character counts. `edited_files` is deduplicated and always complete for each returned session row.
- `darc resolve session` accepts either one full UUID or one UUID prefix and returns `project_id`, `provider`, and `session_id` for each match
- `darc search` defaults to `--mode keyword`; pass `--mode` only for literal, regex, or file/path search modes
- `darc search` accepts query text positionally or with `--query`; use `--query` for query text that begins with `-`
- `darc search` is the canonical turn-search command; its default mode is keyword, and
  `--mode <literal|regex|file-name|file-path|path-fragment>` selects the other turn search modes without requiring
  the word `turns` in the command
- `darc stats workspace`, `darc stats project`, and `darc stats turn` are the canonical names for the protocol
  `insights` payloads
- `darc list files` with no path selector ranks most-touched files; positional `<path-or-glob>` uses path mode, and
  `--path` is the explicit equivalent
- session-scoped read commands use the identity form shown in the command matrix: `darc list files` uses
  `--session`, while `darc list turns`, `darc show session`, `darc show turn`, and `darc stats turn` accept a
  positional session id or `--session-id`; Darc infers `--provider` when that id or prefix is unique within the project
- turn-scoped commands require a turn ordinal supplied either positionally or with `--turn-ordinal`
- do not pass both positional and flag forms for the same value
- pass `--provider` when the same session id or prefix exists for multiple providers
- `darc list files` accepts at most one of positional path, `--path`, `--session`, or `--co-touched-with`; omit all
  four for most-touched mode (`mode=top`)
- `--since` and `--until` on `darc list files` apply to most-touched (`mode=top`), path, and co-touch modes
- `--limit` and `--offset` are accepted by `darc list sessions`, `darc list turns`,
  `darc list files --session`, `darc search`, and every `darc list files` mode; these row/turn-hit limits default to
  `--limit 10 --offset 0`
- `--limit 0` returns an empty page while preserving `has_more`; this is useful as a cheap probe for whether matching
  rows exist
- `--matched-path-limit` caps per-row `matched_paths` previews in `darc list files` path mode and file-search modes;
  it defaults to `20`, and `--include-all-matched-paths` removes that preview cap
- `--turn-limit` and `--turn-offset` on `darc show session` bound embedded turn details and default to `--turn-limit 5 --turn-offset 0`
- `--session-view` on `darc show session` defaults to `compact`, which caps the embedded first prompt and final agent message the same way `darc list sessions --view compact` does; pass `--session-view full` when the complete text pair is needed
- embedded `session_files` in `darc show session` is capped at 100 file rows; use paginated `darc list files --session` when a caller needs the standalone file list
- `darc show turn` and `darc show session` default to `--view narrative`; pass `--view full` when raw tool arguments, outputs, or payload blobs are needed
- `--step-limit` and `--step-offset` on `darc show turn` and `darc show session` bound returned turn steps and default to `--step-limit 10 --step-offset 0`
- `--turn-limit` on `darc stats project` is an inspection bound over indexed turns, not response pagination; the previous `--limit` spelling is accepted as a compatibility alias, and the response echoes `turn_limit`, `inspected_turn_count`, and `turns_has_more`
- `--recent-session-limit` and `--recent-session-offset` on `darc stats workspace` bound the `recent_sessions` preview and default to `--recent-session-limit 50 --recent-session-offset 0`
- `--include-tool-output` on `darc search` is accepted only with `--mode literal` or `--mode regex`
- `--field` and `--exclude-field` on `darc search` are accepted only with `--mode literal` or `--mode regex`; field values accept CLI kebab-case such as `user-message` and stable protocol snake_case such as `user_message`
- `--match-limit` on `darc search` is accepted only with `--mode literal` or `--mode regex`; it caps nested `matches` entries per returned turn hit and defaults to `20`
- `--field tool-output` requires `--include-tool-output`
- session-scoped data commands accept a full UUID session id or an unambiguous UUID prefix; malformed ids return
  `invalid_session_id`, unknown ids or prefixes return `unknown_session`, and ambiguous prefixes or cross-provider ids
  return `ambiguous_session`

## Common Workflows

The read surface is intentionally composable. Use the canonical commands for day-to-day protocol reads.

- compact-first exploration for coding agents:

  ```bash
  darc list sessions --limit 5
  darc list files --limit 10
  darc search "staged init" --limit 5
  darc list turns 11111111 --view oneline --limit 10
  darc show turn 11111111 0 --step-limit 10
  darc show session 11111111 --turn-limit 5 --step-limit 10
  ```

- terminal review with explicit color policy:

  ```bash
  darc search --color always "staged init" --limit 5 | less -R
  darc search --color never "staged init" --limit 5
  ```

- find planning turns by content:

  ```bash
  darc search \
    --root ~/.darc \
    --project-id repo-abc123 \
    "staged init" \
    --since 14d
  ```

- verify exact evidence text without regex escaping; literal and regex searches skip bulky tool outputs by default:

  ```bash
  darc search \
    --root ~/.darc \
    --project-id repo-abc123 \
    --mode literal \
    --query "--output-last-message" \
    --exclude-field tool-arguments
  ```

- search command output or logs explicitly for forensic work:

  ```bash
  darc search \
    --root ~/.darc \
    --project-id repo-abc123 \
    --mode regex \
    "panic: .*" \
    --include-tool-output
  ```

- list most-touched files for initial discovery:

  ```bash
  darc list files \
    --root ~/.darc \
    --project-id repo-abc123 \
    --since 30d \
    --limit 20
  ```

- pivot from a file path to the sessions that touched it:

  ```bash
  darc list files \
    --root ~/.darc \
    --project-id repo-abc123 \
    src/components/planner.rs \
    --limit 20
  ```

- inspect all in-project files touched by one session:

  ```bash
  darc list files \
    --root ~/.darc \
    --project-id repo-abc123 \
    --session 11111111
  ```

- fetch one session summary, narrative turn detail, and touched files in one call:

  ```bash
  ID=$(darc resolve session 11111111 --pick-one | jq -r '.data.match.session_id')

  darc show session \
    --root ~/.darc \
    --project-id repo-abc123 \
    "$ID" \
    --turn-limit 20 \
    --step-limit 20
  ```

- skim one long session as one compact row per turn:

  ```bash
  darc list turns \
    --root ~/.darc \
    --project-id repo-abc123 \
    11111111 \
    --view oneline \
    --limit 10
  ```

## Success envelope

JSON command success responses are written to `stdout` only.

```json
{
  "schema": "darc.query.workspace.v1",
  "generated_at": "2026-04-06T12:00:00Z",
  "darc_version": "0.1.0",
  "data": {}
}
```

Fields:

- `schema`: stable protocol schema id for the specific command and version
- `generated_at`: UTC ISO 8601 timestamp generated by the CLI
- `darc_version`: the Darc package version that emitted the response
- `data`: command-specific payload

## Error envelope

JSON runtime failures and argument parse failures return non-zero exit status and write a structured error envelope to `stderr`.

```json
{
  "schema": "darc.error.v1",
  "generated_at": "2026-04-06T12:00:00Z",
  "darc_version": "0.1.0",
  "error": {
    "code": "unknown_session",
    "message": "No session matched prefix `11111111`.",
    "details": {
      "session": "11111111",
      "looks_like_prefix": true
    },
    "causes": []
  }
}
```

Fields:

- `error.code`: optional stable machine-readable error code
- `error.message`: top-level error message
- `error.details`: optional structured metadata for known error codes
- `error.causes`: causal chain in outer-to-inner order, excluding the top-level message

Current stable JSON error codes:

- `invalid_arguments`: JSON read command arguments were rejected before dispatch, for example because an option was unknown, required input was missing, or two options conflicted
- `missing_required_identity`: a session id, turn ordinal, query, or similar read identity was not supplied in any accepted positional or flag form
- `conflicting_identity_arguments`: the same read identity was supplied in incompatible positional and flag forms
- `status_check_failed`: `darc status --json --check` or `darc status --workspace --json --check` completed its status report but at least one sync-check plan failed
- `invalid_session_id`: the supplied resolver query or data-command session id is not a UUID or accepted UUID-prefix shape
- `unknown_session`: the full UUID or prefix did not resolve to an indexed session
- `ambiguous_session`: `darc resolve session --pick-one` found more than one candidate, or a session-scoped data command found multiple matching sessions; pass `--provider`, `--project-id`, or a longer prefix to choose one session

## Schema ids

Current schema ids:

- `darc.query.workspace.v1`
- `darc.query.resolve_session.v1`
- `darc.query.sessions.v1`
- `darc.query.files.v1`
- `darc.query.session_files.v1`
- `darc.query.session_bundle.v1`
- `darc.query.turns.v1`
- `darc.query.turn.v1`
- `darc.query.search.turns.v1`
- `darc.query.insights.workspace.v1`
- `darc.query.insights.project.v1`
- `darc.query.insights.turn.v1`
- `darc.status.project.v1`
- `darc.status.workspace.v1`
- `darc.upgrade.check.v1`
- `darc.error.v1`

Clients should branch on `schema`, not on `darc_version`.

`darc list turns` supports two projections:

- `view: "full"` keeps the full turn-summary object and includes 500-character user/agent previews plus stats fields such as `tool_call_count`, tokens, runtime, and patch counts
- `view: "oneline"` returns a smaller per-turn object with `turn_ordinal`, `role`, first-line `user_prompt_preview`, first-line `agent_answer_preview`, preview size metadata, `step_count`, and `tool_call_count`
- both projections include `limit`, `offset`, and `has_more`, and default to the first 50 turns
- `oneline` previews are derived from the first source line and capped at 300 characters
- session-scoped oneline rows currently emit `role: "user"` because the preview always comes from the first user message line

## Stability rules

The protocol is still in development.

Current `v1` schemas are the active working contract for Darc Desktop and may still evolve before stabilization.

Target rules within one schema version:

- field meaning must stay stable
- field names must stay stable
- enum spellings must stay stable
- responses may add new fields
- responses may add new array items

After stabilization, breaking changes should require a new schema id, such as `...v2`.

Examples of breaking changes:

- renaming a field
- removing a field
- changing a field type
- changing enum values
- changing payload semantics incompatibly

## Field rules

JSON payloads follow these rules:

- snake_case field names
- lowercase stable enum values
- UTC ISO 8601 timestamps
- explicit `null` for nullable values
- empty arrays instead of omitted list fields
- deterministic ordering where practical

## Upgrade Check

`darc.upgrade.check.v1` reports the explicit release check from `darc upgrade --check --json`.

Fields:

- `current_version`: non-null Darc CLI version string for the running binary
- `latest_version`: latest GitHub Release version without a leading `v`, or `null` when no latest release is published or accessible
- `upgrade_available`: `true` when `latest_version` is newer than `current_version` according to Darc's release-version comparison
- `latest_release_url`: GitHub Release URL for `latest_version`, or `null` when `latest_version` is `null`
- `install_command`: shell installer command for agents or scripts that need a manual fallback; when Darc can resolve the current executable directory, the command includes `DARC_INSTALL_DIR=<dir>` so custom installs update the invoked installation

## Analytics semantics

Some query analytics are now hardened as contract behavior, while others remain provisional.

### Active time

Current hardened rule:

- a turn contributes to active session time only when its status is `completed`
- the turn duration must be at least `2000` ms

Current non-rule:

- Darc does not yet exclude active time based on inferred long single-step spans
- any future exclusion policy of that kind should be treated as a semantic change and documented explicitly

### File analytics

Current file analytics are provisional heuristics derived from normalized tool-call steps.

Today:

- Darc extracts file-like arguments from explicit tool payload keys such as `file_path`, `path`, and `file`
- explicit tool names such as `read`, `grep`, and `view` count toward read-style file analytics
- explicit tool names such as `glob` and `list` count toward list-style file analytics
- explicit tool names such as `write`, `edit`, `replace`, and `patch` count toward write/edit-style file analytics
- Darc also derives file accesses from selected shell-like tools by parsing observed command forms
- current shell rules cover common explicit file-target commands such as `sed`, `rg`, `grep`, `cat`, `nl`, `ls`, `find`, `head`, `tail`, `awk`, `jq`, `cp`, `mv`, `rm`, `mkdir`, `touch`, `chmod`, and `apply_patch`
- shell commands only contribute file analytics when Darc can extract a concrete file-like path from the command text; obvious directory-only operands from list, search, and directory-creation commands are dropped, and implicit cwd-only access plus dynamic shell-variable expansion may still be omitted
- shell metadata such as `chmod` modes, `chown` owners, shell-test comparison operands, file-descriptor redirections, and operands containing unexpanded shell variables are not reported as paths
- this layer is best effort, not a perfect trace: archived rollouts record tool payloads and command text, not syscall-level file I/O, so commands such as `git`, `cargo`, inline Python, shell loops, subshells, or helper scripts may touch files without naming every path explicitly
- paths are reported as canonical display paths after Darc drops obvious directory-only operands such as `ls crates`, `find crates ...`, `rg foo crates`, or `mkdir -p scratch/cache`
- when a configured project root is available, repo-relative paths, `./`-prefixed paths, and absolute paths under that root are normalized to one project-scoped relative display path
- external absolute paths and other paths that cannot be normalized to the configured project root fall back to the stored extracted path string
- insights file-usage rows expose one `path` field and merge counts after this display-path normalization, so equivalent in-repo absolute and relative accesses report as one row

These rules may evolve before stabilization.

### Turn insights

`darc.query.insights.turn.v1` reports one turn's stored metrics plus one-turn tool/file analytics.

Today:

- top-level fields such as `primary_model`, `duration_ms`, `effective_agent_runtime_ms`, `total_token_count`, `token_usage`, `changed_file_count`, `added_line_count`, `removed_line_count`, `step_count`, `tool_call_count`, `tool_output_count`, `attachment_count`, `delegation_count`, `hook_summary_count`, and `has_final_answer` come from the indexed `turns` row for that exact turn
- `primary_model` is the best-effort user-visible model name stored for that turn; it may be `null` for older provider versions or transcripts that did not report a concrete model name
- `total_token_count` is the best-effort normalized cache-aware total token usage stored for that turn; it may be `null` for older provider versions or transcripts that did not report usable token counts
- `token_usage` reports the normalized per-turn token buckets Darc could derive: `input_uncached_token_count`, `cache_read_token_count`, `cache_write_token_count`, `output_token_count`, optional `reasoning_token_count`, `provider_total_token_count`, and `normalized_total_token_count`
- `reasoning_token_count` is currently a subset of `output_token_count`, not an additive peer bucket, so clients must not add it on top of `output_token_count`
- `provider_total_token_count` preserves provider-native semantics when the rollout reported one; for example, current Codex/OpenAI rollout totals can exclude cache buckets while Claude direct assistant rows do not report a native total at all
- unsupported or unreported buckets remain `null`; Darc does not synthesize zeroes for missing provider fields
- `effective_agent_runtime_ms` starts from the turn wall-clock duration and currently adds any delegated-runtime totals that Darc can extract from stable provider payloads
- `changed_file_count`, `added_line_count`, and `removed_line_count` are transcript-derived patch statistics; they count observed `apply_patch`-style edits, not a live git diff against the current repository state
- `tools` comes from normalized per-turn `tool_calls` rows, grouped by `tool_name`
- `shell_commands` comes from Darc-owned parsing of shell-like `tool_calls` payloads such as `exec_command`, `shell_command`, `shell`, and `Bash`
- each `shell_commands[*]` item currently reports the originating `tool_name`, the extracted `command_text`, and optional `workdir`
- `files` comes from normalized per-turn `file_accesses` rows, grouped by canonical display `path` after project-root normalization and after obvious directory-only operands are filtered during extraction
- `files[*].read_count` currently counts both `read` and `list` access kinds
- `files[*].write_count` currently counts both `write` and `edit` access kinds
- `tools` is ordered by higher `count` first, then `name` ascending
- `shell_commands` is ordered by tool call order within the turn
- `files` is ordered by higher total accesses first, then higher `write_count`, then higher `read_count`, then `path` ascending

Clients should treat these analytics as Darc-owned derived data and should not re-derive them from `steps_json`.

### Combined turn queries

`darc show turn --include-insights` embeds one derived `insights` block inside `darc.query.turn.v1`.

Today:

- the top-level turn detail fields remain unchanged
- `insights` includes `primary_model`, `duration_ms`, `effective_agent_runtime_ms`, `total_token_count`, `token_usage`, `changed_file_count`, `added_line_count`, `removed_line_count`, `tool_call_count`, `tool_output_count`, `attachment_count`, `delegation_count`, `hook_summary_count`, `has_final_answer`, `tools`, and `files`
- the embedded `insights.tools` and `insights.files` arrays follow the same derivation and ordering rules as `darc.query.insights.turn.v1`
- this command is the preferred single-round-trip protocol when a client needs both turn detail and turn analytics together

### Session and turn lists

`darc.query.sessions.v1` and `darc.query.turns.v1` surface the best-effort model, token, runtime, and observed patch-count fields needed for lightweight desktop list views.

Today:

- session-list payloads include `view`, which is `compact` by default and `full` when `--view full` is supplied
- session rows include `primary_model`, `total_token_count`, `token_usage`, `effective_agent_runtime_ms`, `changed_file_count`, `added_line_count`, `removed_line_count`, `first_turn_at`, `first_user_prompt`, `first_user_prompt_truncated`, `first_user_prompt_chars`, `first_user_prompt_total_chars`, `final_agent_message`, `final_agent_message_truncated`, `final_agent_message_chars`, `final_agent_message_total_chars`, `aborted_turn_count`, and `edited_files`
- session totals are rollups across the indexed turns in that session
- top-level session-list payloads additionally echo the resolved `provider`, `since`, `until`, and `touched_path` request filters as nullable fields, plus non-null `limit`, `offset`, and `has_more` pagination fields
- top-level turn-list payloads echo nullable `since` and `until` filters plus non-null `limit`, `offset`, and `has_more` pagination fields
- optional `--since` and `--until` filters apply to `latest_turn_at`, using inclusive lower-bound and exclusive upper-bound semantics
- optional `--touching` requires at least one session-scoped, project-scoped file access of any access type whose canonical display path matches the provided glob; Darc scans session candidates in `latest_turn_at` order after the `--since` / `--until` bounds and then applies the file-touch filter before pagination. The payload echoes this request as `touched_path`.
- `--since` and `--until` accept absolute ISO-8601 text or relative `<days>d` shorthand such as `5d`
- each `token_usage.*` session field is `null` unless every indexed turn in that session carried a value for that exact field
- `total_token_count` and `effective_agent_runtime_ms` are currently `null` on a session row unless every indexed turn in that session carried a value for that field
- `first_turn_at` and `first_user_prompt` come from the indexed turn with the minimum `turn_ordinal` in that session and are `null` only when the indexed session has no stored turns
- `final_agent_message` comes from the latest indexed turn in that session and is `null` when that turn has no final answer text
- in `view=compact`, `first_user_prompt` and `final_agent_message` are capped at 500 source characters; `*_chars` is the returned character count, `*_total_chars` is the source character count, and `*_truncated` reports whether additional text was omitted
- in `view=full`, `first_user_prompt` and `final_agent_message` are not capped, their paired `*_truncated` fields are false, and `*_chars` equals `*_total_chars`
- `aborted_turn_count` counts indexed turns in that session where `status` is `aborted`
- `edited_files` is the distinct project-scoped display list from session-scoped `file_accesses` rows with `access_type` of `edit` or `write`, preferring repo-relative paths for in-project files, excluding null or whitespace-only paths, and ordering by display path ascending
- `darc.query.turns.v1` remains session-scoped and keeps non-null top-level `provider` and `session_id`; provider is inferred unless the session id is cross-provider ambiguous
- session-scoped data commands auto-resolve unambiguous UUID prefixes and reject ambiguous prefixes with `ambiguous_session`
- `darc list files <glob>` / `darc list files --path <glob>` and `darc list sessions --touching <glob>` currently use the Rust `glob` crate syntax, matched
  case-insensitively against one canonical project-scoped display path per access
- absolute query paths under the configured project root are normalized down to project-relative form before matching, so `/repo/README.md` and `README.md` hit the same indexed access
- out-of-project paths are not exposed and do not participate in these path-matching filters
- turn rows include `user_prompt_preview`, `user_prompt_preview_chars`, `user_prompt_total_chars`, `agent_answer_preview`, `agent_answer_preview_chars`, `agent_answer_total_chars`, `primary_model`, `total_token_count`, `token_usage`, `effective_agent_runtime_ms`, `changed_file_count`, `added_line_count`, and `removed_line_count`
- in `view=full`, turn previews are capped at 500 normalized characters; in `view=oneline`, previews are first-line summaries capped at 300 normalized characters
- preview truncation is represented by the returned and total character counts; a preview was truncated when `*_preview_chars` is lower than `*_total_chars`
- `primary_model`, `total_token_count`, `token_usage`, and `effective_agent_runtime_ms` may be `null` when the archived provider transcript did not report stable values, or until older projects are re-indexed after additive schema upgrades

### File pivots

`darc.query.files.v1` and `darc.query.session_files.v1` report read-only file-to-session pivots derived from `file_accesses`.

Today:

- `darc.query.files.v1` includes `project_id`, `mode`, nullable `provider`, nullable `path`, nullable `co_touched_with`, nullable `since`, nullable `until`, non-null `limit`, non-null `offset`, non-null `has_more`, nullable `matched_path_limit`, plus `sessions` and `files` arrays
- `mode=top` is selected when no positional path, `--path`, or `--co-touched-with` is supplied; it populates `files` and leaves `sessions` empty
- `mode=path` populates `sessions` and leaves `files` empty
- `mode=co_touched_with` populates `files` and leaves `sessions` empty
- `mode=top` applies `--since` and `--until` to touched turns using `turns.started_at`, with inclusive lower-bound and exclusive upper-bound semantics
- `mode=top` ranks file rows by higher `touch_count`, then higher `session_count`, then newer `last_touched_at`, then `path` ascending
- `mode=top` applies `--limit` and `--offset` after ranking the project-wide touched files
- `mode=top` file rows report `path`, `touch_count`, `session_count`, `read_count`, `write_count`, `first_touched_at`, and `last_touched_at`; co-touch-only fields are omitted
- `mode=path` applies `--since` and `--until` to touched turns using `turns.started_at`, with inclusive lower-bound and exclusive upper-bound semantics
- `mode=path` ranks session rows by higher `touch_count`, then newer `last_touched_at`, then `provider`, then `session_id`
- `mode=path` applies `--limit` and `--offset` after ranking the matching sessions
- `mode=path` session rows report `provider`, `session_id`, `touch_count`, `read_count`, `write_count`, `first_turn_ordinal`, `last_turn_ordinal`, `first_touched_at`, `last_touched_at`, deterministic `matched_paths`, and `matched_paths_truncated`
- `matched_paths` is the canonical matched file preview for that session, ordered by display path ascending
- `matched_paths_truncated=true` means additional matched paths were omitted from the preview; pass `--matched-path-limit <n>` to raise the cap or `--include-all-matched-paths` to remove it
- `darc list files` path mode currently excludes derived `list` accesses, and obvious directory-only operands are
  omitted during extraction, so directory listings, search roots, and `mkdir`-style directory writes do not count as
  file touches
- `mode=co_touched_with` treats the seed path as one exact canonical display path, normalizing project-root absolute paths down to project-relative form when possible
- `mode=co_touched_with` only considers project-scoped in-repo file identities and does not expose or rank external absolute paths
- `mode=co_touched_with` applies `--since` and `--until` to both seed-path matches and returned co-touch rows using `turns.started_at`, with inclusive lower-bound and exclusive upper-bound semantics
- `mode=co_touched_with` ranks file rows by higher `co_touch_count`, then `path` ascending
- `mode=co_touched_with` applies `--limit` and `--offset` after ranking the co-touched files
- `mode=co_touched_with` file rows report `path` and `co_touch_count`; top-mode metrics are omitted
- `darc.query.session_files.v1` reports `project_id`, `provider`, `session_id`, total `file_count`, `limit`, `offset`, `has_more`, and deterministic paginated `files`
- `session_files` rows report canonical `path`, best-effort `repo_relative_path`, `read_count`, `write_count`, `first_turn_ordinal`, and `last_turn_ordinal`
- `session_files` rows collapse equivalent absolute, repo-relative, and `./`-prefixed accesses for the same in-repo file onto one canonical display path before counting
- `session_files` rows omit out-of-project accesses, exclude derived `list` accesses, and omit directory-only operands that Darc filtered during extraction
- `darc list sessions --touching <glob>` reuses the same project-scoped glob semantics as the file-pivot surfaces

### Session bundles

`darc.query.session_bundle.v1` is the preferred single-round-trip protocol when a client needs one session summary, its turn detail, and its in-project file touches together.

Today:

- the top-level payload echoes `project_id`, `provider`, `session_id`, `session_view`, and `view`
- `session` reuses the exact `darc.query.sessions.v1` session row shape
- `turns` reuses the exact `darc.query.turn.v1` turn-detail row shape without wrapping each row in its own envelope
- `turn_limit`, `turn_offset`, and `turns_has_more` describe the embedded turn-detail page
- `step_limit` and `step_offset` describe the step page applied to each embedded turn detail
- `session_view=compact` is the default and caps the embedded `session.first_user_prompt` and `session.final_agent_message` at 500 characters; `session_view=full` keeps both complete fields
- `session_file_limit` is currently `100`; `session_file_count` is the total session-file row count before the embedded cap, and `session_files_has_more=true` means more session file rows existed than the embedded preview returned
- `session_files` reuses the `darc.query.session_files.v1` payload shape with `limit=session_file_limit` and `offset=0`
- `view=narrative` applies the same step projection rules as `darc show turn --view narrative`
- `view=full` keeps the full normalized turn-step payload with `raw_steps_json` still forced to `null`

### Session resolution

`darc.query.resolve_session.v1` is the explicit UUID-prefix expansion protocol for humans and scripts. Session-scoped
read commands already resolve unambiguous prefixes; use this payload when a prefix is ambiguous or when a caller wants
the candidate list before choosing.

Today:

- `query` echoes the supplied full UUID or prefix exactly as resolved by the CLI
- without `--pick-one`, the payload includes deterministic `matches`, `total`, and `truncated` fields
- `matches[*]` rows report `project_id`, `provider`, and canonical `session_id`
- matches are ordered by `project_id` ascending, then `provider` ascending, then `session_id` ascending
- `total` is the true number of matching sessions before the fixed response cap is applied
- results are capped to a generous fixed page and set `truncated=true` when more candidates exist than returned `matches`
- with `--pick-one`, the success payload uses one top-level `match` object for convenience
- a full UUID that does not exist returns `unknown_session`
- `--pick-one` returns `unknown_session` for zero matches and `ambiguous_session` for multiple matches

### Narrative turn detail

`darc show turn` defaults to `--view narrative`, which keeps the same `darc.query.turn.v1` schema but projects each step down to the conversational structure without the bulky tool arguments, tool outputs, or raw payload blobs.

Today:

- the payload includes `step_count` for the full indexed step count plus non-null `step_limit`, `step_offset`, and `steps_has_more` pagination fields for the returned `steps` page
- `reasoning` and `commentary` steps keep their full fields
- `tool_call` keeps `timestamp`, `call_id`, and `name`, but clears `arguments`
- `tool_call_output` keeps `timestamp` and `call_id`, but clears `output`
- `attachment`, `delegation`, `hook_summary`, and `provider_response_item` keep their identifying metadata, but clear `payload_json`
- `raw_steps_json` is forced to `null` in narrative view; explicit `--view narrative --include-raw` is rejected, while omitted `--view` plus `--include-raw` implies `--view full`

### Turn search

`darc.query.search.turns.v1` reports paginated turn hits for one project-scoped search.

Today:

- `mode=keyword` uses SQLite FTS5 over Darc-owned derived per-turn search text
- keyword search currently indexes `user_message`, `final_answer_text`, and selected derived step text such as commentary, tool names, and delegation summaries
- keyword search does not currently index raw tool outputs or raw provider payload blobs
- `mode=literal` treats the query text as exact plain text and matches it against derived `turn_evidence` rows
- `mode=regex` treats the query text as a Rust regular expression with Perl character classes such as `\s` enabled and matches it against the same derived `turn_evidence` rows
- literal and regex search exclude `tool_output` evidence by default because command and tool output is often large and noisy for context-building
- pass `--include-tool-output` with literal or regex search to include command/tool output evidence for forensic searches such as exact errors, stack traces, logs, or command output
- `--include-tool-output` is rejected for `keyword`, `file_name`, `file_path`, and `path_fragment` search because those modes do not inspect `turn_evidence.tool_output`
- pass repeatable `--field` values to restrict literal and regex search to specific evidence fields, or repeatable `--exclude-field` values to omit fields from the default exact-search evidence set
- accepted exact-search field names are `user-message`, `final-answer`, `commentary`, `reasoning-summary`, `tool-name`, `tool-arguments`, `tool-output`, `delegation-summary`, `delegation-metadata`, `hook-summary`, `attachment-metadata`, and `provider-response-item-metadata`; the stable snake_case labels are also accepted
- literal and regex search inspect `user_message`, `final_answer`, `commentary`, `reasoning_summary`, `tool_name`,
  `tool_arguments`, `delegation_summary`, `delegation_metadata`, `hook_summary`,
  `attachment_metadata`, and `provider_response_item_metadata` evidence fields
- with `--include-tool-output`, literal and regex search also inspect `tool_output`
- metadata evidence rows are compact canonical metadata, not raw provider payload blobs
- literal and regex search apply project, provider, session, `--since`, and `--until` filters in SQLite, then scan matching turns in result order
- literal search uses SQLite exact substring predicates to discard nonmatching evidence rows before Darc builds match previews
- regex search scans derived evidence rows in process because SQLite does not evaluate Darc's Rust regular expressions
- literal and regex search stop after finding `offset + limit + 1` matching turn hits or after exhausting the filtered turn corpus, so rare or absent exact queries may scan the full filtered project scope
- literal and regex search return turn hits with nested `matches` entries containing `evidence_ordinal`, `field`, and a bounded `snippet`
- each literal or regex turn hit returns at most `match_limit` nested `matches`, defaulting to 20; `matches_count` is the returned nested match count, and `matches_truncated=true` means additional matching evidence rows in that turn were omitted from the preview
- literal and regex search are not content-index backed; narrow provider, session, or time filters for broad audits when latency matters
- `mode=file_name` searches the derived `file_accesses.file_name` basename field
- `mode=file_path` treats the query text as the same case-insensitive project-scoped glob shape used by
  `darc list files`
- `mode=path_fragment` searches derived path fields from `file_accesses.repo_relative_path` and `file_accesses.path` with exact/prefix/substring ranking
- all search modes return turn identities, top-level turn metadata, `user_prompt_preview`, `user_prompt_preview_chars`, `user_prompt_total_chars`, nullable `agent_answer_preview`, nullable `agent_answer_preview_chars`, nullable `agent_answer_total_chars`, nullable `since` / `until` request echoes, nullable `matched_path_limit`, nullable `match_limit`, `include_tool_output`, `fields`, `excluded_fields`, and optional `snippet` / `matched_paths` / `matches` fields plus `matched_paths_count`, `matched_paths_truncated`, `matches_count`, and `matches_truncated`
- `matched_paths` is empty for keyword search and populated for file-name, file-path, or path-fragment hits
- `matched_paths_count` is the total matched path count collected for that hit before `matched_path_limit`; `matched_paths_truncated=true` means additional file-search paths were omitted from that hit's preview; pass `--matched-path-limit <n>` to raise the cap or `--include-all-matched-paths` to remove it
- `matches` is empty for keyword and file search and populated for literal or regex hits
- `matches_truncated` is always false for keyword and file search; `matched_paths_truncated` is always false for keyword, literal, and regex search
- file-name and path-fragment search use case-insensitive exact/prefix/substring ranking and deduplicate turn hits before applying final pagination
- keyword search currently uses FTS ranking before recency tie-breaks

### Project insights

`darc.query.insights.project.v1` echoes nullable `provider`; when present, `daily_time`, tool/file rankings, failures, and total time are computed from that provider's recent turns only.

`turn_limit` echoes the requested inspection bound, `inspected_turn_count` is the number of turns actually included in the aggregate, and `turns_has_more=true` means older matching turns existed beyond the inspected page.

Project insight `most_read_files[*].path` and `most_written_files[*].path` use the same canonical display-path contract as turn insight `files[*].path`: in-repo absolute paths are normalized to project-scoped relative paths when the configured project root is available, equivalent in-repo path forms are merged before counting, and external paths fall back to the stored extracted path.

`darc.query.insights.workspace.v1` keeps `active_session_count` as the total active session count in the window, while `recent_sessions` is a bounded preview described by `recent_session_limit`, `recent_session_offset`, and `recent_sessions_has_more`.

## Raw and debug fields

Raw/debug payload fields are optional and command-specific.

Today:

- `darc show turn --include-raw` includes `raw_steps_json` and implies `--view full` when no explicit `--view` is supplied
- `darc show turn --include-insights` includes `insights`
- without `--include-raw`, `raw_steps_json` is currently still present in the response and set to `null`
- without `--include-insights`, `insights` is currently still present in the response and set to `null`

## Insights day semantics

Insights payloads use host-local civil days, not UTC days.

- `daily_time[*].date` is the local calendar day for the machine running `darc`
- `window_start` and `window_end` are local calendar days
- timestamp fields such as `started_at` and `latest_turn_at` remain UTC ISO 8601 strings

These local-day semantics are part of the current development-phase `v1` insights contract.

Clients should avoid depending on raw/debug fields unless they explicitly request them.
