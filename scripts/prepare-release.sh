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
    fail "working tree must be clean before preparing a release"
  fi
}

run() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  "$@"
}

[[ $# -eq 1 ]] || {
  usage
  exit 64
}

version="$1"
validate_version "$version"
tag="v$version"
release_date="${DARC_RELEASE_DATE:-$(date -u +%F)}"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || fail "not inside a git repository"
cd "$repo_root"

[[ -f Cargo.toml ]] || fail "missing Cargo.toml"
[[ -f CHANGELOG.md ]] || fail "missing CHANGELOG.md"
require_clean_worktree

tmp_cargo=""
tmp_changelog=""
cleanup() {
  if [[ -n "${tmp_cargo:-}" && -f "$tmp_cargo" ]]; then
    rm -f "$tmp_cargo"
  fi
  if [[ -n "${tmp_changelog:-}" && -f "$tmp_changelog" ]]; then
    rm -f "$tmp_changelog"
  fi
  return 0
}
trap cleanup EXIT

tmp_cargo="$(mktemp "${TMPDIR:-/tmp}/darc-release-cargo.XXXXXX")"
tmp_changelog="$(mktemp "${TMPDIR:-/tmp}/darc-release-changelog.XXXXXX")"

LC_ALL=C DARC_RELEASE_VERSION="$version" perl -0 -e '
my $version = $ENV{"DARC_RELEASE_VERSION"} // die "missing DARC_RELEASE_VERSION\n";
my $text = do { local $/; <> };
my $changed = 0;

$text =~ s{(^\[workspace\.package\][^\n]*\n)(.*?)(?=^\[|\z)}{
    my ($header, $body) = ($1, $2);
    if ($body =~ s/^version\s*=\s*"[^"]*"/version = "$version"/m) {
        $changed = 1;
    }
    "$header$body";
}egms;

die "Cargo.toml missing [workspace.package] version\n" unless $changed;
print $text;
' Cargo.toml > "$tmp_cargo"

LC_ALL=C DARC_RELEASE_VERSION="$version" DARC_RELEASE_DATE="$release_date" perl -0 -e '
my $version = $ENV{"DARC_RELEASE_VERSION"} // die "missing DARC_RELEASE_VERSION\n";
my $release_date = $ENV{"DARC_RELEASE_DATE"} // die "missing DARC_RELEASE_DATE\n";
my $text = do { local $/; <> };
die "CHANGELOG.md already has a section for $version\n" if $text =~ /^##\s+\[?\Q$version\E\]?(?:\s|\z)/m;
die "release date must use YYYY-MM-DD\n" unless $release_date =~ /^\d{4}-\d{2}-\d{2}$/;

my $changed = 0;
$text =~ s{(^##\s+Unreleased[^\n]*\n)(.*?)(?=^##\s+|\z)}{
    my ($header, $body) = ($1, $2);
    $body =~ s/\A[ \t\r\n]+//;
    $body =~ s/[ \t\r\n]+\z//;
    die "CHANGELOG.md Unreleased section is empty\n" unless length $body;
    $changed = 1;
    "$header\n## [$version] - $release_date\n\n$body\n\n";
}egms;

die "CHANGELOG.md missing ## Unreleased section\n" unless $changed;
print $text;
' CHANGELOG.md > "$tmp_changelog"

mv "$tmp_cargo" Cargo.toml
tmp_cargo=""
mv "$tmp_changelog" CHANGELOG.md
tmp_changelog=""

# Refresh local workspace package versions before the locked validation gates.
run cargo +stable update --offline --workspace
run cargo +nightly fmt
run cargo +stable clippy --locked --workspace --all-targets --all-features -- -D warnings -W clippy::all
run cargo +stable test --locked --workspace
run cargo +stable check --locked --workspace --all-targets --all-features --profile dist
run dist plan --tag "$tag"

printf 'Prepared release %s.\n' "$tag"
printf 'Next: commit Cargo.toml, Cargo.lock, and CHANGELOG.md, open the release PR, merge it, then run scripts/tag-release.sh %s.\n' "$version"
