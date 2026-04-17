# Context Wiki

Darc's Context Wiki is an experimental backend-owned workflow for durable project knowledge stored under
`~/.darc/context-wiki/`.

Current MVP scope:

- read-side wiki queries via `darc query wiki ... --json`
- imperative digest lifecycle via `darc wiki digest start` and `darc wiki digest cancel`
- imperative entry lifecycle via `darc wiki entry discard` and `darc wiki entry restore`
- external CLI runtime for `claude`, plus a gated `codex` opt-in for manual testing
- structured `decision_trace` proposal validation
- canonical merge into decision-trace entry markdown plus one digest report per successful run
- durable run artifacts and logs for each digest run

Current gaps:

- `auth_profile` is recorded as run metadata only and does not constrain runtime credentials

## Read Side

Use `darc query wiki ... --json` to inspect Context Wiki state without invoking a runtime.

- `darc query wiki registry --root <path> --project-id <id> --json`
- `darc query wiki entries --root <path> --project-id <id> [--category <id>] [--domain <id>] [--status <status>] [--grep <text>] [--evidence-ref <provider>:<session-id>#<turn-ordinal>] [--covers-session <provider>:<session-id>] --json`
- `darc query wiki entry --root <path> --project-id <id> --entry-id <id> --json`
- `darc query wiki digests --root <path> --project-id <id> [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] --json`
- `darc query wiki digest --root <path> --project-id <id> --digest-id <id> --json`
- `darc query wiki runs --root <path> --project-id <id> [--status <status>] [--since <iso-8601|<days>d>] [--until <iso-8601|<days>d>] [--limit <n>] --json`
- `darc query wiki run --root <path> --project-id <id> --run-id <id> --json`

The query protocol remains the machine-readable contract for desktop and other clients. See
[Query protocol](query-protocol.md).

For history investigation and proposal preparation, prefer the project-scoped read surface over ad hoc SQLite reads:

- `darc query turns --grep ...` to find load-bearing turns by content with optional role and context filters
- `darc query files --path ...` to pivot from one file path to the sessions that touched it
- `darc query files --co-touched-with ...` to discover nearby files often touched in the same sessions
- `darc query session-files ...` to inspect one session's in-project file activity
- `darc query session-bundle ...` to fetch one session summary, its turn detail, and its touched files in one call
- `darc query sessions --touched-path ...` to narrow the session list to work that touched a path of interest
- `darc query resolve-session ...` to expand a UUID prefix before calling any session-scoped query command that requires a full `--session-id`

Before drafting or reviewing decision-trace proposals, check the existing wiki coverage first:

- `darc query wiki entries --grep ...` to find prior entries by title/body/domain language
- `darc query wiki entries --evidence-ref ...` to see whether a specific supporting turn is already cited
- `darc query wiki entries --covers-session ...` to find entries that already cover any turn from a candidate session
- combine `--evidence-ref` and `--covers-session` when you want the union of exact-turn and session-level overlap checks

These query commands ship today for human operators and external clients. The digest worker now uses this read-side
surface directly at runtime instead of relying on a pre-baked narrative artifact.

## Starting A Digest

`darc wiki digest start` creates a new run, snapshots the request artifact, spawns a background worker,
and returns either human-readable status or a machine-readable JSON envelope.

Example:

```bash
darc wiki digest start \
  --root ~/.darc \
  --project-id repo-abc123 \
  --session-ref claude:session-1 \
  --session-ref claude:session-2 \
  --agent claude \
  --runtime external-cli \
  --model claude-sonnet-4-6 \
  --target-category product \
  --target-domain query-protocol \
  --json
```

Key flags:

- `--session-ref <provider>:<session-id>` selects one archived session as a focus seed. Pass it more than once to build a multi-session digest.
- selected seed sessions guide the runtime's initial investigation, but they are not a hard evidence boundary. The runtime may still inspect and cite non-seed sessions through `darc query ...` when they support the same decision trace.
- `--agent <codex|claude>` selects the agent family. `codex` is disabled by default until documented MCP-isolation controls exist; set `DARC_WIKI_UNSAFE_ENABLE_CODEX=1` to opt in at your own risk.
- `--runtime external-cli` selects the currently supported runtime kind.
- `--model <name>` is required and is forwarded to the external CLI.
- `--auth-profile <name>` is optional metadata recorded in run artifacts. It does not currently select credentials, accounts, or billing budgets.
- `--target-category <name>` prioritizes an existing registry category and must already exist in the project registry.
- `--target-domain <slug>` prioritizes a project-scoped registry domain and must already exist in the project registry.
- `--json` emits the `darc.wiki.digest.start.v1` envelope on stdout.

## Monitoring And Canceling

List run state through the read-side query surface:

```bash
darc query wiki runs \
  --root ~/.darc \
  --project-id repo-abc123 \
  --json
```

Inspect one run plus parsed terminal result detail:

```bash
darc query wiki run \
  --root ~/.darc \
  --project-id repo-abc123 \
  --run-id cwrun_0123456789abcdef \
  --json
```

Cancel a run by `run_id`:

```bash
darc wiki digest cancel \
  --root ~/.darc \
  --project-id repo-abc123 \
  --run-id cwrun_0123456789abcdef \
  --json
```

