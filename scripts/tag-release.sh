#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf 'usage: %s <version>\n' "${0##*/}" >&2
  printf 'example: %s 0.1.0\n' "${0##*/}" >&2
}

fail() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

validate_version() {
  local version="$1"
  local semver='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z][0-9A-Za-z.-]*)?(\+[0-9A-Za-z][0-9A-Za-z.-]*)?$'

  [[ -n "$version" ]] || fail "missing release version"
  [[ "$version" != v* ]] || fail "pass the version without a leading v"
  [[ "$version" =~ $semver ]] || fail "version must look like 0.1.0 or 0.1.0-rc.1"
}

require_clean_worktree() {
  if [[ -n "$(git status --porcelain)" ]]; then
    fail "working tree must be clean before tagging a release"
  fi
}

workspace_version() {
  awk '
    /^\[workspace\.package\]$/ { in_workspace = 1; next }
    /^\[/ { in_workspace = 0 }
    in_workspace && /^[[:space:]]*version[[:space:]]*=/ {
      line = $0
      sub(/^[^\"]*\"/, "", line)
      sub(/\".*$/, "", line)
      print line
      exit
    }
  ' Cargo.toml
}

tag_exists() {
  local tag="$1"
  git rev-parse -q --verify "refs/tags/$tag" >/dev/null 2>&1 ||
    git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1
}

[[ $# -eq 1 ]] || {
  usage
  exit 64
}

version="$1"
validate_version "$version"
tag="v$version"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || fail "not inside a git repository"
cd "$repo_root"

[[ -f Cargo.toml ]] || fail "missing Cargo.toml"
[[ -f CHANGELOG.md ]] || fail "missing CHANGELOG.md"
git remote get-url origin >/dev/null 2>&1 || fail "missing origin remote"
require_clean_worktree

git fetch origin main:refs/remotes/origin/main --tags

if tag_exists "$tag"; then
  fail "tag $tag already exists locally or on origin"
fi

if git show-ref --verify --quiet refs/heads/main; then
  git checkout main
else
  git checkout -b main origin/main
fi

git pull --ff-only origin main
require_clean_worktree

local_head="$(git rev-parse HEAD)"
origin_head="$(git rev-parse refs/remotes/origin/main)"
[[ "$local_head" == "$origin_head" ]] || fail "local main does not match origin/main"

actual_version="$(workspace_version)"
[[ -n "$actual_version" ]] || fail "Cargo.toml missing [workspace.package] version"
[[ "$actual_version" == "$version" ]] ||
  fail "Cargo.toml version is $actual_version, expected $version"

awk -v prefix="## [$version] - " '
  index($0, prefix) == 1 && length($0) == length(prefix) + 10 {
    date = substr($0, length(prefix) + 1)
    if (date ~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/) found = 1
  }
  END { exit !found }
' CHANGELOG.md || fail "CHANGELOG.md missing ## [$version] - YYYY-MM-DD section"

git tag -a "$tag" -m "$tag"
if ! git push origin "$tag"; then
  git tag -d "$tag" >/dev/null 2>&1 || true
  fail "failed to push $tag to origin"
fi

printf 'Tagged and pushed %s from main.\n' "$tag"
