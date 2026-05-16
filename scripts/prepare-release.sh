#!/usr/bin/env bash

set -euo pipefail

# 本地发版入口：更新版本、运行验证、提交并创建带说明的 annotated tag。
# 默认只准备并打本地 tag；传 --push 时才推送 commit/tag，触发 GitHub Actions 发布资产。

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PUSH=0
SKIP_VERIFY=0

usage() {
  cat <<'EOF'
usage: scripts/prepare-release.sh <version> [--push] [--skip-verify]

Examples:
  scripts/prepare-release.sh 0.1.30
  scripts/prepare-release.sh 0.1.30 --push
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
git rev-parse --verify "refs/tags/${version}" >/dev/null 2>&1 && die "tag ${version} already exists"

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
  bash -n scripts/*.sh
  bash scripts/test-installers.sh
  cargo fmt --check
  cargo test --workspace --locked
  (
    cd termui/frontend
    npm run typecheck
    npm test -- --run src/__tests__/app.test.tsx src/__tests__/terminal-pane.test.tsx
    npm run build
  )
  cargo build --release -p termd --bin termd
fi

git diff --check
git add \
  .gitignore \
  .github/workflows/release.yml \
  Cargo.lock \
  Cargo.toml \
  proto/src/lib.rs \
  scripts/prepare-release.sh \
  scripts/release-notes.sh \
  scripts/install-termd.sh \
  scripts/test-installers.sh \
  termd/src/net/protocol.rs \
  termd/src/net/pty_bridge.rs \
  termd/src/net/server.rs \
  termd/src/pty/mod.rs \
  termd/src/pty/portable.rs \
  termd/src/pty/supervisor.rs \
  termd/src/runtime/mod.rs \
  termd/tests/session_supervisor.rs \
  termui/frontend/package.json \
  termui/frontend/package-lock.json \
  termui/frontend/src/App.tsx \
  termui/frontend/src/__tests__/app.test.tsx \
  termui/frontend/src/__tests__/protocol-types.test.ts \
  termui/frontend/src/__tests__/terminal-pane.test.tsx \
  termui/frontend/src/components/SessionFilesPanel.tsx \
  termui/frontend/src/components/TerminalPane.tsx \
  termui/frontend/src/protocol/direct-client.ts \
  termui/frontend/src/protocol/types.ts \
  termui/frontend/src/styles.css \
  termui/frontend/src/test/mock-daemon.ts

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
