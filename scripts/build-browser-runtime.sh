#!/usr/bin/env bash
set -euo pipefail

TIGERVNC_VERSION="1.16.2"
TIGERVNC_SHA256="3f22ba333f4b54c1d8f5cd2e57e2723b12706bbd127a3583d57bcc205e94b47d"
TIGERVNC_URL="https://sourceforge.net/projects/tigervnc/files/stable/${TIGERVNC_VERSION}/ubuntu-22.04LTS/source/tigervnc_${TIGERVNC_VERSION}.orig.tar.gz/download"
OPENBOX_VERSION="3.6.1"
OPENBOX_SHA256="8b4ac0760018c77c0044fab06a4f0c510ba87eae934d9983b10878483bde7ef7"
OPENBOX_URL="https://openbox.org/dist/openbox/openbox-${OPENBOX_VERSION}.tar.gz"
GLIBC_MIN_VERSION="2.31"

usage() {
  printf 'usage: %s <output-dir> <termd-version> <amd64|arm64>\n' "$0" >&2
}

[[ $# -eq 3 ]] || {
  usage
  exit 2
}

output_dir="$(realpath -m "$1")"
termd_version="$2"
asset_arch="$3"
case "$(uname -m):${asset_arch}" in
  x86_64:amd64 | aarch64:arm64) ;;
  *)
    printf 'native runner architecture does not match requested asset: %s / %s\n' \
      "$(uname -m)" "$asset_arch" >&2
    exit 1
    ;;
esac

for command in autoreconf cmake curl jq make patch pkg-config readelf sha256sum tar xkbcomp; do
  command -v "$command" >/dev/null || {
    printf 'required build command is missing: %s\n' "$command" >&2
    exit 1
  }
done
[[ -f /usr/src/xorg-server.tar.xz ]] || {
  printf 'xorg-server-source is missing: /usr/src/xorg-server.tar.xz\n' >&2
  exit 1
}

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
runtime_dir="${work_dir}/runtime"
mkdir -p "$output_dir" "${runtime_dir}/bin" "${runtime_dir}/lib" \
  "${runtime_dir}/share/X11" "${runtime_dir}/share/licenses/packages"

download_source() {
  local url="$1"
  local sha256="$2"
  local destination="$3"
  curl --fail --location --retry 4 --retry-connrefused \
    --connect-timeout 20 --max-time 300 --silent --show-error \
    --output "$destination" "$url"
  printf '%s  %s\n' "$sha256" "$destination" | sha256sum --check --status || {
    printf 'source checksum mismatch: %s\n' "$url" >&2
    exit 1
  }
}

tigervnc_archive="${work_dir}/tigervnc.tar.gz"
openbox_archive="${work_dir}/openbox.tar.gz"
download_source "$TIGERVNC_URL" "$TIGERVNC_SHA256" "$tigervnc_archive"
download_source "$OPENBOX_URL" "$OPENBOX_SHA256" "$openbox_archive"
tar -xzf "$tigervnc_archive" -C "$work_dir"
tar -xzf "$openbox_archive" -C "$work_dir"
tigervnc_source="${work_dir}/tigervnc-${TIGERVNC_VERSION}"
openbox_source="${work_dir}/openbox-${OPENBOX_VERSION}"

# Xvnc's autotools build resolves the TigerVNC build tree as ../.., so the
# first-stage CMake output must follow TigerVNC's documented in-source layout.
cmake -S "$tigervnc_source" -B "$tigervnc_source" \
  -DCMAKE_BUILD_TYPE=Release \
  -DBUILD_VIEWER=OFF \
  -DBUILD_STATIC=OFF \
  -DENABLE_GNUTLS=OFF \
  -DENABLE_H264=OFF \
  -DENABLE_NLS=OFF \
  -DENABLE_NETTLE=OFF \
  -DENABLE_SYSTEMD=OFF \
  -DENABLE_WAYLAND=OFF
cmake --build "$tigervnc_source" --parallel "$(nproc)"

mkdir -p "${tigervnc_source}/unix/xserver"
tar -xJf /usr/src/xorg-server.tar.xz -C "${tigervnc_source}/unix/xserver" \
  --strip-components=1
