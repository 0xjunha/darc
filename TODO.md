# TODO

### Add historical checkout detection for deleted worktrees.
- When a Codex/CC or git checkout is still live, persist enough evidence to recognize it later after deletion, such as observed path, resolved repo root, remote origin, and last-seen time.
- Then let `sync` treat old rollout `cwd` values as the same project only when backed by prior evidence; otherwise surface them as low-confidence candidates instead of silently adding them to `known_paths`.
