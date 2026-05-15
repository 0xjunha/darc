# Changelog

All notable Darc release changes should be summarized here.

## Unreleased

- Redact common secrets, credential material, local home paths, and bulky data blobs before storing or migrating indexed session data.

## [0.1.6] - 2026-05-13

- Add `darc index --rebuild` to recreate the shared SQLite index from every configured project's archived sessions, and point users to it when the local index cannot be opened or migrated.
- Show GitHub Release titles as tags, such as `v0.1.6`, while preserving dated changelog headings.
- Narrow internal Rust storage APIs so SQLite schema details are no longer exposed outside the storage crate.
- Streamline public documentation around JSON query contracts and remove the internal backlog from docs.

## [0.1.5] - 2026-05-11

- Clarify README guidance for agent setup and prompt-driven prior-session investigations.
- Show release dates in changelog version headings and have release preparation add them automatically.
- Speed up regex search for queries with a required literal prefix.
- Harden auto-refresh restart, debounce, stale lock, and service status reporting.
- Avoid killing freshly bootstrapped auto-refresh services during start.

## [0.1.4] - 2026-05-08

- Restart already-running auto-refresh services reliably and structure LaunchAgent startup failures.
- Avoid probing arbitrary historical Codex `cwd` directories during refresh; Darc now preserves same-repo matching from logged `git.repository_url` metadata, keeps explicitly linked rename paths recoverable when backed by scoped remote evidence, and avoids broad-prefix imports from nested repos with mismatched remotes.
- Mention the initial SQLite backfill in `darc refresh --auto` setup progress.
- Clarify `darc agent-help` guidance around history-dependent work, current-code verification, and reporting prior-session evidence.

## [0.1.3] - 2026-05-07

- Add `darc refresh --auto` to enable automatic background refresh on macOS, start it immediately, and show setup progress.
- Add `darc agent-help` and a marker-wrapped AGENTS.md guidance line for coding agents.

## [0.1.2] - 2026-05-06

- Refresh workspace package versions in `Cargo.lock` during release preparation before running locked Cargo checks.
- Keep release-profile CI checks fail-open when the workspace version gate cannot read a push base.
- Mark Codex 0.128.0 and sampled audited Claude Code rollout versions from 1.0.91 through 2.1.128 as exact-supported after schema review.
- Fix the Claude schema audit for modern native-wrapper npm packages.
- Report incomplete Claude schema audits separately from transcript schema drift.
- Distinguish sampled Claude drift boundaries from proven first-drift versions in audit output.
- Retry Claude schema audit fixtures with the next low-cost model profile when a cheaper profile misses required tool coverage.

## [0.1.1] - 2026-05-04

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

## [0.1.0] - 2026-05-04

- Initial release.
