#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PREPARE_SCRIPT="$REPO_ROOT/scripts/prepare-release.sh"
TAG_SCRIPT="$REPO_ROOT/scripts/tag-release.sh"
TEST_TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/darc-release-script-tests.XXXXXX")"

cleanup() {
  rm -rf "$TEST_TMP_ROOT"
}
trap cleanup EXIT

fail() {
  printf 'test failure: %s\n' "$1" >&2
  exit 1
}

assert_contains() {
  local file="$1"
  local text="$2"
  grep -Fq -- "$text" "$file" || fail "expected $file to contain: $text"
}

assert_not_contains() {
  local file="$1"
  local text="$2"
  if grep -Fq -- "$text" "$file"; then
    fail "expected $file not to contain: $text"
  fi
}

assert_eq() {
  local expected="$1"
  local actual="$2"
  local label="$3"
  [[ "$actual" == "$expected" ]] || fail "$label: expected [$expected], got [$actual]"
}

assert_fails() {
  local output_file="$1"
  shift
  if "$@" >"$output_file" 2>&1; then
    cat "$output_file" >&2
    fail "expected command to fail: $*"
  fi
}

assert_unreleased_empty_before_next_section() {
  local changelog="$1"
  awk '
    $0 == "## Unreleased" { in_section = 1; next }
    in_section && /^## / { found_next = 1; exit }
    in_section && $0 !~ /^[[:space:]]*$/ { bad = 1; exit }
    END { exit (bad || !found_next) }
  ' "$changelog" || fail "expected Unreleased section in $changelog to be empty"
}

workspace_version() {
  local cargo_toml="$1"
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
  ' "$cargo_toml"
}

write_project_files() {
  local repo="$1"
  local version="$2"
  local unreleased_body="$3"

  cat >"$repo/Cargo.toml" <<EOF
[workspace]
members = []
resolver = "3"

[workspace.package]
version = "$version"
edition = "2024"
EOF

  cat >"$repo/CHANGELOG.md" <<EOF
# Changelog

All notable Darc release changes should be summarized here.

## Unreleased

$unreleased_body
EOF
}

write_release_changelog() {
  local repo="$1"
  local version="$2"

  cat >"$repo/CHANGELOG.md" <<EOF
# Changelog

All notable Darc release changes should be summarized here.

## Unreleased

## $version

- Add release automation.
EOF
}

init_repo() {
  local name="$1"
  local version="$2"
  local unreleased_body="$3"
  local base="$TEST_TMP_ROOT/$name"
  local remote="$base/origin.git"
  local work="$base/work"

  mkdir -p "$base"
  git init --bare "$remote" >/dev/null 2>&1
  git init "$work" >/dev/null 2>&1
  git -C "$work" checkout -b main >/dev/null 2>&1
  git -C "$work" config user.name "Darc Release Test"
  git -C "$work" config user.email "release-test@example.invalid"
  write_project_files "$work" "$version" "$unreleased_body"
  printf 'release test repo\n' >"$work/README.md"
  git -C "$work" add Cargo.toml CHANGELOG.md README.md
  git -C "$work" commit -m "chore: seed release test repo" >/dev/null
  git -C "$work" remote add origin "$remote"
  git -C "$work" push -u origin main >/dev/null 2>&1
  printf '%s\n' "$work"
}

make_fake_release_tools() {
  local fake_bin="$1"
  mkdir -p "$fake_bin"

  cat >"$fake_bin/cargo" <<'EOF'
#!/usr/bin/env sh
: "${DARC_RELEASE_TEST_LOG:?}"
printf 'cargo' >>"$DARC_RELEASE_TEST_LOG"
for arg in "$@"; do
  printf ' %s' "$arg" >>"$DARC_RELEASE_TEST_LOG"
done
printf '\n' >>"$DARC_RELEASE_TEST_LOG"
EOF

  cat >"$fake_bin/dist" <<'EOF'
#!/usr/bin/env sh
: "${DARC_RELEASE_TEST_LOG:?}"
printf 'dist' >>"$DARC_RELEASE_TEST_LOG"
for arg in "$@"; do
  printf ' %s' "$arg" >>"$DARC_RELEASE_TEST_LOG"
done
printf '\n' >>"$DARC_RELEASE_TEST_LOG"
EOF

  chmod +x "$fake_bin/cargo" "$fake_bin/dist"
}

prepare_success_updates_version_changelog_and_runs_checks() {
  local repo fake_bin log actual expected
  repo="$(init_repo prepare-success 0.1.0 "- Add release automation.")"
  fake_bin="$TEST_TMP_ROOT/prepare-success-bin"
  log="$TEST_TMP_ROOT/prepare-success.log"
  make_fake_release_tools "$fake_bin"

  (cd "$repo" && PATH="$fake_bin:$PATH" DARC_RELEASE_TEST_LOG="$log" "$PREPARE_SCRIPT" 0.2.0) >/dev/null

  assert_eq "0.2.0" "$(workspace_version "$repo/Cargo.toml")" "prepared Cargo.toml version"
  assert_contains "$repo/CHANGELOG.md" "## 0.2.0"
  assert_contains "$repo/CHANGELOG.md" "- Add release automation."
  assert_unreleased_empty_before_next_section "$repo/CHANGELOG.md"

  actual="$(cat "$log")"
  expected="cargo +nightly fmt
cargo clippy --workspace --all-targets --all-features -- -D warnings -W clippy::all
cargo test --workspace
dist plan --tag v0.2.0"
  assert_eq "$expected" "$actual" "release check command sequence"
}

