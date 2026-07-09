#!/usr/bin/env bash

set -euo pipefail

# 本地发版入口：更新版本、运行验证、提交并创建带说明的 annotated tag。
# 默认只准备并打本地 tag；传 --push 时才推送 commit/tag，触发 GitHub Actions 发布资产。

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PUSH=0
SKIP_VERIFY=0
ALLOW_DIRTY=0

usage() {
  cat <<'EOF'
usage: scripts/prepare-release.sh <version> [--push] [--skip-verify] [--allow-dirty]

Examples:
  scripts/prepare-release.sh 0.1.30
  scripts/prepare-release.sh 0.1.30 --push

Options:
  --allow-dirty  Explicitly allow preparing a release from a non-clean worktree.
EOF
}

die() {
  printf '[prepare-release] %s\n' "$*" >&2
  exit 1
}

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

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must be plain semver, for example 0.1.30"

cd "$ROOT_DIR"

[[ -d .git ]] || die "must run inside a git checkout"
[[ "$(git branch --show-current)" == "main" ]] || die "release must be prepared from main"
git fetch --tags --quiet
git rev-parse --verify "refs/tags/${version}" >/dev/null 2>&1 && die "tag ${version} already exists"

if [[ "$ALLOW_DIRTY" -eq 0 ]] && [[ -n "$(git status --porcelain=v1 --untracked-files=all)" ]]; then
  die "worktree must be clean before version changes; commit/stash changes or pass --allow-dirty explicitly"
fi
if [[ "$ALLOW_DIRTY" -eq 1 ]]; then
  printf '[prepare-release] WARNING: --allow-dirty set; existing worktree changes may be included in the release commit.\n' >&2
fi

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

# workspace.package.version 变化后必须同步 Cargo.lock；`cargo metadata --locked`
# 不总能捕捉这个差异，后续 `cargo test --locked` 会正确失败，所以这里直接重生成。
cargo generate-lockfile

notes_file="$(mktemp)"
trap 'rm -f "$notes_file"' EXIT
"$ROOT_DIR/scripts/release-notes.sh" "$version" >"$notes_file"
grep -q "用户可见变化" "$notes_file" || die "release notes must include user-visible changes"
if grep -q "请在 scripts/release-notes.sh" "$notes_file"; then
  die "release notes for ${version} are still the placeholder"
fi

if [[ "$SKIP_VERIFY" -eq 0 ]]; then
  bash scripts/qa.sh
  cargo build --release --locked -p termd --bin termd
fi

git diff --check
# 发版改动经常会新增前端组件、测试或文档，硬编码文件列表容易漏提交。
# 依赖 .gitignore 排除构建产物和本地状态，统一暂存当前工作区的全部源码改动。
git add -A

git diff --cached --quiet && die "no staged release changes"
git commit -m "Release ${version}"
git tag -a "$version" -F "$notes_file"

printf '[prepare-release] created commit and annotated tag %s\n' "$version"
if [[ "$PUSH" -eq 1 ]]; then
  git push origin main
  git push origin "$version"
  printf '[prepare-release] pushed main and tag %s\n' "$version"
else
  printf '[prepare-release] not pushed. Run: git push origin main && git push origin %s\n' "$version"
fi
