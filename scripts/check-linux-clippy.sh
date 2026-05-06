#!/usr/bin/env sh
set -eu

# Runs the GitHub-style Linux clippy gate inside Docker.
# Override the image or platform when needed:
#   DARC_LINUX_CLIPPY_IMAGE=rust:1-bookworm scripts/check-linux-clippy.sh
#   DARC_LINUX_CLIPPY_PLATFORM=linux/amd64 scripts/check-linux-clippy.sh

fail() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

command -v docker >/dev/null 2>&1 || fail "docker is required"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || fail "not inside a git repository"
image="${DARC_LINUX_CLIPPY_IMAGE:-rust:1-bookworm}"
target_dir="${DARC_LINUX_CLIPPY_TARGET_DIR:-/tmp/darc-target}"

set -- --rm
if [ -n "${DARC_LINUX_CLIPPY_PLATFORM:-}" ]; then
  set -- "$@" --platform "$DARC_LINUX_CLIPPY_PLATFORM"
fi

docker run "$@" \
  -v "$repo_root:/work" \
  -w /work \
  -e CARGO_TARGET_DIR="$target_dir" \
  -e LC_ALL=C \
  -e LANG=C \
  "$image" \
  sh -c 'set -eu
if ! cargo clippy -V >/dev/null 2>&1; then
  rustup component add clippy
fi
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings -W clippy::all'
