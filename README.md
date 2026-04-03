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
