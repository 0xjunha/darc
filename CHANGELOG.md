# Changelog

All notable Darc release changes should be summarized here.

## Unreleased

- Add Git-backed encrypted shared indexes with `darc share`, `darc remote`, `darc push/fetch/merge/pull`, and explicit shared query filters.
- Store shared-index payloads as compressed encrypted V1 chunks and use Git LFS for encrypted share objects when available.
- Use the system `git` executable for shared-index fetches and pushes so Darc honors existing local Git authentication.
- Harden shared indexes so branch tips retain all exporters, visible metadata avoids credential/local-path leaks, and explicit share selections survive re-indexing.
- Authenticate shared index payloads, isolate bad exporter artifacts during pull, and redact share remote URLs in CLI output.
- Keep shared index pulls and pushes isolated from malformed manifests, unauthenticated branch artifacts, unexpected branch files, and oversized cached exports.
- Bound shared-index manifest discovery, reject unsupported artifact versions and invalid share branch names, and apply author filters when resolving search session prefixes.
- Keep share cache cleanup inside real cache directories, store imported provenance by canonical remote identity, and reject manifest turns missing from the signed sync payload.
- Prevent imported shared sessions from replacing another exporter's session with the same id.
- Keep shared imports tied to fetched branch tips, normalize common share remote URL aliases for provenance, and re-encrypt exports for the current recipient set.
- Reject symlinked share artifact ancestors before manifest or payload reads and prevent invalid duplicate manifests from masking a valid exporter import.
- Strip query and fragment secrets from Git URLs, reset tracked cache changes before merge, and defensively redact share exports.
- Preserve shared index state across rebuilds, reject unaddressable imported session ids, and redact credentialed pathless remotes.
- Reuse trusted encrypted share objects for unchanged exports and keep unaddressable provider child sessions local.
- Reuse previous signed share exports for unchanged selected session sets to speed up incremental pushes.
- Authenticate shared manifest object metadata in sync payloads and bound exporter-directory scans.
- Require signed sync entries to match visible manifests exactly and reject symlinked share key files.
- Prune stale shared turns when replacement imports fail and batch pull imports in one SQLite transaction.
- Redact common secrets, credential material, local home paths, and bulky data blobs before storing or migrating indexed session data.
- Reduce false-positive redaction of indexed examples, CLI help text, boolean config values, search patterns, and comparison code.

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
