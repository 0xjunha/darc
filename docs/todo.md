# TODO

This file tracks work that is still open. Remove solved items instead of leaving historical plans
behind, and keep code references aligned with the current crate split.

Current crate ownership:

- `crates/rollout`: Codex and Claude transcript parsing plus schema and version logic.
- `crates/index`: archive ingestion, duplicate resolution, and SQLite indexing.
- `crates/query`: read-side query, search, and insights.
- `crates/sync`: Claude and Codex discovery plus archive copy planning.
- `crates/wiki`: canonical Context Wiki storage, validation, merge, and run artifacts.
- `crates/core`: facade and orchestration glue.

## Active Backlog

### Context Wiki workflow and policy

#### Separate extraction from registry curation

Current state:

- Proposal validation only accepts existing categories and domains.
- Extractors cannot propose taxonomy changes; registry edits still require manual or backend-owned
  changes.

Needed:

- Let extractors optionally propose category and domain additions in the proposal artifact.
- Keep canonical registry mutation in a separate curator step.
- Enforce slug validation, dedupe and similarity checks, and per-run proposal limits.
- Keep category promotion stricter than domain promotion.

#### Add Windows-capable stale-run liveness checks

Current state:

- `crates/core/src/wiki/state.rs` uses `kill(pid, 0)` on Unix.
- The non-Unix fallback returns `false`, so stale-run repair will treat worker pids as dead on
  Windows.

Needed:

- Implement a Windows process liveness path before claiming cross-platform stale-run parity.

### Sync and project identity

#### Add durable evidence for deleted or moved checkouts

Current state:

- Sync can already learn additional Codex repo roots into `known_paths` when live `cwd`, repo-root,
  and upstream matching says they belong to the active project.
- That works for live worktrees, but it stays weak once a checkout disappears.

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

- Exact and prefix file-name and file-path search already use the staged indexed path.
- The final contains fallback still uses `LIKE '%...%'`.

Needed:

- Either keep substring search as a documented best-effort slow path, remove it, or add a dedicated
  substring side index.

#### Make the text-indexing contract explicit

Current state:

- `turn_search` indexes user message, final answer, commentary text, tool names, and delegation
  summaries.
- It intentionally omits raw tool outputs and most argument payloads today.

Needed:

- Document that policy as part of the query and search contract.
- Decide whether any extra safe summaries should be indexed without reintroducing raw-output or
  raw-payload leakage.

#### Add representative search-scale verification

Current state:

- Search has solid functional coverage, but not much large-history verification.

Needed:

- Add lightweight synthetic-history tests that can catch query-plan, ranking, or pagination
  regressions without turning into brittle performance benchmarks.

### Claude parser and audit follow-ups

#### Deeply review and fix the Claude Code audit pipeline

Current state:

- The hidden `darc claude-schema-audit` workflow exists, but local manual runs are currently not
  trustworthy and should be treated as broken until revalidated end to end.
- Unit tests are not enough here because the real pipeline depends on live package fetch, local
  runtime setup, auth/environment handling, fixture execution, transcript capture, and final drift
  reporting.

Needed:

- Reproduce the local failure manually and record the exact failing stage, versions, and
  environment assumptions.
- Audit the full pipeline end to end: package discovery/download, extraction, released CLI
  execution, host-auth setup, fixture workspace setup, transcript and hook capture, manifest
  derivation, schema diffing, and final report generation.
- Fix the broken stage or stages instead of patching symptoms in isolation.
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
