# Changelog

All notable Darc release changes should be summarized here.

## Unreleased

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
