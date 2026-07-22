#!/usr/bin/env bash

set -euo pipefail

# 在隔离 worktree 中生成 release commit，并发布精确指向它的 annotated tag。
# caller 的当前分支、index 和 worktree 始终保持只读。

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PUSH=0
SKIP_VERIFY=0
ALLOW_DIRTY=0
TEMP_ROOT=
RELEASE_WORKTREE=
WORKTREE_ADDED=0
BASE_OID=
REMOTE_MAIN_OID=
HEAD_REF=
RELEASE_COMMIT=
TAG_OBJECT=
CANDIDATE_REF=
CANDIDATE_OID=
CANDIDATE_TARGET_OID=

usage() {
  cat <<'EOF'
usage: scripts/prepare-release.sh <version> [--push] [--skip-verify] [--allow-dirty]

Examples:
  scripts/prepare-release.sh 0.1.30
  scripts/prepare-release.sh 0.1.30 --push

Options:
  --allow-dirty  Allow unrelated caller changes; caller state always remains unchanged.
EOF
}

die() {
  printf '[prepare-release] %s\n' "$*" >&2
  exit 1
}

cleanup_release_workspace() {
  local primary_status="$1"
  local cleanup_failed=0

  trap - EXIT
  set +e

  if [[ "$WORKTREE_ADDED" -eq 1 ]]; then
    if ! git -C "$ROOT_DIR" worktree remove --force "$RELEASE_WORKTREE"; then
      printf '[prepare-release] failed to remove temporary release worktree: %s\n' \
        "$RELEASE_WORKTREE" >&2
      cleanup_failed=1
    fi
  fi
  if [[ -n "$TEMP_ROOT" ]] && ! rm -rf -- "$TEMP_ROOT"; then
    printf '[prepare-release] failed to remove temporary release directory: %s\n' \
      "$TEMP_ROOT" >&2
    cleanup_failed=1
  fi
  if [[ "$cleanup_failed" -eq 1 ]]; then
    git -C "$ROOT_DIR" worktree prune >/dev/null 2>&1 ||
      printf '[prepare-release] failed to prune temporary worktree metadata\n' >&2
  fi

  if [[ "$primary_status" -ne 0 ]]; then
    exit "$primary_status"
  fi
  if [[ "$cleanup_failed" -eq 1 ]]; then
    exit 1
  fi
  exit 0
}

release_exit_trap() {
  cleanup_release_workspace "$?"
}

ref_exists() {
  local ref="$1"
  local status

  if git show-ref --verify --quiet "$ref"; then
    return 0
  else
    status=$?
  fi
  [[ "$status" -eq 1 ]] && return 1
  return "$status"
}

require_release_paths_clean() {
  local status

  if git diff --quiet -- "${RELEASE_PATHS[@]}"; then
    :
  else
    status=$?
    [[ "$status" -eq 1 ]] || die "failed to inspect release-owned worktree paths"
    die "release-owned files must be clean before version changes; --allow-dirty only permits unrelated changes"
  fi
  if git diff --cached --quiet -- "${RELEASE_PATHS[@]}"; then
    :
  else
    status=$?
    [[ "$status" -eq 1 ]] || die "failed to inspect release-owned index paths"
    die "release-owned files must be clean before version changes; --allow-dirty only permits unrelated changes"
  fi
}

validate_release_commit() {
  local commit="$1"
  local changed parents path
  local notes_changed=0

  parents="$(git rev-list --parents -n 1 "$commit")" || return 1
  [[ "$parents" == "$commit $BASE_OID" ]] || return 1

  changed="$(git diff-tree --no-commit-id --name-only -r "$commit")" || return 1
  while IFS= read -r path; do
    case "$path" in
      Cargo.toml|Cargo.lock|termui/frontend/package.json|termui/frontend/package-lock.json)
        ;;
      "$RELEASE_NOTES_PATH")
        notes_changed=1
        ;;
      *)
        printf '[prepare-release] release commit contains unexpected path: %s\n' "$path" >&2
        return 1
        ;;
    esac
  done <<<"$changed"
  [[ "$notes_changed" -eq 1 ]] || return 1
  git cat-file -e "${commit}:${RELEASE_NOTES_PATH}"
}

