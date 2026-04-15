# TODO

### Context Wiki pre-production blockers
- Harden external agent runtime auth boundaries before serious production deploy.
  - Today `darc wiki digest` launches external Codex/Claude CLIs with inherited host environment and ambient login state.
  - `auth_profile` is currently metadata only; it does not select or constrain credentials at runtime.
  - Decide and implement one explicit production policy:
    - Darc-managed profile-to-credential mapping with a constrained runtime environment.
    - A deliberately named host-auth passthrough mode that is not the default.
    - Provider-native runtimes where Darc owns auth directly.
  - Ensure runtime execution does not accidentally expose unrelated host secrets to digest runs.

### Context Wiki follow-ups
- Split Context Wiki extraction from registry curation.
  - Let the digest extractor agent propose new category/domain candidates when an extracted decision trace does not fit the current registry cleanly.
  - Extend the digest proposal contract with optional proposed registry additions instead of allowing extractors to mutate the canonical registry directly.
  - Add a separate registry-curator agent or backend-owned curation step that validates, accepts, rejects, or defers proposed category/domain additions.
  - Keep hard backend validation for slug shape, dedupe, similarity to existing labels, and per-run proposal limits so registry quality stays stable.
  - Prefer stricter promotion rules for categories than domains because category changes reshape the taxonomy more deeply.
  - Consider a pending/promoted lifecycle so repeated accepted proposals can graduate into the canonical registry without immediate manual edits.
- Add Windows support for stale-run pid liveness checks.
  - Stale run repair currently uses a Unix-only `kill(pid, 0)` existence check before rewriting a run to `interrupted`.
  - On non-Unix targets the fallback treats worker pids as not live, which is not the intended long-term behavior for Windows support.
  - Introduce a Windows-capable process-liveness path so stale-run repair semantics stay consistent across supported hosts.
- Remove the N+1 context-loading path for digest context assembly.
  - `load_selected_session_context()` currently queries the turn list and then calls `query_turn()` once per turn.
  - That rebuilds query context and reopens SQLite repeatedly for long sessions or multi-session digests.
  - Add a batch/session-scoped turn-detail query path in `darc-query` so one digest session can be loaded with one connection and one coherent query flow.
  - This is not a correctness blocker for production, but it should be addressed before higher-scale Context Wiki usage.

### Add historical checkout detection for deleted worktrees.
- Persist enough evidence for live Codex/CC or git checkouts to recognize the same project after deletion, including observed path, resolved repo root, remote origin, and last-seen time.
- Make `sync` treat old rollout `cwd` values as the same project only when backed by prior evidence; otherwise surface them as low-confidence candidates instead of silently adding them to `known_paths`.

### Collapse changed-rollout parsing to one pass.
- In `crates/core/src/rollout/codex/mod.rs`, derive user-turn boundaries during streaming instead of the current pre-scan.
- Preserve current `event_msg.user_message` vs `response_item.role == "user"` behavior.
- Keep parser memory bounded to the current turn.
- Leave incremental reindexing in `crates/core/src/index.rs` unchanged.

### Tighten unchanged-rollout detection before skipping reindex.
- Current index skip detection trusts `archive_path`, file size, and mtime.
- If rollout contents become corrupt without changing those values, indexing can incorrectly skip reindexing and keep stale indexed data.
- Consider storing or deriving a stronger content identity for changed-rollout detection.

### Search follow-ups
- Improve file-search scalability for substring matching.
  - Exact and prefix file-name/path search now use the indexed fast path.
  - The final substring fallback still uses `%...%` matching and can scan large per-project histories.
  - Decide whether to keep substring search as a best-effort slow path, restrict file search to exact/prefix, or add a dedicated substring/trigram-style side index.
- Refine search indexing policy for safe, high-signal text.
  - Keyword search is intentionally conservative today.
  - Decide whether to selectively index more tool-call text, such as curated argument summaries or shell commands, without reintroducing raw-output or raw-payload leakage.
  - Make the indexing policy explicit and documented as part of the query/search contract.
- Add representative search-scale verification.
  - Add a lightweight regression path that checks search behavior on larger synthetic histories, especially file-name/path ranking and pagination.
  - Prefer something that can catch accidental query-plan or ranking regressions without becoming a brittle performance benchmark.

### Claude Code support
- Add a Claude schema audit workflow similar to `codex-schema-audit`.
  - Fetch official Claude Code releases or other upstream-distributed binaries in a reproducible way.
  - Run deterministic fixture-generation scenarios against released Claude Code builds.
  - Derive transcript schema manifests from emitted local session JSONL.
  - Diff derived transcript schemas across versions and report exact coverage, first drift version, and likely parser files to update.
  - Keep SDK / typed JSON API schema auditing separate from local transcript JSONL auditing unless they are proven equivalent.
- Build a Claude rollout schema epoch model instead of the current observed-version allowlist.
  - Expand fixture coverage across more Claude Code versions and transcript variants.
  - Define stable schema families / epochs for archived local transcript JSONL, not just SDK message types.
  - Codify parser families and exact version coverage from the Claude audit output.
  - Keep exact vs best-effort compatibility decisions explicit and reproducible.
- Strengthen Claude turn-boundary extraction in `crates/core/src/rollout/claude/mod.rs`.
  - Refine parsing per schema family / epoch instead of relying on one heuristic parser for every Claude version.
  - Audit whether any additional explicit completion / abort / handoff markers exist in observed transcripts.
  - Reduce reliance on prompt-like `user` heuristics where possible.
  - Preserve current safe behavior for ambiguous boundaries rather than inventing brittle merges.

#### Other Claude follow-ups
- Normalize more Claude provider events into the shared turn model.
  - Map more `system`, `progress`, `origin`, and assistant content variants into structured steps instead of raw preserved payloads where that materially improves indexing.
  - Keep raw preserved payload support for forward-compatible unknown variants.
- Improve Claude tool-result fidelity.
  - Preserve better timestamps and linkage for tool results currently reconstructed from later `user` tool-result lines.
  - Investigate whether archived auxiliary files can improve reconstruction without adding brittle coupling.
- Harden Claude boilerplate / meta filtering.
  - Distinguish real user prompts from command wrappers, task notifications, local-command caveats, and other meta-only transcript lines.
  - Avoid dropping legitimate prompts when the format is only partially understood.
- Index archived Claude auxiliary artifacts when useful.
  - Ingest subagent `.meta.json` fields such as `agentType` and `description`.
  - Decide whether any archived auxiliary text outputs should be indexed directly, linked from turns, or left archive-only.
- Add a public standalone Claude parser API if external callers will need it.
  - Mirror the ergonomics of `parse_codex_rollout` only if the Claude parser contract is stable enough to expose.
- Improve Claude pre-index inspection.
  - Add a lightweight inspection path that can validate Claude session identity, version hints, and candidate kind before full parse.
  - Keep it soft-fail for unknown variants so one bad Claude rollout never crashes the whole index run.
