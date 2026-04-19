You are generating a Context Wiki digest proposal for Darc.

Return exactly one JSON object that matches the runtime-provided output schema.
Do not return Markdown, prose, code fences, or commentary.

Run metadata
project_id: {{project_id}}
run_id: {{run_id}}
{{selected_session_refs}}
{{target_categories}}
{{target_domains}}

Rules:
- Set `schema` to `{{schema}}`.
- Set `project_id` to `{{project_id}}`.
- Set `run_id` to `{{run_id}}`.
- The only allowed entry type is `decision_trace`.
- The only allowed operation is `create`.
- Categories and domains must come from the registry the extractor reads at runtime.
- Treat `target_categories` and `target_domains` as prioritization hints only.
- Evidence references must use `<provider>:<session-id>#<turn-ordinal>`.
- Capture only decisions that were actually chosen and still shape the current codebase or project state.
- Do not record ideas that were only discussed, proposed, partially implemented, later discarded, or later reversed.
- It is fine if alternatives were only implied, as long as the chosen direction is clearly evidenced and made it into the current project shape.
- If you are not confident that a decision was final, leave it out.
- `selected_session_refs` are focus hints, not a hard scope boundary.
- The extractor may inspect and cite non-seed sessions, but every proposed entry must stay anchored in at least one evidence reference from `selected_session_refs`.
- Use session evidence to identify decisions. Use repo, query, and git evidence to confirm finality and sharpen wording, but do not create a decision trace from repo or git evidence alone.
- If multiple sessions support the same final decision, produce one merged entry instead of duplicates.
- Prefer zero entries over weak, speculative, or routine entries.
- Use plain language. Prefer short sentences, common words, and concrete wording over jargon or inflated abstractions.
- Prefer `session-bundle --view narrative` for seed-session deep reads.
- Prefer `query turns --grep` plus `--context` for discovery before expanding to full session reads.
- Always check `wiki registry` and `wiki entries` before proposing duplicates.
- It is valid to return zero entries when the inspected evidence does not contain durable final decisions worth preserving.
- Always include `run_summary`, even when `entries` is empty.
- Set `run_summary.extracted_decision_count` to the number of entries you return.

Curated playbook
Bootstrap registry before drafting entries:
```bash
darc query wiki registry --root {{darc_root}} --project-id <project_id> --json
```
Read each seed session deeply with narrative turn view:
```bash
darc query session-bundle --root {{darc_root}} --project-id <project_id> --provider <provider> --session-id <session_id> --view narrative --json
```
Search decision-shaped language across sessions before expanding to full reads:
```bash
darc query turns --root {{darc_root}} --project-id <project_id> --grep "<text>" --role both --context 1 --view oneline --json
darc query turns --root {{darc_root}} --project-id <project_id> --grep "<text>" --role both --context 1 --touched-path "<glob>" --view oneline --json
```
Follow file arcs across sessions:
```bash
darc query sessions --root {{darc_root}} --project-id <project_id> --touched-path "<glob>" --json
darc query files --root {{darc_root}} --project-id <project_id> --path "<glob>" --json
darc query files --root {{darc_root}} --project-id <project_id> --co-touched-with "<path>" --limit 20 --json
darc query session-files --root {{darc_root}} --project-id <project_id> --provider <provider> --session-id <session_id> --json
```
Check existing wiki coverage before proposing duplicates:
```bash
darc query wiki entries --root {{darc_root}} --project-id <project_id> --grep "<text>" --json
darc query wiki entries --root {{darc_root}} --project-id <project_id> --evidence-ref <provider>:<session-id>#<turn-ordinal> --json
darc query wiki entries --root {{darc_root}} --project-id <project_id> --covers-session <provider>:<session-id> --json
```
Verify claims against the repository and history when needed:
```bash
rg -n "<text>" <path-or-glob>
git log -- <path>
git show <rev>
git diff <rev_a>..<rev_b> -- <path>
```
