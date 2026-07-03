# Claude support

## Claude support policy

Darc tracks Claude rollout support at three levels:

- `exact`: versions backed by checked fixtures and explicit parser coverage. Darc stores this as an explicit audited version set in `crates/rollout/src/claude/version.rs`, with the support rationale recorded in [Schema changelog](schema-changelog.md). Today the exact set includes audited anchors from `1.0.91` through `2.1.199`, but only for the individual versions listed in the Rust table.
- `best_effort_forward`: versions that map onto a known Claude schema epoch but are not fixture-backed exact matches. Darc preserves unknown payloads instead of dropping them, so parsing continues with degraded certainty rather than failing fast.
- `unsupported`: versions earlier than the practical Claude audit floor (`1.0.88`) or malformed rollouts that cannot be parsed safely. Individual unsupported rollout files are skipped during `darc index`; they do not abort the entire index run.

The current parser epochs are broader than the exact set. Exactness is intentionally narrower than epoch membership, and `latest_exact_supported_claude_cli_version()` means the highest exact audited release, not every lower release.

Known adjacent audit drift boundaries are currently `2.0.22`, `2.1.85`, `2.1.90`, `2.1.160`, `2.1.161`, `2.1.178`, `2.1.198`, and `2.1.199`. Parser-relevant boundaries are `2.1.90` for top-level `attachment` lines, `2.1.161` for persisted `promptSource` user-line metadata, and `2.1.198` for observed `stop_hook_summary` and `origin` metadata. The `2.0.22`, `2.1.85`, `2.1.160`, `2.1.178`, and `2.1.199` audit drifts are recorded in the schema changelog but did not require additional persisted JSONL parser epoch splits.