prepare_rejects_leading_v_version() {
  local repo out
  repo="$(init_repo prepare-leading-v 0.1.0 "- Add release automation.")"
  out="$TEST_TMP_ROOT/prepare-leading-v.out"

  assert_fails "$out" bash -c "cd '$repo' && '$PREPARE_SCRIPT' v0.2.0"
  assert_contains "$out" "without a leading v"
  assert_eq "0.1.0" "$(workspace_version "$repo/Cargo.toml")" "Cargo.toml after rejected version"
}

prepare_rejects_dirty_worktree() {
  local repo fake_bin log out
  repo="$(init_repo prepare-dirty 0.1.0 "- Add release automation.")"
  fake_bin="$TEST_TMP_ROOT/prepare-dirty-bin"
  log="$TEST_TMP_ROOT/prepare-dirty.log"
  out="$TEST_TMP_ROOT/prepare-dirty.out"
  make_fake_release_tools "$fake_bin"
  printf 'dirty\n' >>"$repo/README.md"

  assert_fails "$out" bash -c "cd '$repo' && PATH='$fake_bin':\$PATH DARC_RELEASE_TEST_LOG='$log' '$PREPARE_SCRIPT' 0.2.0"
  assert_contains "$out" "working tree must be clean"
  assert_eq "0.1.0" "$(workspace_version "$repo/Cargo.toml")" "Cargo.toml after dirty rejection"
  [[ ! -f "$log" ]] || fail "prepare script should not run checks after dirty rejection"
}

prepare_rejects_empty_unreleased_without_partial_write() {
  local repo fake_bin log out
  repo="$(init_repo prepare-empty 0.1.0 "")"
  fake_bin="$TEST_TMP_ROOT/prepare-empty-bin"
  log="$TEST_TMP_ROOT/prepare-empty.log"
  out="$TEST_TMP_ROOT/prepare-empty.out"
  make_fake_release_tools "$fake_bin"

  assert_fails "$out" bash -c "cd '$repo' && PATH='$fake_bin':\$PATH DARC_RELEASE_TEST_LOG='$log' '$PREPARE_SCRIPT' 0.2.0"
  assert_contains "$out" "Unreleased section is empty"
  assert_eq "0.1.0" "$(workspace_version "$repo/Cargo.toml")" "Cargo.toml after empty changelog rejection"
  assert_not_contains "$repo/CHANGELOG.md" "## 0.2.0"
  [[ ! -f "$log" ]] || fail "prepare script should not run checks after changelog rejection"
}

prepare_rejects_existing_version_section() {
  local repo fake_bin log out
  repo="$(init_repo prepare-existing-section 0.1.0 "- Add release automation.")"
  fake_bin="$TEST_TMP_ROOT/prepare-existing-section-bin"
  log="$TEST_TMP_ROOT/prepare-existing-section.log"
  out="$TEST_TMP_ROOT/prepare-existing-section.out"
  make_fake_release_tools "$fake_bin"
  cat >>"$repo/CHANGELOG.md" <<'EOF'

## 0.2.0

- Previous release.
EOF
  git -C "$repo" add CHANGELOG.md
  git -C "$repo" commit -m "docs: add existing changelog section" >/dev/null

  assert_fails "$out" bash -c "cd '$repo' && PATH='$fake_bin':\$PATH DARC_RELEASE_TEST_LOG='$log' '$PREPARE_SCRIPT' 0.2.0"
  assert_contains "$out" "already has a section"
  assert_eq "0.1.0" "$(workspace_version "$repo/Cargo.toml")" "Cargo.toml after existing section rejection"
  [[ ! -f "$log" ]] || fail "prepare script should not run checks after existing section rejection"
}

make_release_ready_repo() {
  local name="$1"
  local version="$2"
  local repo
  repo="$(init_repo "$name" "$version" "- Placeholder before release prep.")"
  write_release_changelog "$repo" "$version"
  git -C "$repo" add CHANGELOG.md
  git -C "$repo" commit -m "chore: prepare release" >/dev/null
  git -C "$repo" push origin main >/dev/null 2>&1
  printf '%s\n' "$repo"
}

