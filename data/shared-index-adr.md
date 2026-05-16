# ADR: Shared Darc Index V1

## Status

Accepted for V1 implementation.

## Context

Darc is local-first: it syncs local Claude and Codex session archives, indexes redacted normalized session data into
SQLite, and exposes read/query commands over that local index. Team sharing should let a project reuse useful agent
session context from teammates without introducing a Darc-hosted cloud service and without uploading raw provider logs.

The accepted product direction is:

- Use Git as the transport and persistence backend.
- Store share data on branches named `darc/<name>`.
- `darc push <name>` pushes the active project's share branch `darc/<name>` to the default remote unless `--remote`
  selects a configured Darc share remote.
- Support a separate index-only remote through `darc remote add`.
- Encrypt shared content by default and redact before encryption.
- Keep minimal sync metadata visible, but encrypt user prompts, answers, tool payloads, file paths, model metadata,
  branch names, titles, previews, and other session content.
- Let users keep all sessions locally while selecting which sessions are shared.
- Default queries should remain local-only; shared results require explicit `--shared`, `--author`, or equivalent scope.
- Pull/fetch/merge should feel like Git, but imported shared sessions are indexed automatically after pull.

## Research Summary

Git ref names cannot contain `:` because Git uses colon in fetch and push refspecs. Branches therefore use real refs
under `refs/heads/darc/<name>`, while the CLI accepts the shorthand `<name>`.

Two Rust Git libraries were evaluated:

- `gix` is pure Rust and exposes a broad repository abstraction, but its feature surface and remote push APIs are still
  more complex for a small V1. Its own docs describe a large feature matrix and a separate trust model.
- `git2` is a mature binding to libgit2. It directly supports repository read/write, fetch, push, remote callbacks,
  credential helpers, and SSH agent credentials. It is not pure Rust, but it is the more canonical, stable choice for
  V1 push/fetch behavior.

The share payload format choices were:

- SQLite: rejected for remote artifacts because Darc already treats SQLite as a local cache and because schema migration,
  FTS tables, and page-level old content are poor sharing boundaries.
- CBOR or another binary Serde format: deferred. It would reduce bytes but add a new format dependency and make debugging
  harder. Payloads are encrypted, so compression and readability tradeoffs are less important for V1.
- JSON: accepted. Darc already depends on `serde_json`, and versioned JSON is simple to inspect, test, and migrate.

The encryption choices were:

- Custom crypto: rejected.
- Git hosting authorization only: rejected because remote hosts would still store plaintext session content.
- `age` X25519 recipients: accepted. The age Rust crate supports generated X25519 identities, public recipients, and
  multi-recipient encryption. Darc will use native age recipients rather than SSH recipient compatibility in V1.
- Ed25519 payload signatures: accepted. `age` recipient encryption is anonymous, so Darc signs encrypted sync and turn
  plaintext with a persistent local signing key before encryption and verifies signatures before import or pruning.

## Decision

Implement a V1 shared-index feature as a Git-backed, encrypted, redacted projection of canonical Darc index rows.

### Branch And Remote Model

- A user command branch argument such as `team` resolves to the Git branch `darc/team`.
- `darc push team` exports the active project and pushes `refs/heads/darc/team`.
- By default, Darc uses the active project's configured `git_upstream` or `origin` URL.
- `darc remote add <name> <url>` stores an optional share-only remote in Darc config.
- `darc push team --remote <name>`, `darc fetch team --remote <name>`, and `darc pull team --remote <name>` use that
  remote URL instead of the source repository.
- Darc stores a local share-cache Git repository under the Darc root. The source working tree is not checked out or
  modified when pushing or fetching share branches.

### Artifact Layout

Each share branch stores only Darc share artifacts:

```text
darc-share/v1/project.json
darc-share/v1/exporters/<exporter-fingerprint>/manifest.json
darc-share/v1/objects/<recipient-fingerprint>-<payload-sha256>.age
darc-share/v1/objects/sync-<recipient-fingerprint>-<payload-sha256>.age
```

`project.json` is visible and contains only routing metadata:

- artifact schema id and version
- canonical project key
- project name
- source Git URL fingerprint or normalized URL
- created/updated timestamps

Each exporter manifest is visible and contains only sync metadata:

- artifact schema id and version
- project key
- branch name
- exporter user id, display name, and email from Git config
- exporter public age recipient
- exporter public signing key
- session provider, session id, turn ordinal ranges, content hashes, object paths, and encrypted sync object path
- export timestamp

The branch tip preserves one manifest namespace per exporter. A push replaces only the current exporter's manifest and
objects that are no longer referenced by another exporter. This keeps a team branch usable for fresh pullers: the latest
tree can expose every teammate's current export instead of only the most recent pusher.

Merge bounds visible manifest discovery by exporter count and aggregate manifest bytes, and v1 import rejects artifacts
whose visible manifest, encrypted sync payload, or encrypted turn payload version is not exactly `1`.

Before committing a share cache update, Darc removes paths outside this artifact layout and stages only
`darc-share/v1`. Unexpected plaintext files, symlinks, orphan files, and unsupported artifact paths from a fetched
branch are not republished. Existing exporter namespaces are retained only when their encrypted sync payload and turn
objects can be decrypted and their exporter signatures verify against the visible exporter identity.

Encrypted object files contain the sensitive payload:

- an encrypted sync payload containing the exporter identity and latest turn keep set for authenticated pruning
- redacted session metadata
- redacted turn rows and `steps_json`
- model metadata
- file paths and derived evidence needed to rebuild local query tables
- any preview text

V1 exports one encrypted object per turn. Each object repeats the parent session metadata needed to import that turn.
This keeps incremental updates smooth when a session receives new turns: already-pushed turn objects remain unchanged and
only new or changed turn objects are added. Push reuses cached ciphertext only by deterministic object path and applies
explicit object-count and aggregate encrypted-byte caps while building the export.

### Project Identity

Darc local `project_id` remains host-local. Shared artifacts use a canonical project key:

1. Prefer the normalized Git upstream URL from Darc project config.
2. Fall back to `origin` URL read from the active Git repository.

3. If no Git URL exists, fail sharing setup with a clear message.

The key is `git:<normalized-url>`. Normalization removes trailing `.git`, lowercases GitHub-style hostnames, trims
trailing slashes, normalizes common SSH/HTTPS forms where safe, strips URL userinfo, lowercases only the host, and
preserves repository path case for case-sensitive Git hosts. Unsupported or local remotes such as `file://` URLs and
filesystem paths are rejected for project keys because the key is visible in share artifacts. Darc imports a share
artifact only when the canonical project key matches the active project's key.

### Identity And Provenance

Darc uses a persistent local signing identity for provenance and Git config for display metadata:

- `user.name` becomes display name.
- `user.email` becomes email.
- `user_id` is a stable SHA-256-derived id from the Ed25519 signing public key, so same-email exporters remain distinct
  and age-recipient key rotation does not change the exporter identity.

SQLite adds a `users` table plus provenance columns on `sessions`:

- `origin_kind`: `local` or `shared`
- `origin_user_id`
- `origin_remote`
- `imported_at`
- `share_state`: `unset`, `included`, or `excluded`

`origin_remote` is an alias-independent, non-secret provenance key derived from the credential-sanitized remote URL and
Git branch. Renaming a Darc remote alias for the same URL does not fork provenance, and retargeting an alias does not
allow the new remote to prune rows imported from the old remote.

This keeps canonical session and turn tables as the query source while preserving who a shared session came from.

### Share Selection

Session sharing is controlled by SQLite state, not by raw archive files:

- Project share policy: `manual` or `all`.
- Manual policy shares only sessions explicitly marked `included`.
- All policy shares every local session unless the session is marked `excluded`.
- Imported shared sessions are never re-exported by default.

CLI:

```text
darc share status
darc share policy manual
darc share policy all
darc share include <session>
darc share include --all
darc share exclude <session>
darc share exclude --all
darc share key
darc share recipient add <age-recipient>
darc share recipient list
darc share recipient remove <age-recipient>
```

`include --all` sets the policy to `all` and clears session-level overrides so previous exclusions do not survive.
`exclude --all` sets the policy to `manual` and clears explicit includes for the project.

### Encryption And Keys

Darc auto-generates an age X25519 identity on first sharing command that needs a key. It stores the secret key under:

```text
<darc-root>/keys/share.agekey
```

It also auto-generates a persistent Ed25519 signing key under:

```text
<darc-root>/keys/share.signingkey
```

The public recipient is shown by `darc share key` and included in visible manifests so teammates can add it as a
recipient. V1 encrypts every payload object to:

- the local user's public recipient
- every configured recipient in Darc config

V1 revocation is intentionally simple: removing a recipient prevents future objects from being encrypted to that
recipient, and the next share commit removes old payload objects from the branch tip. It does not revoke access to
objects that remain in older Git history or were already pulled. Deleting old historical access requires normal Git
history rewriting and remote retention controls.

### Import And Conflict Model

Fetch downloads the `darc/<name>` branch into the local Darc share cache. Merge imports all current exporter manifests
from the fetched tree into the local SQLite index. Pull is fetch plus merge.

Merge decrypts and validates the encrypted sync payload signature before destructive pruning. It imports only visible
manifest turns that also appear in the signed sync payload, prunes stale imported turns only for the authenticated
exporter identity contained in that decrypted payload, and then removes empty imported sessions for that exporter.
Malformed, mismatched, undecryptable, unauthenticated, schema-incompatible, or foreign exporter manifests, sync payloads,
and turn objects are skipped with warnings. Valid objects continue to import. This keeps one bad teammate chunk from
blocking the whole team index while preserving rigorous warning and test coverage.

Writes are isolated by content-addressed objects and manifest entries. Concurrent pushes may still have Git-level
non-fast-forward failures. V1 reports those failures and asks users to pull first, matching Git expectations.

### Query Scope

Default session-oriented read commands return only local sessions:

- `darc search ...`
- `darc list sessions ...`
- `darc show session ...`
- `darc show turn ...`
- `darc list turns ...`
- `darc stats turn ...`

V1 adds shared scope filters:

```text
--shared
--author <email-or-user-id>
--scope local|shared|all
```

`--shared` is shorthand for `--scope shared`. `--author` implies shared/all imported content unless `--scope` is more
specific. This can later be flipped to make `all` the default and require `--local` if the product wants shared context
to feel fully integrated.

Project-level file pivots and aggregate stats need a separate aggregate scoping pass because they mix sessions before
returning a result. V1 keeps the explicit shared contract on session list/search and session-resolved read commands,
then treats project-wide file/stat scope as follow-up work.

## Consequences

### Positive

- No Darc cloud service is required.
- Raw provider logs and raw SQLite files remain local.
- Git hosting sees only minimal sync metadata and encrypted payload chunks.
- JSON schemas are easy to inspect and migrate.
- Per-turn chunks support smooth incremental export for extended sessions.
- Provenance is stored directly with sessions, so query filters and attribution are straightforward.

### Negative

- `git2` introduces a libgit2 dependency and is not pure Rust.
- V1 revocation is forward-only.
- Visible manifest metadata still reveals project key, author identity, session ids, turn ordinals, object hashes, and
  coarse timestamps.
- Separate index-only remotes require explicit Darc remote config.
- Full Git conflict resolution is not hidden; non-fast-forward push failures remain user-visible.

## Implementation Phases

1. Storage schema:
   - Add `users`, `project_share_policies`, provenance columns, and share selection state.
   - Add store APIs for policy updates, selected export rows, and idempotent shared import.
2. Share crate:
   - Add `darc-share` as a leaf capability crate.
   - Implement project key resolution, Git config identity, age key management, recipient config, JSON schemas, export,
     import, warning collection, and Git cache push/fetch.
3. CLI:
   - Add `darc share`, `darc remote`, `darc push`, `darc fetch`, `darc merge`, and `darc pull`.
   - Add query scope flags for shared and author filtering.
4. Docs:
   - Update README privacy and command examples.
   - Update query protocol for provenance and shared query filters.
   - Add a changelog entry under Unreleased.
5. Tests:
   - Unit test branch naming, project key normalization, user id derivation, key generation, encryption/decryption,
     malformed object warnings, share selection policy, and import idempotency.
   - Integration test export/fetch/pull against a local bare Git repository.
   - Query tests for local default, shared scope, all scope, and author filters.

## References

- Git ref format docs: `:` is refspec syntax, so the real branch namespace is `darc/<name>`.
- `git2` docs: `Remote::push`, fetch/push callbacks, credential helpers, and SSH agent credentials.
- `gix` docs: pure-Rust repository abstraction with a broader and more complex feature matrix.
- `age` docs: X25519 recipients and identities for multi-recipient file encryption.
- `serde_json` docs: streaming JSON serialization/deserialization over `Read` and `Write`.
