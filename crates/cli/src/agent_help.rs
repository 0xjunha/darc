use super::*;

/// Prints the selected agent-facing Darc usage surface.
pub(crate) fn run_agent_help(args: AgentHelpArgs) -> Result<()> {
    if args.agents_md_line {
        println!("{}", render_agents_md_line());
    } else {
        print!("{}", render_agent_help_guide());
    }
    Ok(())
}

/// Renders the marker-wrapped one-line AGENTS.md guidance.
pub(crate) fn render_agents_md_line() -> String {
    format!(
        "{AGENTS_MD_GUIDANCE_START_MARKER} {AGENTS_MD_GUIDANCE_TEXT} {AGENTS_MD_GUIDANCE_END_MARKER}"
    )
}

/// Renders concise operating guidance for agents using Darc.
pub(crate) fn render_agent_help_guide() -> &'static str {
    r#"# Darc Agent Help

Darc is a local archive and query layer for coding-agent sessions. Use it to recover prior-session evidence, not as a replacement for reading the current repository, docs, or tests.

## When to Use Darc

Use Darc as a primary evidence source for:
- prior decisions or rationale
- regressions and repeated failures
- PR handoffs or unfinished work
- ambiguous user references to earlier work
- file, module, or command history across past sessions

Use Darc only as orientation for current-code audits, refactor planning, or implementation work. In those cases, let Darc point you toward likely context, then verify everything against current source and tests.

Skip Darc for fresh, self-contained tasks where current files, tests, docs, or the user prompt are enough.

## Safe First Commands

- `darc status --json`: preflight active-project resolution and index freshness.
- `darc search <query> --limit 5`: find matching turns by keyword.
- `darc search --mode literal --query <text> --limit 5`: find exact text without regex escaping.
- `darc list sessions --limit 5`: browse recent indexed sessions.
- `darc list files --limit 10`: rank files touched in prior sessions.
- `darc list sessions --touching <glob> --limit 5`: find sessions that touched a file or path pattern.
- `darc search --mode file-path <glob> --limit 5`: find turns associated with touched file paths.

## Task Recipes

Decision or rationale:
- `darc search --mode literal --query "<decision phrase>" --limit 5`
- `darc show turn <SESSION_ID> <TURN_ORDINAL> --step-limit 10`

Regression or repeated failure:
- `darc search --mode literal --query "<error text>" --limit 5`
- `darc list sessions --touching <path> --limit 5`

File or module history:
- `darc list sessions --touching <path> --limit 5`
- `darc list files --co-touched-with <path> --limit 10`

Hotspot orientation:
- `darc list files --limit 25`
- Treat historical churn as a map, not a verdict.

## Evidence Ladder

1. Start with bounded `search`, `list`, or `stats` reads.
2. Keep the returned `session_id` and `turn_ordinal` handles.
3. Skim one candidate session with `darc list turns <SESSION_ID> --view oneline --limit 20`.
4. Inspect exact evidence with `darc show turn <SESSION_ID> <TURN_ORDINAL> --step-limit 10`.
5. Use `darc show session <SESSION_ID> --turn-limit 5 --step-limit 10` only after narrowing.

## Reporting Darc Evidence

When Darc materially shapes your answer, briefly separate prior-session evidence from current verification:

- Darc showed: the relevant prior decision, session evidence, or file/history pattern.
- Current source/tests confirmed: what is true in the repository now.

When exact prior evidence matters, include the `session_id` and `turn_ordinal` so another agent can inspect the same turn. Do not list Darc commands unless they help reproduce the evidence.

## Output Discipline

- Prefer small `--limit`, `--turn-limit`, and `--step-limit` values first.
- Prefer `darc show turn` for exact evidence and `darc show session` for bounded broader context.
- Use `--color never` when piping JSON to `jq` or another parser.
- Do not treat high historical churn as proof of bad code.

## Mutating Boundaries

Read surfaces such as `status --json`, `list`, `show`, `search`, `stats`, and `resolve` are safe for investigation.

`darc refresh`, `darc sync`, `darc index`, `darc init`, `darc project ...`, and service/upgrade commands can write local Darc state or config. Run them only when freshness or setup is part of the task.
"#
}