create_tag_object() {
  local tagger

  tagger="$(git var GIT_COMMITTER_IDENT)" || return 1
  {
    printf 'object %s\n' "$RELEASE_COMMIT"
    printf 'type commit\n'
    printf 'tag %s\n' "$version"
    printf 'tagger %s\n\n' "$tagger"
    cat "$NOTES_FILE"
  } | git mktag
}

read_candidate_ref() {
  local status

  CANDIDATE_OID=
  CANDIDATE_TARGET_OID=
  if ref_exists "$CANDIDATE_REF"; then
    CANDIDATE_OID="$(git rev-parse --verify "$CANDIDATE_REF")" || return 2
    CANDIDATE_TARGET_OID="$(git rev-parse --verify "${CANDIDATE_REF}^{commit}")" || return 2
    return 0
  else
    status=$?
  fi
  return "$status"
}

remove_candidate_ref() {
  local status

  if read_candidate_ref; then
    git update-ref -d -m "clean release candidate ${version}" \
      "$CANDIDATE_REF" "$CANDIDATE_OID"
  else
    status=$?
    [[ "$status" -eq 1 ]] || return "$status"
  fi
}

remove_current_candidate_ref() {
  local status

  if read_candidate_ref; then
    if [[ "$CANDIDATE_OID" == "$TAG_OBJECT" &&
          "$CANDIDATE_TARGET_OID" == "$RELEASE_COMMIT" ]]; then
      git update-ref -d -m "clean exact release candidate ${version}" \
        "$CANDIDATE_REF" "$TAG_OBJECT"
    fi
  else
    status=$?
    [[ "$status" -eq 1 ]] || return "$status"
  fi
}

install_recovery_candidate() {
  local status update_status=0

  git update-ref -m "recover release candidate ${version}" \
    "$CANDIDATE_REF" "$TAG_OBJECT" "" || update_status=$?

  if read_candidate_ref; then
    if [[ "$CANDIDATE_OID" == "$TAG_OBJECT" &&
          "$CANDIDATE_TARGET_OID" == "$RELEASE_COMMIT" ]]; then
      if [[ "$update_status" -ne 0 ]]; then
        printf '[prepare-release] candidate update returned %s after the exact ref was created; treating it as preserved\n' \
          "$update_status" >&2
      fi
      return 0
    fi
    printf '[prepare-release] recovery candidate has concurrent value %s targeting %s; it was not overwritten\n' \
      "$CANDIDATE_OID" "$CANDIDATE_TARGET_OID" >&2
    return 1
  else
    status=$?
  fi
  if [[ "$status" -eq 1 ]]; then
    printf '[prepare-release] recovery candidate is absent after update status %s\n' \
      "$update_status" >&2
  else
    printf '[prepare-release] failed to inspect recovery candidate after update status %s\n' \
      "$update_status" >&2
  fi
  return 1
}

diagnose_publication_failure() {
  local publish_status="$1"
  local status tag_oid tag_target_oid

  if ref_exists "refs/tags/${version}"; then
    tag_oid="$(git rev-parse --verify "refs/tags/${version}")" || {
      printf '[prepare-release] formal tag exists but cannot be resolved\n' >&2
      return "$publish_status"
    }
    tag_target_oid="$(git rev-parse --verify "refs/tags/${version}^{commit}")" || tag_target_oid=
    if [[ "$tag_oid" == "$TAG_OBJECT" && "$tag_target_oid" == "$RELEASE_COMMIT" ]]; then
      remove_current_candidate_ref ||
        printf '[prepare-release] failed to CAS-delete the exact recovery candidate %s\n' \
          "$CANDIDATE_REF" >&2
      printf '[prepare-release] publication command failed after the exact formal tag was created; release remains reachable\n' >&2
    else
      printf '[prepare-release] publication failed because conflicting formal tag %s now exists; recovery candidates were left untouched\n' \
        "$version" >&2
    fi
    return "$publish_status"
  else
    status=$?
  fi
  if [[ "$status" -ne 1 ]]; then
    printf '[prepare-release] failed to inspect formal tag after publication failure; refusing to create a recovery candidate\n' >&2
    return "$publish_status"
  fi

  if install_recovery_candidate; then
    printf '[prepare-release] local publication was rejected; exact annotated tag object is retained at %s\n' \
      "$CANDIDATE_REF" >&2
    printf '[prepare-release] after resolving the branch race, retry this release; to discard it run: git update-ref -d %s\n' \
      "$CANDIDATE_REF" >&2
  else
    printf '[prepare-release] local publication failed and the recovery candidate could not be created\n' >&2
  fi
  return "$publish_status"
}

