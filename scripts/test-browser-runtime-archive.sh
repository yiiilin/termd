#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VALIDATOR="${ROOT_DIR}/scripts/validate-browser-runtime-archive.sh"
BUILDER="${ROOT_DIR}/scripts/build-browser-runtime.sh"

[[ -x "$VALIDATOR" ]] || {
  printf 'browser runtime archive validator is missing: %s\n' "$VALIDATOR" >&2
  exit 1
}
grep -Fq 'tar --hard-dereference --sort=name' "$BUILDER"
grep -Fq 'validate-browser-runtime-archive.sh" "$archive_path"' "$BUILDER"

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

runtime_dir="${work_dir}/runtime"
mkdir -p "${runtime_dir}/bin" "${runtime_dir}/lib" \
  "${runtime_dir}/share/X11/xkb/rules"
printf 'binary\n' >"${runtime_dir}/bin/Xtigervnc"
printf 'library\n' >"${runtime_dir}/lib/libfixture.so"
printf 'rules\n' >"${runtime_dir}/share/X11/xkb/rules/base"

regular_archive="${work_dir}/regular.tar.gz"
tar --hard-dereference -czf "$regular_archive" -C "$runtime_dir" bin lib share
"$VALIDATOR" "$regular_archive"

hardlink_dir="${work_dir}/hardlink"
cp -a "$runtime_dir" "$hardlink_dir"
ln "${hardlink_dir}/share/X11/xkb/rules/base" \
  "${hardlink_dir}/share/X11/xkb/rules/xorg"
hardlink_archive="${work_dir}/hardlink.tar.gz"
tar -czf "$hardlink_archive" -C "$hardlink_dir" bin lib share
if "$VALIDATOR" "$hardlink_archive"; then
  printf 'validator accepted a hardlink entry\n' >&2
  exit 1
fi

symlink_dir="${work_dir}/symlink"
cp -a "$runtime_dir" "$symlink_dir"
ln -s base "${symlink_dir}/share/X11/xkb/rules/xorg"
symlink_archive="${work_dir}/symlink.tar.gz"
tar -czf "$symlink_archive" -C "$symlink_dir" bin lib share
if "$VALIDATOR" "$symlink_archive"; then
  printf 'validator accepted a symbolic link entry\n' >&2
  exit 1
fi

invalid_archive="${work_dir}/invalid.tar.gz"
printf 'not a tar archive\n' >"$invalid_archive"
if "$VALIDATOR" "$invalid_archive"; then
  printf 'validator accepted an invalid archive\n' >&2
  exit 1
fi

printf 'browser runtime archive validation tests passed\n'
