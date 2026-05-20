#!/usr/bin/env sh
set -eu

# Benchmarks Darc cold refresh or index rebuild with the real CLI binary.
#
# Defaults create a fresh temporary Darc root, run `darc init`, then time
# `darc refresh`. For redaction-equivalence checks, set DARC_BENCH_SNAPSHOT to
# write a compact snapshot digest, or DARC_BENCH_COMPARE to compare against one.
#
# Examples:
#   cargo build --release
#   scripts/bench-cold-refresh.sh
#   DARC_BENCH_MODE=index-rebuild DARC_BENCH_ROOT=/tmp/darc-root scripts/bench-cold-refresh.sh
#   DARC_BENCH_SNAPSHOT=/tmp/redaction.before scripts/bench-cold-refresh.sh
#   DARC_BENCH_COMPARE=/tmp/redaction.before scripts/bench-cold-refresh.sh

DARC_BIN="${DARC_BIN:-$(pwd)/target/release/darc}"
DARC_BENCH_MODE="${DARC_BENCH_MODE:-cold-refresh}"
DARC_BENCH_SNAPSHOT="${DARC_BENCH_SNAPSHOT:-}"
DARC_BENCH_COMPARE="${DARC_BENCH_COMPARE:-}"
TIME_BIN="${TIME_BIN:-/usr/bin/time}"

if [ -z "${DARC_BENCH_ROOT:-}" ]; then
  DARC_BENCH_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/darc-cold-refresh.XXXXXX")"
fi

if [ ! -x "$DARC_BIN" ]; then
  echo "missing executable DARC_BIN: $DARC_BIN" >&2
  echo "run: cargo build --release" >&2
  exit 2
fi

if [ ! -x "$TIME_BIN" ]; then
  TIME_BIN=time
fi

count_table() {
  sqlite3 -readonly "$1" "SELECT COUNT(*) FROM $2;"
}

snapshot_digest() {
  sqlite3 -readonly -batch "$1" <<'SQL' | LC_ALL=C shasum -a 256 | LC_ALL=C awk '{print $1}'
.mode quote
SELECT 'sessions', project_id, provider, session_id, cwd FROM sessions ORDER BY project_id, provider, session_id;
SELECT 'turns', project_id, provider, session_id, turn_ordinal, user_message, ifnull(final_answer_text, ''), steps_json FROM turns ORDER BY project_id, provider, session_id, turn_ordinal;
SELECT 'turn_search', project_id, provider, session_id, turn_ordinal, user_message_text, final_answer_text, tool_text FROM turn_search ORDER BY project_id, provider, session_id, turn_ordinal;
SELECT 'tool_calls', project_id, provider, session_id, turn_ordinal, call_ordinal, call_id, ifnull(tool_name, ''), ifnull(arguments_text, ''), ifnull(output_text, ''), ifnull(status, '') FROM tool_calls ORDER BY project_id, provider, session_id, turn_ordinal, call_ordinal;
SELECT 'file_accesses', project_id, provider, session_id, turn_ordinal, call_ordinal, call_id, tool_name, access_type, path, ifnull(repo_relative_path, ''), ifnull(file_name, '') FROM file_accesses ORDER BY project_id, provider, session_id, turn_ordinal, call_ordinal, access_type, path;
SELECT 'turn_evidence', project_id, provider, session_id, turn_ordinal, evidence_ordinal, field, text FROM turn_evidence ORDER BY project_id, provider, session_id, turn_ordinal, evidence_ordinal;
SQL
}

write_snapshot() {
  db_path="$1"
  {
    echo "schema=darc.redaction.snapshot.v1"
    echo "sessions=$(count_table "$db_path" sessions)"
    echo "turns=$(count_table "$db_path" turns)"
    echo "tool_calls=$(count_table "$db_path" tool_calls)"
    echo "file_accesses=$(count_table "$db_path" file_accesses)"
    echo "turn_evidence=$(count_table "$db_path" turn_evidence)"
    echo "turn_search=$(count_table "$db_path" turn_search)"
    echo "sha256=$(snapshot_digest "$db_path")"
  }
}

echo "DARC_BIN=$DARC_BIN"
echo "DARC_BENCH_ROOT=$DARC_BENCH_ROOT"
echo "DARC_BENCH_MODE=$DARC_BENCH_MODE"

case "$DARC_BENCH_MODE" in
  cold-refresh)
    "$DARC_BIN" init --root "$DARC_BENCH_ROOT"
    "$TIME_BIN" -p "$DARC_BIN" refresh --root "$DARC_BENCH_ROOT"
    ;;
  index-rebuild)
    "$TIME_BIN" -p "$DARC_BIN" index --rebuild --root "$DARC_BENCH_ROOT"
    ;;
  *)
    echo "unsupported DARC_BENCH_MODE: $DARC_BENCH_MODE" >&2
    echo "expected: cold-refresh or index-rebuild" >&2
    exit 2
    ;;
esac

if [ -n "$DARC_BENCH_SNAPSHOT" ] || [ -n "$DARC_BENCH_COMPARE" ]; then
  snapshot_temp="$(mktemp "${TMPDIR:-/tmp}/darc-redaction-snapshot.XXXXXX")"
  write_snapshot "$DARC_BENCH_ROOT/index.sqlite" > "$snapshot_temp"

  if [ -n "$DARC_BENCH_SNAPSHOT" ]; then
    cp "$snapshot_temp" "$DARC_BENCH_SNAPSHOT"
    echo "snapshot=$DARC_BENCH_SNAPSHOT"
  fi

  if [ -n "$DARC_BENCH_COMPARE" ]; then
    if cmp -s "$DARC_BENCH_COMPARE" "$snapshot_temp"; then
      echo "snapshot_match=yes"
    else
      echo "snapshot_match=no"
      echo "expected=$DARC_BENCH_COMPARE" >&2
      echo "actual=$snapshot_temp" >&2
      exit 1
    fi
  fi
fi
