# TODO

This file tracks work that is still open. Remove solved items instead of leaving historical plans
behind, and keep code references aligned with the current crate split.

Current crate ownership:

- `crates/rollout`: Codex and Claude transcript parsing plus schema and version logic.
- `crates/index`: archive ingestion, duplicate resolution, and write-side indexing flow.
- `crates/store`: SQLite schema, migrations, storage helpers, and derived-index analytics.
- `crates/query`: read-side query, search, and insights over the shared store.
- `crates/sync`: Claude and Codex discovery plus archive copy planning.
- `crates/core`: facade and orchestration glue.

## Active Backlog

### Sync and project identity

#### Add durable evidence for deleted or moved checkouts

Current state:

- Sync now matches Codex sessions by logged `git.repository_url`, by explicit linked/known paths, or by current-project
  paths when metadata is missing, without probing arbitrary historical `cwd` directories.
- Additional checkouts must be explicitly registered or linked before path-only legacy logs can be associated with the
  active project; broad parent-prefix matches do not override a mismatched logged remote.
- Broad linked-path matches with logged remotes require scoped remote evidence for that linked path; missing old paths
  without stored/live upstream evidence are skipped instead of being trusted by prefix alone.

Needed:

- Persist richer last-seen checkout evidence such as observed `cwd`, resolved repo root, remote
  origin, and last-seen time.
- Only treat historical rollout paths as the same project when backed by that evidence.
- When evidence is missing, surface old paths as low-confidence candidates instead of silently
  promoting them into `known_paths`.

### Indexing and parsing

#### Reduce Codex rollout parsing to one streaming pass

Current state:

- Codex parsing lives in `crates/rollout/src/codex/parser.rs`.
- The current path reads the header, scans the file again for event-based user boundaries, and then
  parses the full stream.

Needed:

- Derive user-turn boundary strategy during the main streaming parse.
- Keep parser memory bounded to the current turn.
- Leave the higher-level incremental indexing flow unchanged.

#### Strengthen unchanged-rollout skip identity

Current state:

- Reindex skip detection in `crates/index/src/engine.rs` trusts `archive_path`, `source_size`, and
  `source_mtime_ms`.

Needed:

- Add a stronger content identity, such as a hash or equivalent stable fingerprint, so replaced or
  corrupted rollout contents cannot be mistaken for an unchanged archive copy.

### Search

#### Decide what to do about slow substring file search

Current state:

- Exact and prefix file-name and path-fragment search already use the staged indexed path.
- The final contains fallback still uses `LIKE '%...%'`.

Needed:

- Either keep substring search as a documented best-effort slow path, remove it, or add a dedicated
  substring side index.

#### Make the text-indexing contract explicit

Current state:

- `turn_search` indexes user message, final answer, commentary text, tool names, and delegation
  summaries.
- Keyword search intentionally omits raw tool outputs and most argument payloads.
- Exact literal/regex search uses the separate derived `turn_evidence` rows for user messages, final answers,
  commentary, plaintext reasoning summaries, tool names, tool arguments, tool outputs, delegation summaries, and
  compact metadata for delegations, hooks, attachments, and provider response items.

Needed:

- Keep exact-search scale tests representative as larger local archives are indexed.

#### Add representative search-scale verification

Current state:

- Search has solid functional coverage, but not much large-history verification.

Needed:

- Add lightweight synthetic-history tests that can catch query-plan, ranking, or pagination
  regressions without turning into brittle performance benchmarks.

### Claude parser and audit follow-ups

#### Keep revalidating the Claude Code audit pipeline

Current state:

- The hidden `darc claude-schema-audit` workflow has been revalidated against the modern
  `@anthropic-ai/claude-code` native-wrapper package layout.
- Unit tests are not enough here because the real pipeline depends on live package fetch, local
  runtime setup, auth/environment handling, fixture execution, transcript capture, and final drift
  reporting.

Needed:

- Periodically audit the full pipeline end to end: package discovery/download, extraction, released CLI
  execution, host-auth setup, fixture workspace setup, transcript and hook capture, manifest
  derivation, schema diffing, and final report generation.
- Fix any broken stage or stages instead of patching symptoms in isolation.
- Add regression coverage for the real failure mode so the command is trustworthy outside the unit
  test harness.
- Re-run the full workflow locally after the fix and document any remaining prerequisites or
  caveats clearly.

#### Keep expanding fixture-backed Claude coverage

Current state:

- The hidden Claude schema audit already exists.
- Claude parsing already maps versions into `ClaudeSchemaEpoch`, but exact fixture-backed coverage
  is still narrow.

Needed:

- Refresh exact fixtures regularly.
- Tighten epoch boundaries when audit output shows real transcript drift.
- Update docs and tests at the same time as epoch changes.

#### Strengthen epoch-specific Claude parsing

Current state:

- Claude parsing already uses epoch-aware compatibility, but some turn-boundary and event handling
  still relies on heuristics or generic preserved payloads.

Needed:

- Move more epoch-specific behavior into explicit structured parsing.
- Keep safe fallback behavior for unknown variants instead of inventing brittle merges.

#### Improve Claude normalization and auxiliary-artifact usage

Current state:

- Some Claude provider events are still preserved generically.
- Auxiliary artifacts such as `.meta.json` are archived, but only partially surfaced.

Needed:

- Normalize more `system`, `progress`, `origin`, and related provider events into shared structured
  steps where it improves indexing.
- Improve tool-result reconstruction fidelity when archived artifacts can support it.
- Decide which auxiliary fields should be indexed directly, linked from turns, or kept archive-only.

#### Keep Claude parser surface area deliberate

Current state:

- The parser is still an internal implementation detail.

Needed:

- Only expose a standalone public Claude parser API if the contract is stable enough to support
  external callers.
- Add lightweight pre-index inspection when it materially improves early rejection of obviously bad
  or mismatched Claude archives.