publish_local_tag() {
  local status tag_oid target_oid

  if {
    printf 'start\n'
    printf 'verify %s %s\n' "$HEAD_REF" "$BASE_OID"
    printf 'create refs/tags/%s %s\n' "$version" "$TAG_OBJECT"
    printf 'prepare\n'
    printf 'commit\n'
  } | git update-ref --stdin; then
    :
  else
    status=$?
    diagnose_publication_failure "$status"
    return "$status"
  fi

  tag_oid="$(git rev-parse --verify "refs/tags/${version}")" || return 1
  target_oid="$(git rev-parse --verify "refs/tags/${version}^{commit}")" || return 1
  if [[ "$tag_oid" != "$TAG_OBJECT" || "$target_oid" != "$RELEASE_COMMIT" ]]; then
    printf '[prepare-release] formal tag changed during publication verification\n' >&2
    return 1
  fi
  remove_candidate_ref || {
    printf '[prepare-release] formal tag was published but stale recovery candidate cleanup failed: %s\n' \
      "$CANDIDATE_REF" >&2
    return 1
  }
}

case "${1:-}" in
  -h|--help)
    usage
    exit 0
    ;;
esac

version="${1:-}"
[[ -n "$version" ]] || {
  usage >&2
  exit 2
}
shift

while (($#)); do
  case "$1" in
    --push)
      PUSH=1
      ;;
    --skip-verify)
      SKIP_VERIFY=1
      ;;
    --allow-dirty)
      ALLOW_DIRTY=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
  shift
done

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
  die "version must be plain semver, for example 0.1.30"

RELEASE_NOTES_PATH="docs/releases/${version}.md"
CANDIDATE_REF="refs/termd/release-candidates/${version}"
RELEASE_PATHS=(
  Cargo.toml
  Cargo.lock
  termui/frontend/package.json
  termui/frontend/package-lock.json
  "$RELEASE_NOTES_PATH"
)

cd "$ROOT_DIR"
[[ "$(git rev-parse --is-inside-work-tree 2>/dev/null)" == "true" ]] ||
  die "must run inside a git checkout"
HEAD_REF="$(git symbolic-ref -q HEAD)" || die "release must be prepared from a branch"
[[ "$HEAD_REF" == "refs/heads/main" ]] || die "release must be prepared from main"
BASE_OID="$(git rev-parse --verify "${HEAD_REF}^{commit}")" ||
  die "failed to capture the main commit"

git fetch --tags --quiet origin \
  '+refs/heads/main:refs/remotes/origin/main' ||
  die "failed to fetch origin/main and tags"
REMOTE_MAIN_OID="$(git rev-parse --verify 'refs/remotes/origin/main^{commit}')" ||
  die "failed to capture origin/main"
if git merge-base --is-ancestor "$REMOTE_MAIN_OID" "$BASE_OID"; then
  :
else
  ancestor_status=$?
  [[ "$ancestor_status" -eq 1 ]] ||
    die "failed to compare origin/main with local main"
  die "remote origin/main is not an ancestor of local main; reconcile origin/main before release"
fi
if ref_exists "refs/tags/${version}"; then
  die "tag ${version} already exists"
else
  tag_lookup_status=$?
  [[ "$tag_lookup_status" -eq 1 ]] ||
    die "failed to inspect tag ${version}; refusing to continue"
fi

require_release_paths_clean
dirty_status="$(git status --porcelain=v1 --untracked-files=all -- . \
  ":(exclude)${RELEASE_NOTES_PATH}")" ||
  die "failed to inspect worktree status; refusing to continue"
if [[ "$ALLOW_DIRTY" -eq 0 && -n "$dirty_status" ]]; then
  die "worktree must be clean before version changes; commit/stash changes or pass --allow-dirty explicitly"
fi
if [[ "$ALLOW_DIRTY" -eq 1 ]]; then
  printf '[prepare-release] WARNING: --allow-dirty set; caller branch/index/worktree remain unchanged.\n' >&2
