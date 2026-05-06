# Claude support

## Claude support policy

Darc tracks Claude rollout support at three levels:

- `exact`: versions backed by checked fixtures and explicit parser coverage. Darc stores this as an explicit audited version set in `crates/rollout/src/claude/version.rs`, with the support rationale recorded in [Schema changelog](schema-changelog.md). Today the exact set includes audited anchors from `1.0.91` through `2.1.128`, but only for the individual versions listed in the Rust table.
- `best_effort_forward`: versions that map onto a known Claude schema epoch but are not fixture-backed exact matches. Darc preserves unknown payloads instead of dropping them, so parsing continues with degraded certainty rather than failing fast.
- `unsupported`: versions earlier than the practical Claude audit floor (`1.0.88`) or malformed rollouts that cannot be parsed safely. Individual unsupported rollout files are skipped during `darc index`; they do not abort the entire index run.

The current parser epochs are broader than the exact set. Exactness is intentionally narrower than epoch membership, and `latest_exact_supported_claude_cli_version()` means the highest exact audited release, not every lower release.

Known adjacent audit drift boundaries are currently `2.0.22`, `2.1.85`, and `2.1.90`. Only `2.1.90` is currently parser-relevant: checked fixtures and parser behavior treat it as the top-level `attachment` boundary. `2.0.22` changed the audit's stream-json subtype manifest, and `2.1.85` is a non-breaking manifest change where audited fixtures stopped emitting optional `progress` lines. Other epoch boundaries are coarse compatibility windows inferred from sampled audits and fixtures, not proven adjacent breaking versions.
