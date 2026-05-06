# Schema changelog

This maintainer ledger records why Darc treats upstream rollout versions as exact, best-effort, or unsupported. It complements the Rust support tables and tests; it is not the source of truth by itself.

## Codex

### Exact through 0.128.0

- Upstream versions: stable Codex CLI releases `0.33.0 ..= 0.128.0`.
- Darc support: `exact` when the rollout version is within the supported parser families and the parser sees no unsupported exact-mode fields.
- Schema owner: `crates/rollout/src/codex/version.rs`.
- Audit evidence: `darc codex-schema-audit` compares published GitHub release schemas against `latest_exact_supported_codex_cli_version()`.
- Notes: `0.128.0` is the current exact boundary. Newer stable releases stay `best_effort_forward` until a schema audit confirms compatibility and the boundary is advanced.

## Claude Code

Claude Code exact support is version-specific. Darc marks a version exact only after live audit fixtures have exercised that published npm package and the parser can consume the observed transcripts without falling back to best effort.

Parser epochs are broader compatibility buckets. When sampled endpoints on both sides of an unsampled interval emit the same normalized transcript manifest, Darc treats the interval as compatible for epoch purposes, but the skipped versions remain non-exact until they are audited directly.

### Known adjacent drift boundaries

- `2.0.22`: live refinement found `2.0.21` compatible with the prior manifest and `2.0.22` drifted. The visible audit-manifest change was `stream_event_subtypes` gaining `hook_response` in stream-json output; Darc does not currently treat this as a persisted rollout parser epoch boundary.
- `2.1.85`: live refinement found `2.1.84` before the change and `2.1.85` after it. The audited fixtures no longer emitted `progress` lines. Darc treats progress as optional preserved metadata, so this was not parser-breaking.
- `2.1.90`: checked fixtures and parser behavior treat this as the top-level `attachment` boundary. `2.1.89` is before attachment-line support; `2.1.90` and later map to the attachment-aware parser epoch.

The other parser epoch starts in `crates/rollout/src/claude/version.rs` are provisional coarse windows, not proven adjacent breaking versions.

### Registry stride-10 live sweep

- Upstream versions: stable npm releases from `0.2.9` through `2.1.128`; npm reported `393` stable releases.
- Darc support: `exact` only for sampled versions that completed the live fixture suite and parsed cleanly.
- Evidence: live `darc claude-schema-audit --use-host-auth --from-version 0.2.9 --sample-stride 10 --survey-mode refine`, followed by a resumed cached pass from `1.0.91` after early releases proved non-auditable under the current transcript collector.
- Low-cost profile: the audit probed Haiku first, then Sonnet; older CLIs used `ANTHROPIC_MODEL` where `--model` did not exist, and modern CLIs used `--effort low` where available. Opus was never selected explicitly.
- Exact versions added from this sweep: `1.0.91`, `1.0.105`, `1.0.115`, `1.0.126`, `2.0.10`, `2.0.21`, `2.0.22`, `2.0.31`, `2.0.44`, `2.1.7`, `2.1.18`, `2.1.29`, `2.1.40`, `2.1.52`, `2.1.64`, `2.1.75`, `2.1.100`, `2.1.113`, and `2.1.124`.
- Existing exact versions retained: `2.1.81`, `2.1.84`, `2.1.85`, `2.1.86`, `2.1.87`, `2.1.126`, and `2.1.128`.
- Failed sampled versions remain non-exact: early sampled releases before `1.0.91` either did not emit the live project transcript layout, timed out, or could not be forced onto Haiku/Sonnet; sampled `2.0.55`, `2.0.65`, and `2.0.75` failed fixture-quality checks because the model did not trigger required tools.
- Notes: The first transcript drift in the sampled `1.0.91 ..= 2.1.128` pass was narrowed to `2.0.22`. Later sampled drift windows were recorded, but the existing Claude parser epochs already cover the successful sampled transcripts.

### 2.1.84 ..= 2.1.86 stride-2 follow-up

- Darc support: `exact` for the published versions `2.1.84`, `2.1.85`, and `2.1.86`.
- Schema id: `claude.*_transcript.2_1_84_to_2_1_89`.
- Evidence: live `darc claude-schema-audit --use-host-auth --from-version 2.1.84 --sample-stride 2 --survey-mode refine`.
- Notes: The transcript manifest changed at `2.1.85`: the audited fixtures no longer emitted `progress` lines. Darc already treats progress lines as optional preserved metadata, so no parser epoch split was required.

### 2.1.126 ..= 2.1.128 stride-1 follow-up

- Darc support: `exact` for `2.1.126` and `2.1.128`.
- Schema id: `claude.*_transcript.2_1_90_to_latest`.
- Evidence: live `darc claude-schema-audit --use-host-auth --from-version 2.1.126 --sample-stride 1 --survey-mode refine`.
- Notes: Transcript manifests matched. Supplementary Agent SDK metadata changed from `0.2.126` to `0.2.128`, but SDK drift does not determine rollout transcript compatibility.
