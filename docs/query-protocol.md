# Darc Query Protocol

`darc query` is the machine-readable read protocol for desktop and other clients.

Use it instead of:

- opening `index.sqlite` directly
- parsing human-oriented command output
- deriving analytics from raw `steps_json` outside `darc`

## Commands

Query commands emit JSON envelopes on stdout by default.

### Workspace

- `darc query workspace [--root <path>]`

### Sessions, Turns, And Files

- `darc query resolve-session <uuid-or-prefix> [--root <path>] [--project-id <id>] [--provider <provider>] [--pick-one]`
- `darc query sessions [--root <path>] [--project-id <id>] [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--touched-path <glob>] [--limit <n>] [--offset <n>]`
- `darc query files [--root <path>] [--project-id <id>] <path-or-glob> [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] [--offset <n>]`
- `darc query files [--root <path>] [--project-id <id>] --path <path-or-glob> [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] [--offset <n>]`
- `darc query files [--root <path>] [--project-id <id>] --co-touched-with <path> [--limit <n>] [--offset <n>]`
- `darc query session-files [--root <path>] [--project-id <id>] [--provider <provider>] <session-id>`
- `darc query session-files [--root <path>] [--project-id <id>] [--provider <provider>] --session-id <session-id>`
- `darc query session-bundle [--root <path>] [--project-id <id>] [--provider <provider>] <session-id> [--view <full|narrative>] [--turn-limit <n>] [--turn-offset <n>]`
- `darc query turns [--root <path>] [--project-id <id>] [--provider <provider>] <session-id> [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--view <full|oneline>] [--limit <n>] [--offset <n>]`
- `darc query turn [--root <path>] [--project-id <id>] [--provider <provider>] <session-id> <turn-ordinal> [--view <full|narrative>] [--include-raw] [--include-insights]`

### Search

- `darc query search turns [--root <path>] [--project-id <id>] <query> [--mode <keyword|literal|regex|file-name|file-path|path-fragment>] [--include-tool-output] [--provider <provider>] [--session-id <id>] [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] [--offset <n>]`
- `darc query search turns [--root <path>] [--project-id <id>] --query <query> [--mode <keyword|literal|regex|file-name|file-path|path-fragment>] [--include-tool-output] [--provider <provider>] [--session-id <id>] [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] [--offset <n>]`

### Insights

- `darc query insights workspace [--root <path>] [--window <days>d]`
- `darc query insights project [--root <path>] [--project-id <id>] [--turn-limit <n>]`
- `darc query insights turn [--root <path>] [--project-id <id>] [--provider <provider>] <session-id> <turn-ordinal>`
- `darc query insights turn [--root <path>] [--project-id <id>] [--provider <provider>] --session-id <session-id> --turn-ordinal <n>`

## Argument rules

- project-scoped queries accept optional `--project-id`; when omitted, Darc resolves the configured project from the current directory
- project-wide provider filters default to all providers when `--provider` is omitted
- `darc query resolve-session` accepts either one full UUID or one UUID prefix and returns `project_id`, `provider`, and `session_id` for each match
- `darc query search turns` defaults to `--mode keyword`; pass `--mode` only for literal, regex, or file/path search modes
- `darc query search turns` accepts query text positionally or with `--query`; use `--query` for query text that begins with `-`
- `darc query files` treats positional `<path-or-glob>` as path mode; `--path` is the explicit equivalent
- session-scoped commands accept `<session-id>` positionally or with `--session-id`; Darc infers `--provider` when that session id is unique within the project
- turn-scoped commands accept `<turn-ordinal>` positionally or with `--turn-ordinal`
- do not pass both positional and flag forms for the same value
- pass `--provider` when the same session id exists for multiple providers
- `darc query files` requires exactly one of positional path, `--path`, or `--co-touched-with`
- `--since` and `--until` on `darc query files` require path mode
- `--limit` and `--offset` are accepted by `darc query sessions`, `darc query turns`, `darc query search turns`, and both `darc query files` modes; these row/turn-hit limits default to `--limit 50 --offset 0`
- `--turn-limit` and `--turn-offset` on `darc query session-bundle` bound embedded turn details and default to `--turn-limit 50 --turn-offset 0`
- `darc query turn` and `darc query session-bundle` default to `--view narrative`; pass `--view full` when raw tool arguments, outputs, or payload blobs are needed
- `--turn-limit` on `darc query insights project` is an inspection bound over indexed turns, not response pagination; the previous `--limit` spelling is accepted as a compatibility alias
- `--include-tool-output` on `darc query search turns` is accepted only with `--mode literal` or `--mode regex`
- session-scoped data commands require a full UUID session id; malformed ids return `invalid_session_id`, unknown UUIDs return `unknown_session`, ambiguous cross-provider UUIDs return `ambiguous_session`, and UUID-like prefixes fail explicitly instead of auto-resolving