fi

[[ -f "$RELEASE_NOTES_PATH" ]] ||
  die "release notes file is missing: ${RELEASE_NOTES_PATH}"
TEMP_ROOT="$(mktemp -d)" || die "failed to create temporary release directory"
RELEASE_WORKTREE="${TEMP_ROOT}/worktree"
NOTES_FILE="${TEMP_ROOT}/release-notes.md"
trap 'release_exit_trap' EXIT

"$ROOT_DIR/scripts/release-notes.sh" "$version" >"$NOTES_FILE"
grep -q "用户可见变化" "$NOTES_FILE" ||
  die "release notes must include user-visible changes"
if grep -q "请在 scripts/release-notes.sh" "$NOTES_FILE"; then
  die "release notes for ${version} are still the placeholder"
fi

git worktree add --quiet --detach "$RELEASE_WORKTREE" "$BASE_OID" ||
  die "failed to create isolated release worktree"
WORKTREE_ADDED=1
mkdir -p "${RELEASE_WORKTREE}/docs/releases"
cp "$NOTES_FILE" "${RELEASE_WORKTREE}/${RELEASE_NOTES_PATH}"

set +e
(
  set -e
  cd "$RELEASE_WORKTREE"
  python3 - "$version" <<'PY'
import json
import pathlib
import re
import sys

version = sys.argv[1]
root_cargo = pathlib.Path("Cargo.toml")
text = root_cargo.read_text()
text, count = re.subn(r'(?m)^version = "\d+\.\d+\.\d+"$', f'version = "{version}"', text, count=1)
if count != 1:
    raise SystemExit("failed to update workspace version in Cargo.toml")
root_cargo.write_text(text)

for package_json in (pathlib.Path("termui/frontend/package.json"), pathlib.Path("termui/frontend/package-lock.json")):
    data = json.loads(package_json.read_text())
    data["version"] = version
    if package_json.name == "package-lock.json":
        data["packages"][""]["version"] = version
    package_json.write_text(json.dumps(data, indent=2) + "\n")
PY

  # 直接从现有 lockfile 定向改 workspace package 版本。generate-lockfile/cargo update
  # 都会重新解析兼容依赖，可能把无关的传递依赖升级混入补丁发版。
  lockfile_before="${TEMP_ROOT}/Cargo.lock.before"
  verified_lockfile="${TEMP_ROOT}/Cargo.lock.release"
  cp Cargo.lock "$lockfile_before"
  python3 - "$lockfile_before" "$verified_lockfile" "$version" <<'PY'
import pathlib
import re
import sys
import tomllib

before_path = pathlib.Path(sys.argv[1])
verified_path = pathlib.Path(sys.argv[2])
release_version = sys.argv[3]
root_manifest = tomllib.loads(pathlib.Path("Cargo.toml").read_text())

workspace_names = set()
root_package = root_manifest.get("package")
if root_package and root_package.get("version", {}).get("workspace") is True:
    workspace_names.add(root_package["name"])
workspace = root_manifest.get("workspace", {})
for member_pattern in workspace.get("members", []):
    for member_path in pathlib.Path(".").glob(member_pattern):
        manifest_path = member_path / "Cargo.toml"
        if not manifest_path.is_file():
            continue
        member_manifest = tomllib.loads(manifest_path.read_text())
        package = member_manifest.get("package", {})
        if package.get("version", {}).get("workspace") is True:
            workspace_names.add(package["name"])
if not workspace_names:
    raise SystemExit("failed to identify workspace packages for Cargo.lock validation")

lock_text = before_path.read_text()
seen = set()

def update_package(match):
    block = match.group(0)
    package = tomllib.loads(block)["package"][0]
    name = package.get("name")
    if name not in workspace_names or "source" in package:
        return block
    if name in seen:
        raise SystemExit(f"duplicate workspace package in Cargo.lock: {name}")
    seen.add(name)
    updated, count = re.subn(
        r'(?m)^version = "[^"]+"$',
        f'version = "{release_version}"',
        block,
        count=1,
    )
    if count != 1:
        raise SystemExit(f"failed to update Cargo.lock package version: {name}")
    return updated

verified = re.sub(
    r'(?ms)^\[\[package\]\]\n.*?(?=^\[\[package\]\]\n|\Z)',
    update_package,
    lock_text,
)
if seen != workspace_names:
    raise SystemExit(
        "Cargo.lock workspace package set mismatch: "
        f"missing={sorted(workspace_names - seen)}"
    )
verified_path.write_text(verified)
PY
  cp "$verified_lockfile" Cargo.lock
  if [[ "$SKIP_VERIFY" -eq 0 ]]; then
    bash scripts/qa.sh
    cargo build --release --locked -p termd --bin termd
  fi
  if ! cmp -s "$verified_lockfile" Cargo.lock; then
    die "Cargo.lock changed outside workspace package version updates; dependency updates must be committed separately"
  fi

  git add -- "${RELEASE_PATHS[@]}"
  git diff --cached --check -- "${RELEASE_PATHS[@]}"
  if git diff --cached --quiet -- "${RELEASE_PATHS[@]}"; then
    die "no staged release changes"
  else
    staged_diff_status=$?
    [[ "$staged_diff_status" -eq 1 ]] || die "failed to inspect staged release changes"
  fi
  git commit -m "Release ${version}" -- "${RELEASE_PATHS[@]}"
)
commit_status=$?
set -e