The cancel command returns the `darc.wiki.digest.cancel.v1` envelope when `--json` is set.

## Managing Entry Status

Discard one canonical entry without deleting its Markdown artifact:

```bash
darc wiki entry discard \
  --root ~/.darc \
  --project-id repo-abc123 \
  --entry-id cw_0123456789abcdef \
  --json
```

Restore one discarded entry back to `active`:

```bash
darc wiki entry restore \
  --root ~/.darc \
  --project-id repo-abc123 \
  --entry-id cw_0123456789abcdef \
  --json
```

These commands mutate the canonical entry frontmatter in place, preserve the Markdown body, and update the
entry `status` plus `updated_at` fields. Restore rejects the request when another active entry already occupies
the same canonical identity, which prevents duplicate active decision traces after a later digest recreated the
discarded idea as a new entry.

## Runtime Requirements

The current MVP uses external CLIs already installed on the host machine.

- `--agent claude` expects the `claude` CLI.
- `--agent codex` expects the `codex` CLI, but the digest runtime is disabled by default until documented MCP-isolation controls exist. Set `DARC_WIKI_UNSAFE_ENABLE_CODEX=1` only when you intentionally want the ungated Codex path for manual testing.
- Set `DARC_WIKI_CODEX_BIN` to override the Codex executable path.
- Set `DARC_WIKI_CLAUDE_BIN` to override the Claude executable path.
- Digest workers invoke these CLIs in a background process and capture stdout/stderr into run-local log files.

Current auth caveat:

- Darc currently launches these CLIs with inherited host environment and ambient login state.
- `auth_profile` is metadata only today. It does not enforce profile selection or sandbox credentials.
- Codex remains gated because `codex exec` does not yet expose documented MCP-isolation controls for this workflow.

### Auth And Billing Behavior

- `darc wiki digest` does not provide its own token budget, billing account, or separate runtime identity.
- Each digest run uses whatever auth context the underlying CLI is already using on the host machine.
- `--auth-profile` is metadata only today. It does not switch accounts, credentials, or billing budgets.
- For `--agent claude`:
  - when bare-compatible auth is available, Darc runs Claude with `--bare`
  - bare-compatible auth currently means one of: `ANTHROPIC_API_KEY`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`, or `CLAUDE_CODE_USE_FOUNDRY`
  - otherwise, Darc falls back to the normal Claude Code CLI path without `--bare`, and usage follows the machine's current Claude CLI login/auth context
- For `--agent codex`:
  - usage follows the current Codex CLI auth context on the machine
  - Darc does not currently select a separate Codex account or billing budget

## Run Artifacts

Each run lives under:

```text
~/.darc/context-wiki/projects/<project-id>/runs/<run-id>/
```

Important files:

- `run.toml`: durable lifecycle state, selected sessions, runtime metadata, progress, and terminal error fields
- `request.json`: original digest request payload
- `proposal.json`: captured structured proposal artifact when one is produced
- `result.json`: terminal runtime and validation summary
- `events.jsonl`: progress and warning events emitted by the worker
- `agent.stdout.log`: captured runtime stdout
- `agent.stderr.log`: captured runtime stderr
- `cancel.flag`: cancellation signal written by `darc wiki digest cancel`

Shared runtime files under `~/.darc/context-wiki/`:

- `proposal.schema.v1.json`: shared JSON Schema artifact recorded in `run.toml` as `proposal_schema_path`; Codex consumes the file path directly, while Claude receives the equivalent schema JSON inline

## Current Success Semantics

Currently, a `succeeded` run means:

- the runtime request and shared proposal schema artifacts were prepared successfully
- the external runtime exited successfully
- the returned JSON matched Darc's proposal contract
- proposal validation passed
- canonical decision-trace markdown was merged or updated under `entries/`
- one digest markdown report was written under `digests/`
- `created_entry_ids`, `updated_entry_ids`, and `digest_id` were persisted into `run.toml`
- `result.json` was written after canonical artifact merge completed

A successful run may still produce zero decision-trace entries when the validated proposal finds no durable
decisions worth preserving. In that case Darc still writes the digest report and terminal run artifacts.

## Proposal Rules

The worker instructs the runtime to return exactly one JSON object matching Darc's schema.

Current validation rules include:

- `schema` must be `darc.wiki.digest.proposal.v1`
- `project_id` and `run_id` must match the current run
- only `decision_trace` entries are allowed
- only `create` operations are allowed
- categories must come from the project registry
- domains must come from registry domains
- `--target-domain` only prioritizes existing registry domains for the run
- evidence references must use `<provider>:<session-id>#<turn-ordinal>` and resolve to indexed turns for the current project
- zero proposed entries is valid when the inspected evidence does not contain durable decisions worth preserving

## Current Limitations

- Context Wiki imperative workflows are experimental and may still change.
- `darc query wiki ...` is the stable read-side contract; imperative `darc wiki ...` behavior is still MVP-stage.
- The Codex digest runtime is not normal supported behavior yet; use the explicit unsafe opt-in only for controlled manual testing.
- Read-side wiki queries do not expose internal artifact paths; use `darc query wiki run ... --json` for run/result detail and inspect the run directory directly only when you need raw logs.