## Common Workflows

The protocol is intentionally composable. A few common read patterns are now first-class:

- find planning turns by content:

  ```bash
  darc query search turns \
    --root ~/.darc \
    --project-id repo-abc123 \
    "staged init" \
    --since 14d
  ```

- verify exact evidence text without regex escaping; literal and regex searches skip bulky tool outputs by default:

  ```bash
  darc query search turns \
    --root ~/.darc \
    --project-id repo-abc123 \
    --mode literal \
    --query "--output-last-message"
  ```

- search command output or logs explicitly for forensic work:

  ```bash
  darc query search turns \
    --root ~/.darc \
    --project-id repo-abc123 \
    --mode regex \
    "panic: .*" \
    --include-tool-output
  ```

- pivot from a file path to the sessions that touched it:

  ```bash
  darc query files \
    --root ~/.darc \
    --project-id repo-abc123 \
    "src/components/planner.rs" \
    --limit 20
  ```

- inspect all in-project files touched by one session:

  ```bash
  darc query session-files \
    --root ~/.darc \
    --project-id repo-abc123 \
    11111111-1111-4111-8111-111111111111
  ```

- fetch one session summary, narrative turn detail, and touched files in one call:

  ```bash
  ID=$(darc query resolve-session 11111111 --pick-one | jq -r '.data.match.session_id')

  darc query session-bundle \
    --root ~/.darc \
    --project-id repo-abc123 \
    "$ID" \
    --turn-limit 20
  ```

- skim one long session as one compact row per turn:

  ```bash
  darc query turns \
    --root ~/.darc \
    --project-id repo-abc123 \
    11111111-1111-4111-8111-111111111111 \
    --view oneline \
    --limit 50
  ```

## Success envelope

Query success responses are written to `stdout` only.

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

Query failures return non-zero exit status and write a structured error envelope to `stderr`.

