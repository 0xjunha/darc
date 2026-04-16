# Context Wiki

Darc's Context Wiki is an experimental backend-owned workflow for durable project knowledge stored under
`~/.darc/context-wiki/`.

Current MVP scope:

- read-side wiki queries via `darc query wiki ... --json`
- imperative digest lifecycle via `darc wiki digest start` and `darc wiki digest cancel`
- imperative entry lifecycle via `darc wiki entry discard` and `darc wiki entry restore`
- external CLI runtimes for `codex` and `claude`
- structured `decision_trace` proposal validation
- canonical merge into decision-trace entry markdown plus one digest report per successful run
- durable run artifacts and logs for each digest run

Current gaps:

- `auth_profile` is recorded as run metadata only and does not constrain runtime credentials

## Read Side

Use `darc query wiki ... --json` to inspect Context Wiki state without invoking a runtime.

- `darc query wiki registry --root <path> --project-id <id> --json`
- `darc query wiki entries --root <path> --project-id <id> --json`
- `darc query wiki entries --root <path> --project-id <id> --category <id> --domain <id> --status active --json`
- `darc query wiki entry --root <path> --project-id <id> --entry-id <id> --json`
- `darc query wiki digests --root <path> --project-id <id> --json`
- `darc query wiki digests --root <path> --project-id <id> --limit 50 --json`
- `darc query wiki digest --root <path> --project-id <id> --digest-id <id> --json`
- `darc query wiki run --root <path> --project-id <id> --run-id <id> --json`
- `darc query wiki runs --root <path> --project-id <id> --status running --limit 50 --json`

The query protocol remains the machine-readable contract for desktop and other clients. See
[Query protocol](query-protocol.md).

For history investigation and proposal preparation, prefer the project-scoped read surface over ad hoc SQLite reads:

- `darc query turns --grep ...` to find load-bearing turns by content with optional role and context filters
- `darc query files --path ...` to pivot from one file path to the sessions that touched it
- `darc query files --co-touched-with ...` to discover nearby files often touched in the same sessions
- `darc query session-files ...` to inspect one session's in-project file activity
- `darc query sessions --touched-path ...` to narrow the session list to work that touched a path of interest

## Starting A Digest

`darc wiki digest start` creates a new run, snapshots the request and context artifacts, spawns a background worker,
and returns either human-readable status or a machine-readable JSON envelope.

Example:

```bash
darc wiki digest start \
  --root ~/.darc \
  --project-id repo-abc123 \
  --session-ref codex:session-1 \
  --session-ref claude:session-2 \
  --agent codex \
  --runtime external-cli \
  --model gpt-5.4 \
  --target-category product \
  --target-domain query-protocol \
  --json
```

Key flags:

- `--session-ref <provider>:<session-id>` selects one archived session. Pass it more than once to build a multi-session digest.
- `--agent <codex|claude>` selects the agent family.
- `--runtime external-cli` selects the currently supported runtime kind.
- `--model <name>` is required and is forwarded to the external CLI.
- `--auth-profile <name>` is optional metadata recorded in run artifacts. It does not currently select credentials.
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

- `--agent codex` expects the `codex` CLI.
- `--agent claude` expects the `claude` CLI.
- Set `DARC_WIKI_CODEX_BIN` to override the Codex executable path.
- Set `DARC_WIKI_CLAUDE_BIN` to override the Claude executable path.
- Digest workers invoke these CLIs in a background process and capture stdout/stderr into run-local log files.

Current auth caveat:

- Darc currently launches these CLIs with inherited host environment and ambient login state.
- `auth_profile` is metadata only today. It does not enforce profile selection or sandbox credentials.

## Run Artifacts

Each run lives under:

```text
~/.darc/context-wiki/projects/<project-id>/runs/<run-id>/
```

Important files:

- `run.toml`: durable lifecycle state, selected sessions, runtime metadata, progress, and terminal error fields
- `request.json`: original digest request payload
- `context.json`: assembled registry and narrative-turn context given to the runtime
- `proposal.schema.json`: JSON Schema supplied to the runtime
- `proposal.json`: captured structured proposal artifact when one is produced
- `result.json`: terminal runtime and validation summary
- `events.jsonl`: progress and warning events emitted by the worker
- `agent.stdout.log`: captured runtime stdout
- `agent.stderr.log`: captured runtime stderr
- `cancel.flag`: cancellation signal written by `darc wiki digest cancel`

## Current Success Semantics

Currently, a `succeeded` run means:

- the selected session context was assembled successfully
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
- evidence references must use `<provider>:<session-id>#<turn-ordinal>` and only reference selected sessions
- zero proposed entries is valid when the selected sessions contain no durable decisions worth preserving

## Current Limitations

- Context Wiki imperative workflows are experimental and may still change.
- `darc query wiki ...` is the stable read-side contract; imperative `darc wiki ...` behavior is still MVP-stage.
- Read-side wiki queries do not expose internal artifact paths; use `darc query wiki run ... --json` for run/result detail and inspect the run directory directly only when you need raw logs.
