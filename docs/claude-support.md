# Claude support and analytics

## Claude support policy

Darc tracks Claude rollout support at three levels:

- `exact`: versions backed by checked fixtures and explicit parser coverage. Darc's current exact Claude rollout support boundary is still anchored by `latest_exact_supported_claude_cli_version()` in `crates/core/src/rollout/claude/version.rs`. Today that exact set is the observed fixture-backed releases `2.1.81`, `2.1.84`, and `2.1.87`.
- `best_effort_forward`: versions that map onto a known Claude schema epoch but are not fixture-backed exact matches. Darc preserves unknown payloads instead of dropping them, so parsing continues with degraded certainty rather than failing fast.
- `unsupported`: versions earlier than the practical Claude audit floor (`1.0.88`) or malformed rollouts that cannot be parsed safely. Individual unsupported rollout files are skipped during `darc index`; they do not abort the entire index run.

The current parser epochs are broader than the exact set. Exactness is intentionally narrower than epoch membership.

## Analytics helper

After indexing archived sessions with `darc index`, library consumers can summarize the indexed Claude rollout corpus with:

```rust
use darc_core::report_claude_rollout_analytics;

let report = report_claude_rollout_analytics(None)?;
```

The report aggregates indexed Claude sessions and turns by schema family, determinism, completion status, tool usage, delegation events, attachments, hook summaries, and turn durations from the normalized SQLite index.