isolated_head="$(git -C "$RELEASE_WORKTREE" rev-parse --verify 'HEAD^{commit}')" ||
  die "failed to inspect the isolated release commit"
if [[ "$commit_status" -ne 0 ]]; then
  if [[ "$isolated_head" != "$BASE_OID" ]]; then
    printf '[prepare-release] commit command failed after isolated HEAD advanced to %s\n' \
      "$isolated_head" >&2
  fi
  exit "$commit_status"
fi

RELEASE_COMMIT="$isolated_head"
[[ "$RELEASE_COMMIT" != "$BASE_OID" ]] ||
  die "release commit did not advance the isolated worktree"
validate_release_commit "$RELEASE_COMMIT" ||
  die "isolated release commit failed structural validation"

TAG_OBJECT="$(create_tag_object)" ||
  die "failed to create annotated tag object for ${version}"
[[ "$(git rev-parse --verify "${TAG_OBJECT}^{commit}")" == "$RELEASE_COMMIT" ]] ||
  die "annotated tag object does not target the exact release commit"

publish_local_tag || exit $?

printf '[prepare-release] created release commit %s and annotated tag %s.\n' \
  "$RELEASE_COMMIT" "$version"
printf '[prepare-release] caller branch/index/worktree are unchanged; local main remains at %s.\n' \
  "$BASE_OID"
printf '[prepare-release] after handling unrelated caller changes, advance local main with:\n'
printf "[prepare-release]   git add -- '%s'\n" "$RELEASE_NOTES_PATH"
printf '[prepare-release]   git merge --ff-only %s\n' "$RELEASE_COMMIT"

if [[ "$PUSH" -eq 1 ]]; then
  [[ "$(git rev-parse --verify "refs/tags/${version}")" == "$TAG_OBJECT" ]] ||
    die "tag ${version} changed before push; local release was not pushed"
  [[ "$(git rev-parse --verify "refs/tags/${version}^{commit}")" == "$RELEASE_COMMIT" ]] ||
    die "tag ${version} no longer targets the release commit; local release was not pushed"
  [[ "$(git rev-parse --verify "$HEAD_REF")" == "$BASE_OID" ]] ||
    die "local main changed before push; release was not pushed"

  if git push --atomic --force-with-lease="refs/heads/main:${REMOTE_MAIN_OID}" origin \
    "${RELEASE_COMMIT}:refs/heads/main" \
    "${TAG_OBJECT}:refs/tags/${version}"; then
    :
  else
    push_status=$?
    printf '[prepare-release] atomic push failed; local exact tag remains and no non-atomic fallback was attempted\n' >&2
    exit "$push_status"
  fi
  printf '[prepare-release] atomically pushed remote main and tag %s; local main remains at %s.\n' \
    "$version" "$BASE_OID"
else
  printf '[prepare-release] not pushed. Run: git push --atomic --force-with-lease=refs/heads/main:%s origin %s:refs/heads/main %s:refs/tags/%s\n' \
    "$REMOTE_MAIN_OID" "$RELEASE_COMMIT" "$TAG_OBJECT" "$version"
fi