(
  cd "${tigervnc_source}/unix/xserver"
  xserver_version="$(awk -F, '/^AC_INIT\(\[xorg-server\],/{value=$2; gsub(/[ \[\]]/, "", value); print value; exit}' configure.ac)"
  case "$xserver_version" in
    1.20.*) xserver_patch="xserver120.patch" ;;
    1.21.* | 21.*) xserver_patch="xserver21.patch" ;;
    *)
      printf 'unsupported Xorg server source version: %s\n' "$xserver_version" >&2
      exit 1
      ;;
  esac
  patch -p1 < "../${xserver_patch}"
  autoreconf -fiv
  export PIXMANINCDIR=/usr/include/pixman-1
  ./configure \
    --prefix=/usr \
    --disable-config-hal \
    --disable-config-udev \
    --disable-devel-docs \
    --disable-dmx \
    --disable-docs \
    --disable-dri \
    --disable-dri2 \
    --disable-dri3 \
    --disable-glamor \
    --disable-glx \
    --disable-install-setuid \
    --disable-kdrive \
    --disable-selective-werror \
    --disable-static \
    --disable-unit-tests \
    --disable-xephyr \
    --disable-xnest \
    --disable-xorg \
    --disable-xvfb \
    --disable-xwayland \
    --disable-xwin \
    --disable-xquartz \
    --enable-composite \
    --enable-record \
    --enable-screensaver \
    --enable-xinerama \
    --enable-xres \
    --enable-xv \
    --with-default-font-path=built-ins \
    --with-xkb-bin-directory= \
    --with-default-xkb-rules=evdev \
    --with-pic \
    --with-xkb-path=/usr/share/X11/xkb \
    --without-dtrace
  make --jobs "$(nproc)"
)

xvnc_binary="${tigervnc_source}/unix/xserver/hw/vnc/Xvnc"
[[ -x "$xvnc_binary" ]] || {
  printf 'TigerVNC build did not produce Xvnc\n' >&2
  exit 1
}
install -m 0755 "$xvnc_binary" "${runtime_dir}/bin/Xtigervnc"
install -m 0755 "$(command -v xkbcomp)" "${runtime_dir}/bin/xkbcomp"

(
  cd "$openbox_source"
  ./configure \
    --prefix=/usr \
    --disable-nls \
    --disable-session-management \
    --disable-startup-notification
  make --jobs "$(nproc)"
  make install DESTDIR="${work_dir}/openbox-stage"
)
install -m 0755 "${work_dir}/openbox-stage/usr/bin/openbox" "${runtime_dir}/bin/openbox"
mkdir -p "${runtime_dir}/share/themes"
cp -aL "${work_dir}/openbox-stage/usr/share/themes/Clearlooks" \
  "${runtime_dir}/share/themes/Clearlooks"
while IFS= read -r library; do
  install -m 0644 -D "$(readlink -f "$library")" \
    "${runtime_dir}/lib/$(basename "$library")"
done < <(find "${work_dir}/openbox-stage/usr/lib" \
  \( -type f -o -type l \) -name '*.so*' | sort)

copy_package_license() {
  local dependency="$1"
  local resolved package copyright destination
  resolved="$(readlink -f "$dependency")"
  package="$(dpkg-query -S "$resolved" 2>/dev/null | head -n1 | cut -d: -f1 || true)"
  [[ -n "$package" ]] || return 0
  copyright="/usr/share/doc/${package%%:*}/copyright"
  [[ -f "$copyright" ]] || return 0
  destination="${runtime_dir}/share/licenses/packages/${package//[:\/]/_}.txt"
  [[ -e "$destination" ]] || install -m 0644 "$copyright" "$destination"
}

is_host_glibc_component() {
  case "$1" in
    ld-linux-*.so.* | libc.so.* | libanl.so.* | libdl.so.* | libm.so.* | \
      libnss_*.so.* | libpthread.so.* | libresolv.so.* | librt.so.* | libutil.so.*)
      return 0
      ;;
    *) return 1 ;;
  esac
}

