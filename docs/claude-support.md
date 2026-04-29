# Claude support

## Claude support policy

Darc tracks Claude rollout support at three levels:

- `exact`: versions backed by checked fixtures and explicit parser coverage. Darc's current exact Claude rollout support boundary is still anchored by `latest_exact_supported_claude_cli_version()` in `crates/rollout/src/claude/version.rs`. Today that exact set is the observed exact-supported releases `2.1.81`, `2.1.84`, and `2.1.87`.
- `best_effort_forward`: versions that map onto a known Claude schema epoch but are not fixture-backed exact matches. Darc preserves unknown payloads instead of dropping them, so parsing continues with degraded certainty rather than failing fast.
- `unsupported`: versions earlier than the practical Claude audit floor (`1.0.88`) or malformed rollouts that cannot be parsed safely. Individual unsupported rollout files are skipped during `darc index`; they do not abort the entire index run.

The current parser epochs are broader than the exact set. Exactness is intentionally narrower than epoch membership.
