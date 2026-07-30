#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf 'usage: %s <browser-runtime.tar.gz>\n' "$0" >&2
}

[[ $# -eq 1 ]] || {
  usage
  exit 2
}

archive="$1"
[[ -f "$archive" ]] || {
  printf 'browser runtime archive does not exist: %s\n' "$archive" >&2
  exit 1
}

listing="$(LC_ALL=C tar -tvzf "$archive")" || {
  printf 'browser runtime archive is not a readable gzip tar: %s\n' "$archive" >&2
  exit 1
}
[[ -n "$listing" ]] || {
  printf 'browser runtime archive is empty: %s\n' "$archive" >&2
  exit 1
}

unsupported="$(
  awk '
    substr($1, 1, 1) != "-" && substr($1, 1, 1) != "d" {
      print
      exit
    }
  ' <<<"$listing"
)"
[[ -z "$unsupported" ]] || {
  printf 'browser runtime archive contains an unsupported entry type:\n%s\n' \
    "$unsupported" >&2
  exit 1
}

printf 'validated browser runtime archive: %s\n' "$archive"