tag_success_pushes_annotated_tag_from_main() {
  local repo out tag_type
  repo="$(make_release_ready_repo tag-success 0.2.0)"
  out="$TEST_TMP_ROOT/tag-success.out"
  git -C "$repo" checkout -b scratch >/dev/null 2>&1

  (cd "$repo" && "$TAG_SCRIPT" 0.2.0) >"$out" 2>&1

  assert_contains "$out" "Tagged and pushed v0.2.0"
  tag_type="$(git -C "$repo" cat-file -t v0.2.0)"
  assert_eq "tag" "$tag_type" "created tag object type"
  git -C "$repo" ls-remote --exit-code --tags origin refs/tags/v0.2.0 >/dev/null ||
    fail "expected v0.2.0 to be pushed to origin"
  assert_eq "main" "$(git -C "$repo" branch --show-current)" "current branch after tagging"
}

tag_rejects_dirty_worktree() {
  local repo out
  repo="$(make_release_ready_repo tag-dirty 0.2.0)"
  out="$TEST_TMP_ROOT/tag-dirty.out"
  printf 'dirty\n' >>"$repo/README.md"

  assert_fails "$out" bash -c "cd '$repo' && '$TAG_SCRIPT' 0.2.0"
  assert_contains "$out" "working tree must be clean"
  if git -C "$repo" rev-parse -q --verify refs/tags/v0.2.0 >/dev/null; then
    fail "dirty tag test unexpectedly created a local tag"
  fi
}

tag_rejects_version_mismatch() {
  local repo out
  repo="$(make_release_ready_repo tag-version-mismatch 0.1.0)"
  write_release_changelog "$repo" 0.2.0
  git -C "$repo" add CHANGELOG.md
  git -C "$repo" commit -m "docs: add mismatched release heading" >/dev/null
  git -C "$repo" push origin main >/dev/null 2>&1
  out="$TEST_TMP_ROOT/tag-version-mismatch.out"

  assert_fails "$out" bash -c "cd '$repo' && '$TAG_SCRIPT' 0.2.0"
  assert_contains "$out" "Cargo.toml version is 0.1.0, expected 0.2.0"
  if git -C "$repo" rev-parse -q --verify refs/tags/v0.2.0 >/dev/null; then
    fail "version mismatch test unexpectedly created a local tag"
  fi
}

tag_rejects_missing_changelog_section() {
  local repo out
  repo="$(init_repo tag-missing-changelog 0.2.0 "- Unreleased but not finalized.")"
  out="$TEST_TMP_ROOT/tag-missing-changelog.out"

  assert_fails "$out" bash -c "cd '$repo' && '$TAG_SCRIPT' 0.2.0"
  assert_contains "$out" "CHANGELOG.md missing ## 0.2.0 section"
  if git -C "$repo" rev-parse -q --verify refs/tags/v0.2.0 >/dev/null; then
    fail "missing changelog test unexpectedly created a local tag"
  fi
}

tag_rejects_existing_remote_tag() {
  local repo out
  repo="$(make_release_ready_repo tag-existing-remote 0.2.0)"
  git -C "$repo" tag -a v0.2.0 -m v0.2.0
  git -C "$repo" push origin v0.2.0 >/dev/null 2>&1
  git -C "$repo" tag -d v0.2.0 >/dev/null 2>&1
  out="$TEST_TMP_ROOT/tag-existing-remote.out"

  assert_fails "$out" bash -c "cd '$repo' && '$TAG_SCRIPT' 0.2.0"
  assert_contains "$out" "tag v0.2.0 already exists"
}

tag_rejects_leading_v_version() {
  local repo out
  repo="$(make_release_ready_repo tag-leading-v 0.2.0)"
  out="$TEST_TMP_ROOT/tag-leading-v.out"

  assert_fails "$out" bash -c "cd '$repo' && '$TAG_SCRIPT' v0.2.0"
  assert_contains "$out" "without a leading v"
  if git -C "$repo" rev-parse -q --verify refs/tags/v0.2.0 >/dev/null; then
    fail "leading-v test unexpectedly created a local tag"
  fi
}

run_test() {
  local name="$1"
  shift
  printf 'test: %s\n' "$name"
  "$@"
}

run_test "prepare success updates version, changelog, and checks" prepare_success_updates_version_changelog_and_runs_checks
run_test "prepare rejects leading-v versions" prepare_rejects_leading_v_version
run_test "prepare rejects dirty worktrees" prepare_rejects_dirty_worktree
run_test "prepare rejects empty Unreleased without partial writes" prepare_rejects_empty_unreleased_without_partial_write
run_test "prepare rejects existing version sections" prepare_rejects_existing_version_section
run_test "tag pushes annotated tag from synced main" tag_success_pushes_annotated_tag_from_main
run_test "tag rejects dirty worktrees" tag_rejects_dirty_worktree
run_test "tag rejects Cargo.toml version mismatch" tag_rejects_version_mismatch
run_test "tag rejects missing changelog section" tag_rejects_missing_changelog_section
run_test "tag rejects existing remote tag" tag_rejects_existing_remote_tag
run_test "tag rejects leading-v versions" tag_rejects_leading_v_version

printf 'all release script tests passed\n'
