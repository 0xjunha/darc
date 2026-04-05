Derived Claude rollout fixtures checked into the repo for parser regression tests.

- `historical/*.jsonl` are minimized real parent-session fixtures covering the older parser epoch
  families from `1.0.88` through `2.1.83`.
- `errors/*.jsonl` are minimized real failure-state parent-session fixtures.
- `subagents/*.jsonl` are minimized real Claude subagent-session fixtures.
- `modern/2.1.89-subagent-task-parent.jsonl` is a minimized real parent-session transcript from
  the Claude schema audit workspace before the `attachment` drift.
- `modern/2.1.90-subagent-task-parent.jsonl` is the matching minimized real parent-session
  transcript after the `attachment` drift.

These fixtures keep the important rollout structure while replacing volatile ids and paths with
stable test values.