```json
{
  "schema": "darc.error.v1",
  "generated_at": "2026-04-06T12:00:00Z",
  "darc_version": "0.1.0",
  "error": {
    "code": "unknown_session",
    "message": "No session found for id `11111111`. The session id must be the full UUID. Try `darc query resolve-session 11111111` to expand a prefix.",
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

Session-id-specific error codes:

- `invalid_session_id`: the supplied resolver query or data-command session id is not a full UUID or accepted UUID-prefix shape
- `unknown_session`: the full UUID or explicit prefix did not resolve to an indexed session
- `ambiguous_session`: `darc query resolve-session --pick-one` found more than one candidate, or a session-scoped data command found the same full UUID under multiple providers; pass `--provider` to choose one provider

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
- `darc.error.v1`

Clients should branch on `schema`, not on `darc_version`.

`darc query turns` supports two projections:

- `view: "full"` keeps the existing turn-summary shape and now also includes `tool_call_count`
- `view: "oneline"` returns a smaller per-turn object with `turn_ordinal`, `role`, `user_preview`, `step_count`, and `tool_call_count`
- both projections include `limit`, `offset`, and `has_more`, and default to the first 50 turns
- `oneline.user_preview` is derived from the first `user_message` line and capped at 80 characters
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

Query payloads follow these rules:

- snake_case field names
- lowercase stable enum values
- UTC ISO 8601 timestamps
- explicit `null` for nullable values
- empty arrays instead of omitted list fields
- deterministic ordering where practical

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
- this layer is best effort, not a perfect trace: archived rollouts record tool payloads and command text, not syscall-level file I/O, so commands such as `git`, `cargo`, inline Python, shell loops, subshells, or helper scripts may touch files without naming every path explicitly
- paths are currently reported as the extracted path string after Darc drops obvious directory-only operands such as `ls crates`, `find crates ...`, `rg foo crates`, or `mkdir -p scratch/cache`
- `repo_relative_path` is included on file-usage rows when the indexed access already carried a repo-relative label; otherwise it is `null`

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
- `files` comes from normalized per-turn `file_accesses` rows, grouped by `path`, after obvious directory-only operands are filtered during extraction
- `files[*].read_count` currently counts both `read` and `list` access kinds
- `files[*].write_count` currently counts both `write` and `edit` access kinds
- `tools` is ordered by higher `count` first, then `name` ascending
- `shell_commands` is ordered by tool call order within the turn
- `files` is ordered by higher total accesses first, then higher `write_count`, then higher `read_count`, then `path` ascending

Clients should treat these analytics as Darc-owned derived data and should not re-derive them from `steps_json`.

### Combined turn queries

`darc query turn --include-insights` embeds one derived `insights` block inside `darc.query.turn.v1`.

Today:

- the top-level turn detail fields remain unchanged
- `insights` includes `primary_model`, `duration_ms`, `effective_agent_runtime_ms`, `total_token_count`, `token_usage`, `changed_file_count`, `added_line_count`, `removed_line_count`, `tool_call_count`, `tool_output_count`, `attachment_count`, `delegation_count`, `hook_summary_count`, `has_final_answer`, `tools`, and `files`
- the embedded `insights.tools` and `insights.files` arrays follow the same derivation and ordering rules as `darc.query.insights.turn.v1`
- this command is the preferred single-round-trip protocol when a client needs both turn detail and turn analytics together

### Session and turn lists

`darc.query.sessions.v1` and `darc.query.turns.v1` surface the best-effort model, token, runtime, and observed patch-count fields needed for lightweight desktop list views.

Today:

- session rows include `primary_model`, `total_token_count`, `token_usage`, `effective_agent_runtime_ms`, `changed_file_count`, `added_line_count`, `removed_line_count`, `first_turn_at`, `first_user_prompt`, `aborted_turn_count`, and `edited_files`
- session totals are rollups across the indexed turns in that session
- top-level session-list payloads additionally echo the resolved `since`, `until`, and `touched_path` request filters as nullable fields, plus non-null `limit`, `offset`, and `has_more` pagination fields
- top-level turn-list payloads echo nullable `since` and `until` filters plus non-null `limit`, `offset`, and `has_more` pagination fields
- optional `--since` and `--until` filters apply to `latest_turn_at`, using inclusive lower-bound and exclusive upper-bound semantics
- optional `--touched-path` requires at least one session-scoped, project-scoped file access of any access type whose canonical display path matches the provided glob; Darc scans session candidates in `latest_turn_at` order after the `--since` / `--until` bounds and then applies touched-path pagination
- `--since` and `--until` accept absolute ISO-8601 text or relative `<days>d` shorthand such as `5d`
- each `token_usage.*` session field is `null` unless every indexed turn in that session carried a value for that exact field
- `total_token_count` and `effective_agent_runtime_ms` are currently `null` on a session row unless every indexed turn in that session carried a value for that field
- `first_turn_at` and `first_user_prompt` come from the indexed turn with the minimum `turn_ordinal` in that session and are `null` only when the indexed session has no stored turns
- `aborted_turn_count` counts indexed turns in that session where `status` is `aborted`
- `edited_files` is the distinct `COALESCE(repo_relative_path, path)` list from session-scoped `file_accesses` rows with `access_type` of `edit` or `write`, excluding null or whitespace-only paths and ordered by display path ascending
- `darc.query.turns.v1` remains session-scoped and keeps non-null top-level `provider` and `session_id`; provider is inferred unless the session id is cross-provider ambiguous
- session-scoped data commands do not auto-resolve UUID prefixes; callers must expand prefixes explicitly with `darc query resolve-session`
- `query files <glob>` / `query files --path <glob>` and `--touched-path <glob>` on `query sessions` currently use the Rust `glob` crate syntax, matched case-insensitively against one canonical project-scoped display path per access
- absolute query paths under the configured project root are normalized down to project-relative form before matching, so `/repo/README.md` and `README.md` hit the same indexed access
- out-of-project paths are not exposed and do not participate in these path-matching filters
- turn rows include `primary_model`, `total_token_count`, `token_usage`, `effective_agent_runtime_ms`, `changed_file_count`, `added_line_count`, and `removed_line_count`
- `primary_model`, `total_token_count`, `token_usage`, and `effective_agent_runtime_ms` may be `null` when the archived provider transcript did not report stable values, or until older projects are re-indexed after additive schema upgrades

### File pivots

`darc.query.files.v1` and `darc.query.session_files.v1` report read-only file-to-session pivots derived from `file_accesses`.

Today:

- `darc.query.files.v1` includes `project_id`, `mode`, nullable `path`, nullable `co_touched_with`, nullable `since`, nullable `until`, non-null `limit`, non-null `offset`, non-null `has_more`, plus `sessions` and `files` arrays
- `mode=path` populates `sessions` and leaves `files` empty
- `mode=co_touched_with` populates `files` and leaves `sessions` empty
- `mode=path` applies `--since` and `--until` to touched turns using `turns.started_at`, with inclusive lower-bound and exclusive upper-bound semantics
- `mode=path` ranks session rows by higher `touch_count`, then newer `last_touched_at`, then `provider`, then `session_id`
- `mode=path` applies `--limit` and `--offset` after ranking the matching sessions
- `mode=path` session rows report `provider`, `session_id`, `touch_count`, `read_count`, `write_count`, `first_turn_ordinal`, `last_turn_ordinal`, `first_touched_at`, `last_touched_at`, and deterministic `matched_paths`
- `matched_paths` is the canonical matched file list for that session, ordered by display path ascending
- `query files` path mode currently excludes derived `list` accesses, and obvious directory-only operands are omitted during extraction, so directory listings, search roots, and `mkdir`-style directory writes do not count as file touches
- `mode=co_touched_with` treats the seed path as one exact canonical display path, normalizing project-root absolute paths down to project-relative form when possible
- `mode=co_touched_with` only considers project-scoped in-repo file identities and does not expose or rank external absolute paths
- `mode=co_touched_with` ranks file rows by higher `co_touch_count`, then `path` ascending
- `mode=co_touched_with` applies `--limit` and `--offset` after ranking the co-touched files
- `mode=co_touched_with` file rows report `path` plus the number of distinct sessions that touched both that file and the seed file
- `darc.query.session_files.v1` reports `project_id`, `provider`, `session_id`, and deterministic `files`
- `session_files` rows report canonical `path`, best-effort `repo_relative_path`, `read_count`, `write_count`, `first_turn_ordinal`, and `last_turn_ordinal`
- `session_files` rows collapse equivalent absolute, repo-relative, and `./`-prefixed accesses for the same in-repo file onto one canonical display path before counting
- `session_files` rows omit out-of-project accesses, exclude derived `list` accesses, and omit directory-only operands that Darc filtered during extraction
- `query sessions --touched-path <glob>` reuses the same project-scoped glob semantics as the file-pivot surfaces

### Session bundles

`darc.query.session_bundle.v1` is the preferred single-round-trip protocol when a client needs one session summary, its turn detail, and its in-project file touches together.

Today:

- the top-level payload echoes `project_id`, `provider`, `session_id`, and `view`
- `session` reuses the exact `darc.query.sessions.v1` session row shape
- `turns` reuses the exact `darc.query.turn.v1` turn-detail row shape without wrapping each row in its own envelope
- `turn_limit`, `turn_offset`, and `turns_has_more` describe the embedded turn-detail page
- `session_files` reuses the exact `darc.query.session_files.v1` payload shape
- `view=narrative` applies the same step projection rules as `darc query turn --view narrative`
- `view=full` keeps the full normalized turn-step payload with `raw_steps_json` still forced to `null`

### Session resolution

`darc.query.resolve_session.v1` is the explicit UUID-prefix expansion protocol for humans and scripts.

Today:

- `query` echoes the supplied full UUID or prefix exactly as resolved by the CLI
- without `--pick-one`, the payload includes deterministic `matches`, `total`, and `truncated` fields
- `matches[*]` rows report `project_id`, `provider`, and canonical `session_id`
- matches are ordered by `project_id` ascending, then `provider` ascending, then `session_id` ascending
- results are capped to a generous fixed page and set `truncated=true` when more candidates exist
- with `--pick-one`, the success payload uses one top-level `match` object for convenience
- a full UUID that does not exist returns `unknown_session`
- `--pick-one` returns `unknown_session` for zero matches and `ambiguous_session` for multiple matches

### Narrative turn detail

`darc query turn` defaults to `--view narrative`, which keeps the same `darc.query.turn.v1` schema but projects each step down to the conversational structure without the bulky tool arguments, tool outputs, or raw payload blobs.

Today:

- `reasoning` and `commentary` steps keep their full fields
- `tool_call` keeps `timestamp`, `call_id`, and `name`, but clears `arguments`
- `tool_call_output` keeps `timestamp` and `call_id`, but clears `output`
- `attachment`, `delegation`, `hook_summary`, and `provider_response_item` keep their identifying metadata, but clear `payload_json`
- `raw_steps_json` is forced to `null` in narrative view even when `--include-raw` is set

### Turn search

`darc.query.search.turns.v1` reports paginated turn hits for one project-scoped search.

Today:

- `mode=keyword` uses SQLite FTS5 over Darc-owned derived per-turn search text
- keyword search currently indexes `user_message`, `final_answer_text`, and selected derived step text such as commentary, tool names, and delegation summaries
- keyword search does not currently index raw tool outputs or raw provider payload blobs
- `mode=literal` treats the query text as exact plain text and matches it against derived `turn_evidence` rows
- `mode=regex` treats the query text as a Rust regular expression and matches it against the same derived `turn_evidence` rows
- literal and regex search exclude `tool_output` evidence by default because command and tool output is often large and noisy for context-building
- pass `--include-tool-output` with literal or regex search to include command/tool output evidence for forensic searches such as exact errors, stack traces, logs, or command output
- `--include-tool-output` is rejected for `keyword`, `file_name`, `file_path`, and `path_fragment` search because those modes do not inspect `turn_evidence.tool_output`
- literal and regex search inspect `user_message`, `final_answer`, `commentary`, `reasoning_summary`, `tool_name`,
  `tool_arguments`, `delegation_summary`, `delegation_metadata`, `hook_summary`,
  `attachment_metadata`, and `provider_response_item_metadata` evidence fields
- with `--include-tool-output`, literal and regex search also inspect `tool_output`
- metadata evidence rows are compact canonical metadata, not raw provider payload blobs
- literal and regex search apply project, provider, session, `--since`, and `--until` filters in SQLite, then scan matching turns in result order
- literal search uses SQLite exact substring predicates to discard nonmatching evidence rows before Darc builds match previews
- regex search scans derived evidence rows in process because SQLite does not evaluate Darc's Rust regular expressions
- literal and regex search stop after finding `offset + limit + 1` matching turn hits or after exhausting the filtered turn corpus, so rare or absent exact queries may scan the full filtered project scope
- literal and regex search return turn hits with nested `matches` entries containing `field` and a bounded `snippet`
- each literal or regex turn hit returns at most 20 nested `matches`; `matches_truncated=true` means additional matching evidence rows in that turn were omitted from the preview
- literal and regex search are not content-index backed; narrow provider, session, or time filters for broad audits when latency matters
- `mode=file_name` searches the derived `file_accesses.file_name` basename field
- `mode=file_path` treats the query text as the same case-insensitive project-scoped glob shape used by `darc query files`
- `mode=path_fragment` searches derived path fields from `file_accesses.repo_relative_path` and `file_accesses.path` with exact/prefix/substring ranking
- all search modes return turn identities, top-level turn metadata, nullable `since` / `until` request echoes, `include_tool_output`, and optional `snippet` / `matched_paths` / `matches` fields plus `matches_truncated`
- `matched_paths` is empty for keyword search and populated for file-name, file-path, or path-fragment hits
- `matches` is empty for keyword and file search and populated for literal or regex hits
- `matches_truncated` is always false for keyword and file search
- file-name and path-fragment search use case-insensitive exact/prefix/substring ranking and deduplicate turn hits before applying final pagination
- keyword search currently uses FTS ranking before recency tie-breaks

### Hard debugging

`hard_debuggings` is currently provisional.

Today it is ranked by:

- higher `step_count` first
- then higher `duration_ms`
- then stable identity tie-breaks

This should be treated as a temporary ranking policy until Darc adopts a more explicit debugging score.

## Raw and debug fields

Raw/debug payload fields are optional and command-specific.

Today:

- `darc query turn --view full --include-raw` includes `raw_steps_json`
- `darc query turn --include-insights` includes `insights`
- without `--include-raw`, `raw_steps_json` is currently still present in the response and set to `null`
- without `--include-insights`, `insights` is currently still present in the response and set to `null`

## Insights day semantics

Insights payloads use host-local civil days, not UTC days.

- `daily_time[*].date` is the local calendar day for the machine running `darc`
- `window_start` and `window_end` are local calendar days
- timestamp fields such as `started_at` and `latest_turn_at` remain UTC ISO 8601 strings

These local-day semantics are part of the current development-phase `v1` insights contract.

Clients should avoid depending on raw/debug fields unless they explicitly request them.