declare -a dependency_queue=(
  "${runtime_dir}/bin/Xtigervnc"
  "${runtime_dir}/bin/openbox"
  "${runtime_dir}/bin/xkbcomp"
)
declare -A inspected_dependencies=()
while ((${#dependency_queue[@]} > 0)); do
  current="${dependency_queue[0]}"
  dependency_queue=("${dependency_queue[@]:1}")
  [[ -z "${inspected_dependencies[$current]:-}" ]] || continue
  inspected_dependencies["$current"]=1
  ldd_output="$(LD_LIBRARY_PATH="${runtime_dir}/lib" ldd "$current")"
  if grep -Fq 'not found' <<<"$ldd_output"; then
    printf 'unresolved runtime dependency for %s:\n%s\n' "$current" "$ldd_output" >&2
    exit 1
  fi
  while IFS= read -r dependency; do
    [[ -n "$dependency" && -f "$dependency" ]] || continue
    dependency_name="$(basename "$dependency")"
    is_host_glibc_component "$dependency_name" && continue
    copy_package_license "$dependency"
    packaged="${runtime_dir}/lib/${dependency_name}"
    if [[ ! -e "$packaged" ]]; then
      install -m 0644 "$(readlink -f "$dependency")" "$packaged"
    fi
    dependency_queue+=("$packaged")
  done < <(awk '/=> \// { print $3 } /^[[:space:]]*\// { print $1 }' <<<"$ldd_output")
done

max_required_glibc="$(
  find "${runtime_dir}/bin" "${runtime_dir}/lib" -type f -print0 |
    while IFS= read -r -d '' elf; do
      readelf --version-info "$elf" 2>/dev/null || true
    done |
    sed -n 's/.*Name: GLIBC_\([0-9][0-9.]*\).*/\1/p' |
    sort -V |
    tail -n1
)"
if [[ -z "$max_required_glibc" ]] ||
  [[ "$(printf '%s\n%s\n' "$GLIBC_MIN_VERSION" "$max_required_glibc" | sort -V | tail -n1)" != "$GLIBC_MIN_VERSION" ]]; then
  printf 'browser runtime requires unsupported glibc %s (maximum allowed: %s)\n' \
    "${max_required_glibc:-unknown}" "$GLIBC_MIN_VERSION" >&2
  exit 1
fi

cp -aL /usr/share/X11/xkb "${runtime_dir}/share/X11/xkb"
install -m 0644 "${tigervnc_source}/LICENCE.TXT" \
  "${runtime_dir}/share/licenses/TigerVNC-LICENCE.txt"
install -m 0644 "${openbox_source}/COPYING" \
  "${runtime_dir}/share/licenses/Openbox-COPYING.txt"
for copyright in /usr/share/doc/xorg-server-source/copyright /usr/share/doc/xkb-data/copyright; do
  [[ -f "$copyright" ]] || continue
  install -m 0644 "$copyright" \
    "${runtime_dir}/share/licenses/packages/$(basename "$(dirname "$copyright")")-copyright.txt"
done
{
  printf 'TigerVNC %s\nSource: %s\nSHA-256: %s\n\n' \
    "$TIGERVNC_VERSION" "$TIGERVNC_URL" "$TIGERVNC_SHA256"
  printf 'Openbox %s\nSource: %s\nSHA-256: %s\n' \
    "$OPENBOX_VERSION" "$OPENBOX_URL" "$OPENBOX_SHA256"
  printf '\nRuntime glibc baseline: %s\n' "$GLIBC_MIN_VERSION"
} > "${runtime_dir}/share/licenses/SOURCES.txt"

find "$runtime_dir" -type l -print -quit | grep -q . && {
  printf 'browser runtime must not contain symbolic links\n' >&2
  exit 1
}
find "${runtime_dir}/bin" -type f -exec chmod 0755 {} +
find "${runtime_dir}/lib" "${runtime_dir}/share" -type f -exec chmod 0644 {} +
strip --strip-unneeded "${runtime_dir}/bin/Xtigervnc" "${runtime_dir}/bin/openbox"
find "${runtime_dir}/lib" -type f -exec strip --strip-unneeded {} + 2>/dev/null || true

archive_name="termd-browser-runtime-linux-${asset_arch}.tar.gz"
manifest_name="termd-browser-runtime-linux-${asset_arch}.json"
archive_path="${output_dir}/${archive_name}"
manifest_path="${output_dir}/${manifest_name}"
tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner \
  -czf "$archive_path" -C "$runtime_dir" bin lib share
archive_size="$(stat -c '%s' "$archive_path")"
archive_sha256="$(sha256sum "$archive_path" | cut -d' ' -f1)"
jq -n \
  --arg termd_version "$termd_version" \
  --arg runtime_version "tigervnc-${TIGERVNC_VERSION}-openbox-${OPENBOX_VERSION}" \
  --arg arch "$asset_arch" \
  --arg glibc_min_version "$GLIBC_MIN_VERSION" \
  --arg archive_file "$archive_name" \
  --arg archive_sha256 "$archive_sha256" \
  --argjson archive_size_bytes "$archive_size" \
  '{
    schema_version: 1,
    termd_version: $termd_version,
    runtime_version: $runtime_version,
    os: "linux",
    arch: $arch,
    glibc_min_version: $glibc_min_version,
    archive_file: $archive_file,
    archive_size_bytes: $archive_size_bytes,
    archive_sha256: $archive_sha256
  }' > "$manifest_path"

printf 'built %s (%s bytes)\n' "$archive_path" "$archive_size"
printf 'wrote %s\n' "$manifest_path"
