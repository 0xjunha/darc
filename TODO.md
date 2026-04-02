# TODO

### Add historical checkout detection for deleted worktrees.
- Persist enough evidence for live Codex/CC or git checkouts to recognize the same project after deletion, including observed path, resolved repo root, remote origin, and last-seen time.
- Make `sync` treat old rollout `cwd` values as the same project only when backed by prior evidence; otherwise surface them as low-confidence candidates instead of silently adding them to `known_paths`.

### Collapse changed-rollout parsing to one pass.
- In `crates/core/src/rollout/codex/mod.rs`, derive user-turn boundaries during streaming instead of the current pre-scan.
- Preserve current `event_msg.user_message` vs `response_item.role == "user"` behavior.
- Keep parser memory bounded to the current turn.
- Leave incremental reindexing in `crates/core/src/parse.rs` unchanged.

### Tighten unchanged-rollout detection before skipping reparse.
- Current parse skip detection trusts `archive_path`, file size, and mtime.
- If rollout contents become corrupt without changing those values, parse can incorrectly skip reparsing and keep stale indexed data.
- Consider storing or deriving a stronger content identity for changed-rollout detection.
