#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/bench-share-export.sh [--sessions N] [--mode lfs|plain|both] [--root DARC_ROOT] [--darc DARC_BIN]

Benchmarks Darc share export/push with a temporary Darc root copied from the
current user's real Darc config and index. Run from the Darc repository root.

Outputs one JSON object per benchmark mode.
USAGE
}

sessions=10
mode=both
darc_root="${HOME}/.darc"
darc_bin="./target/release/darc"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --sessions)
      sessions="$2"
      shift 2
      ;;
    --mode)
      mode="$2"
      shift 2
      ;;
    --root)
      darc_root="$2"
      shift 2
      ;;
    --darc)
      darc_bin="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

case "$mode" in
  lfs|plain|both) ;;
  *)
    echo "--mode must be lfs, plain, or both" >&2
    exit 2
    ;;
esac

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

if [[ ! -x "$darc_bin" ]]; then
  echo "Darc binary not found or not executable: $darc_bin" >&2
  exit 2
fi

if [[ ! -f "$darc_root/config.toml" || ! -f "$darc_root/index.sqlite" ]]; then
  echo "Darc root must contain config.toml and index.sqlite: $darc_root" >&2
  exit 2
fi

workdir="$(mktemp -d "${TMPDIR:-/tmp}/darc-share-bench.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

prepare_root() {
  local target_root="$1"
  rm -rf "$target_root"
  mkdir -p "$target_root"
  cp "$darc_root/config.toml" "$target_root/config.toml"
  cp "$darc_root/index.sqlite" "$target_root/index.sqlite"
  python3 - "$target_root" "$darc_root" <<'PY'
from pathlib import Path
import sys

target = Path(sys.argv[1])
source = Path(sys.argv[2]).expanduser()
config = target / "config.toml"
text = config.read_text()
text = text.replace(f'root = "{source}"', f'root = "{target}"')
text = text.replace(str(source / "projects"), str(target / "projects"))
config.write_text(text)
PY
}

select_sessions() {
  local root="$1"
  local limit="$2"
  "$darc_bin" list sessions --root "$root" --limit 10000 \
    | python3 -c 'import json,re,sys
limit = int(sys.argv[1])
sessions = json.load(sys.stdin)["data"]["sessions"]
uuid = re.compile(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
session_ids = [session["session_id"] for session in sessions]
selected = 0
for session_id in session_ids:
    if not uuid.match(session_id):
        continue
    if any(other != session_id and other.startswith(session_id + "/") for other in session_ids):
        continue
    print(session_id)
    selected += 1
    if selected >= limit:
        break
' "$limit"
}

run_one() {
  local run_mode="$1"
  local root="$workdir/root-$run_mode"
  local remote="$workdir/share-$run_mode.git"
  local branch="bench-$run_mode-$(date +%Y%m%d%H%M%S)"
  local started ended duration_ms push_output

  prepare_root "$root"
  git init --bare "$remote" >/dev/null
  "$darc_bin" remote --root "$root" add bench "$remote" >/dev/null
  local recipient
  recipient="$("$darc_bin" share --root "$root" key | sed -n 's/^public_key=//p')"
  "$darc_bin" share recipient add --root "$root" "$recipient" >/dev/null

  while IFS= read -r session_id; do
    [[ -n "$session_id" ]] || continue
    "$darc_bin" share include --root "$root" "$session_id" >/dev/null
  done < <(select_sessions "$root" "$sessions")

  started="$(python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
)"
  if [[ "$run_mode" == "plain" ]]; then
    push_output="$(DARC_SHARE_DISABLE_LFS=1 "$darc_bin" push "$branch" --remote bench --root "$root")"
  else
    push_output="$("$darc_bin" push "$branch" --remote bench --root "$root")"
  fi
  ended="$(python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
)"
  duration_ms=$((ended - started))

  python3 - "$run_mode" "$sessions" "$duration_ms" "$remote" "$root" "$push_output" <<'PY'
import json
import os
from pathlib import Path
import subprocess
import sys

mode, sessions, duration_ms, remote, root, push_output = sys.argv[1:]
def du_kib(path):
    path = Path(path)
    if not path.exists():
        return 0
    return int(subprocess.check_output(["du", "-sk", str(path)], text=True).split()[0])

def file_size_kib(paths):
    return sum(path.stat().st_size for path in paths if path.is_file()) // 1024

def manifest_chunk_count(root):
    count = 0
    for manifest in Path(root).glob("share-cache/**/darc-share/v1/exporters/*/manifest.json"):
        try:
            count += len(json.loads(manifest.read_text()).get("chunks", []))
        except (OSError, json.JSONDecodeError):
            pass
    return count

share_objects = list(Path(root).glob("share-cache/**/darc-share/v1/objects/*.age"))
lfs_objects = list((Path(remote) / "lfs" / "objects").glob("**/*"))
git_lfs_available = subprocess.run(
    ["git", "lfs", "version"],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    check=False,
).returncode == 0

print(json.dumps({
    "mode": mode,
    "session_limit": int(sessions),
    "duration_ms": int(duration_ms),
    "remote_kib": du_kib(remote),
    "remote_lfs_kib": du_kib(Path(remote) / "lfs" / "objects"),
    "share_cache_kib": du_kib(os.path.join(root, "share-cache")),
    "encrypted_object_count": len(share_objects),
    "encrypted_object_kib": file_size_kib(share_objects),
    "encrypted_chunk_count": manifest_chunk_count(root),
    "remote_lfs_file_count": sum(1 for path in lfs_objects if path.is_file()),
    "git_lfs_available": git_lfs_available,
    "push_output": push_output,
}, sort_keys=True))
PY
}

case "$mode" in
  lfs) run_one lfs ;;
  plain) run_one plain ;;
  both)
    run_one plain
    run_one lfs
    ;;
esac
