# Changelog

All notable Darc release changes should be summarized here.

## Unreleased

## 0.1.3

- Add `darc refresh --auto` to enable automatic background refresh on macOS, start it immediately, and show setup progress.
- Add `darc agent-help` and a marker-wrapped AGENTS.md guidance line for coding agents.

## 0.1.2

- Refresh workspace package versions in `Cargo.lock` during release preparation before running locked Cargo checks.
- Keep release-profile CI checks fail-open when the workspace version gate cannot read a push base.
- Mark Codex 0.128.0 and sampled audited Claude Code rollout versions from 1.0.91 through 2.1.128 as exact-supported after schema review.
- Fix the Claude schema audit for modern native-wrapper npm packages.
- Report incomplete Claude schema audits separately from transcript schema drift.
- Distinguish sampled Claude drift boundaries from proven first-drift versions in audit output.
- Retry Claude schema audit fixtures with the next low-cost model profile when a cheaper profile misses required tool coverage.

## 0.1.1

- Improve file analytics to drop shell syntax pseudo-paths while preserving concrete tool, patch, metadata-reference, and shell-test paths.
- Install `darc` into `~/.local/bin` by default when using the shell installer.
- Add `darc upgrade` for checking and applying newer Darc CLI releases.
- Add a documented, opt-in startup nudge for newer Darc CLI releases.
- Keep passive upgrade checks out of no-write command modes and anonymous unless `darc upgrade` is run explicitly.
- Improve `darc upgrade` root handling and custom-install fallback guidance.
- Keep `darc upgrade --check --json` installer guidance consistent with custom installs and bound remote HTTP errors.
- Show commented common workflow examples in top-level help, including `darc upgrade --check`.
- Build release artifacts with a pinned stable Rust toolchain and check release-profile Linux targets when the workspace
  version changes.

## 0.1.0

- Initial release.
