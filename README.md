# memstack

[![License](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)
[![CI](https://github.com/0xjunha/memstack/actions/workflows/ci.yml/badge.svg)](https://github.com/0xjunha/memstack/actions/workflows/ci.yml)
[![codecov](https://codecov.io/github/0xjunha/memstack/graph/badge.svg?token=J5ZVVBJ3U9)](https://codecov.io/github/0xjunha/memstack)

## Maintainer checks

Run the hidden Codex rollout schema audit before cutting a Memstack patch release when the Codex rollout parser may need a compatibility review:

```bash
memstack codex-schema-audit
```

If you want to override the default released-binary cache location, pass it explicitly:

```bash
memstack codex-schema-audit --cache-dir /path/to/cache
```

What the audit checks:

- Memstack's current exact Codex rollout support boundary is defined in `crates/core/src/rollout/codex/version.rs` by `latest_exact_supported_codex_cli_version()`.
- The audit queries Codex GitHub Releases and walks stable release tags from the latest stable tag down to that exact-support boundary.
- For each audited tag, it downloads that release's published platform binary package, caches it locally, runs `codex app-server generate-internal-json-schema`, and compares the exported `RolloutLine.json` schema against the boundary tag's schema.
- If the schema is unchanged across the audited range, the command reports compatibility. It does not update code or docs automatically.
- If the schema drifts, the command exits `1` and reports the first drifting tag plus likely Memstack files to review.

What the audit does not do:

- It does not inspect a local Codex source checkout.
- It does not build Codex from source.
- It only audits stable releases that are currently published on Codex GitHub Releases.
- It does not bump Memstack's exact-support boundary automatically.

What the audit caches locally:

```bash
~/Library/Caches/memstack/schema-audit/codex
```

On Linux and Windows, the default cache root follows the platform cache directory returned by the OS.

If you see an error like:

```text
GitHub Releases are missing the stable release tag `rust-v0.118.0`
```

the published release catalog no longer contains the exact-support boundary tag that Memstack needs as the audit baseline. Memstack cannot advance the audit until that release remains available or the exact-support boundary is updated.

Run the hidden Claude rollout schema audit before cutting a Memstack patch release when the Claude rollout parser may need a compatibility review:

```bash
memstack claude-schema-audit --use-host-auth
```

If you want to override the default released-package cache location, pass it explicitly:

```bash
memstack claude-schema-audit --use-host-auth --cache-dir /path/to/cache
```

What the Claude audit checks:

- Memstack's current exact Claude rollout support boundary is defined in `crates/core/src/rollout/claude/version.rs` by `latest_exact_supported_claude_cli_version()`.
- The audit queries the npm registry for published `@anthropic-ai/claude-code` releases and walks stable package versions from the latest published version down to that exact-support boundary.
- For each audited version, it downloads the published package tarball, caches it locally, runs deterministic fixture prompts against the released CLI, and derives a normalized transcript schema manifest from the emitted local transcript JSONL plus hook and stream-json output.
- Memstack does not provide an OS-level sandbox for executing published Claude packages. The audit therefore requires explicit `--use-host-auth` opt-in and runs the released CLI with your host Claude login state plus an allowlist of Claude/cloud auth environment variables, not your full shell environment.
- Each fixture run uses a dedicated workspace under `~/src/.memstack-claude-audit`, so Claude session logs never record your actual Memstack repository path as the audited project.
- The command also derives a supplementary Agent SDK surface manifest from published `.d.ts` files when a matching `@anthropic-ai/claude-agent-sdk` package advertises compatibility with that Claude Code version.
- If the transcript manifest is unchanged across the audited range, the command reports compatibility. It does not update code or docs automatically.
- If the transcript manifest drifts, the command exits `1` and reports the first drifting version plus likely Memstack files to review. Supplementary Agent SDK drift is reported separately and does not determine compatibility by itself.

Claude audit runtime requirements:

- A working local `node` runtime.
- A working local `python3` or `python` runtime for hook capture.
- Claude authentication that the released CLI can use.

What the Claude audit does not do:

- It does not inspect a local Claude Code source checkout.
- It does not build Claude Code from source.
- It does not claim that Agent SDK types and local transcript JSONL are equivalent.
- It does not bump Memstack's exact-support boundary automatically.

What the Claude audit caches locally:

```bash
~/Library/Caches/memstack/schema-audit/claude
```

## Claude support policy

Memstack tracks Claude rollout support at three levels:

- `exact`: versions backed by checked fixtures and explicit parser coverage. Memstack's current exact Claude rollout support boundary is still anchored by `latest_exact_supported_claude_cli_version()` in `crates/core/src/rollout/claude/version.rs`. Today that exact set is the observed fixture-backed releases `2.1.81`, `2.1.84`, and `2.1.87`.
- `best_effort_forward`: versions that map onto a known Claude schema epoch but are not fixture-backed exact matches. Memstack preserves unknown payloads instead of dropping them, so parsing continues with degraded certainty rather than failing fast.
- `unsupported`: versions earlier than the practical Claude audit floor (`1.0.88`) or malformed rollouts that cannot be parsed safely. Individual unsupported rollout files are skipped during `memstack parse`; they do not abort the entire parse run.

The current parser epochs are broader than the exact set. Exactness is intentionally narrower than epoch membership.

## Claude analytics helper

After indexing archived sessions with `memstack parse`, library consumers can summarize the indexed Claude rollout corpus with:

```rust
use memstack_core::report_claude_rollout_analytics;

let report = report_claude_rollout_analytics(None)?;
```

The report aggregates indexed Claude sessions and turns by schema family, determinism, completion status, tool usage, delegation events, attachments, hook summaries, and turn durations from the normalized SQLite index.
