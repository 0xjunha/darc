# Schema audits

These hidden maintainer commands check whether Darc's rollout parsers still match published upstream CLI releases before a patch release.

## Codex schema audit

Run the hidden Codex rollout schema audit when the Codex rollout parser may need a compatibility review:

```bash
darc codex-schema-audit
```

If you want to override the default released-binary cache location, pass it explicitly:

```bash
darc codex-schema-audit --cache-dir /path/to/cache
```

What the audit checks:

- Darc's current exact Codex rollout support boundary is defined in `crates/rollout/src/codex/version.rs` by `latest_exact_supported_codex_cli_version()`.
- The audit queries Codex GitHub Releases and walks stable release tags from the latest stable tag down to that exact-support boundary.
- For each audited tag, it downloads that release's published platform binary package, caches it locally, runs `codex app-server generate-internal-json-schema`, and compares the exported `RolloutLine.json` schema against the boundary tag's schema.
- If the schema is unchanged across the audited range, the command reports compatibility. It does not update code or docs automatically.
- If the schema drifts, the command exits `1` and reports the first drifting tag plus likely Darc files to review.

What the audit does not do:

- It does not inspect a local Codex source checkout.
- It does not build Codex from source.
- It only audits stable releases that are currently published on Codex GitHub Releases.
- It does not bump Darc's exact-support boundary automatically.

What the audit caches locally:

```bash
~/Library/Caches/darc/schema-audit/codex
```

On Linux and Windows, the default cache root follows the platform cache directory returned by the OS.

If you see an error like:

```text
GitHub Releases are missing the stable release tag `rust-v0.118.0`
```

the published release catalog no longer contains the exact-support boundary tag that Darc needs as the audit baseline. Darc cannot advance the audit until that release remains available or the exact-support boundary is updated.

## Claude schema audit

The hidden Claude rollout schema audit command exists, but the live end-to-end pipeline is currently tracked as
untrusted in [Backlog](todo.md). Until that backlog item is closed, treat manual runs as investigative diagnostics,
not release-gating compatibility proof.

Run the command when reproducing or revalidating the Claude audit pipeline:

```bash
darc claude-schema-audit --use-host-auth
```

If you want to override the default released-package cache location, pass it explicitly:

```bash
darc claude-schema-audit --use-host-auth --cache-dir /path/to/cache
```

What the Claude audit checks:

- Darc's current exact Claude rollout support boundary is defined in `crates/rollout/src/claude/version.rs` by `latest_exact_supported_claude_cli_version()`.
- The audit queries the npm registry for published `@anthropic-ai/claude-code` releases and walks stable package versions from the latest published version down to that exact-support boundary.
- For each audited version, it downloads the published package tarball, caches it locally, runs deterministic fixture prompts against the released CLI, and derives a normalized transcript schema manifest from the emitted local transcript JSONL plus hook and stream-json output.
- Darc does not provide an OS-level sandbox for executing published Claude packages. The audit therefore requires explicit `--use-host-auth` opt-in and runs the released CLI with your host Claude login state plus an allowlist of Claude/cloud auth environment variables, not your full shell environment.
- Each fixture run uses a dedicated workspace under `~/src/.darc-claude-audit`, so Claude session logs never record your actual Darc repository path as the audited project.
- The command also derives a supplementary Agent SDK surface manifest from published `.d.ts` files when a matching `@anthropic-ai/claude-agent-sdk` package advertises compatibility with that Claude Code version.
- If the transcript manifest is unchanged across the audited range, the command reports compatibility. It does not update code or docs automatically.
- If the transcript manifest drifts, the command exits `1` and reports the first drifting version plus likely Darc files to review. Supplementary Agent SDK drift is reported separately and does not determine compatibility by itself.

Claude audit runtime requirements:

- A working local `node` runtime.
- A working local `python3` or `python` runtime for hook capture.
- Claude authentication that the released CLI can use.

What the Claude audit does not do:

- It does not inspect a local Claude Code source checkout.
- It does not build Claude Code from source.
- It does not claim that Agent SDK types and local transcript JSONL are equivalent.
- It does not bump Darc's exact-support boundary automatically.

What the Claude audit caches locally:

```bash
~/Library/Caches/darc/schema-audit/claude
```
