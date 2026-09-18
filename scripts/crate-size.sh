#!/usr/bin/env bash
# Fail the gate when any workspace crate's src/ has grown past its ceiling.
#
# The metric is non-test source lines per crate, defined as
#   find crates/<name>/src -name '*.rs' -print0 | xargs -0 cat | wc -l
# tests/ directories do not count.
#
# Default ceiling is 8000. Crates already above that are grandfathered in
# scripts/crate-ceilings.txt. A PR that shrinks a crate should run
#   scripts/crate-size.sh --tighten
# and commit the result, so the ceiling follows the deletion down and never
# ratchets back up.
#
# Usage:
#   scripts/crate-size.sh           # silent on pass, one line per failure
#   scripts/crate-size.sh -v        # print every crate
#   scripts/crate-size.sh --tighten # rewrite grandfathered ceilings downward
set -euo pipefail

cd "$(dirname "$0")/.." || exit 1

DEFAULT=8000
CEILINGS=scripts/crate-ceilings.txt

verbose=0
tighten=0
while [ $# -gt 0 ]; do
  case "$1" in
    -v) verbose=1 ;;
    --tighten) tighten=1 ;;
    *)
      printf 'unknown option: %s\n' "$1" >&2
      exit 2
      ;;
  esac
  shift
done

list_crates() {
  local src
  for src in crates/*/src; do
    if [ -d "$src" ]; then
      basename "$(dirname "$src")"
    fi
  done | LC_ALL=C sort
}

# GNU xargs runs `cat` with no args when find prints nothing, and then hangs
# on stdin. BSD xargs does not. The metric pipeline is used only when a crate
# has at least one .rs file.
measure() {
  local crate n
  crate="$1"
  if [ -z "$(find "crates/${crate}/src" -name '*.rs' -print)" ]; then
    printf '%s\n' 0
    return 0
  fi
  n=$(find "crates/${crate}/src" -name '*.rs' -print0 | xargs -0 cat | wc -l)
  printf '%s\n' "$n" | tr -d '[:space:]'
}

recorded_ceiling() {
  local crate line name value
  crate="$1"
  if [ ! -f "$CEILINGS" ]; then
    return 0
  fi
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      ''|'#'*) continue ;;
    esac
    name=${line%% *}
    value=${line#* }
    if [ "$name" = "$crate" ]; then
      printf '%s\n' "$value"
      return 0
    fi
  done < "$CEILINGS"
}

ceiling_of() {
  local recorded
  recorded=$(recorded_ceiling "$1")
  if [ -n "$recorded" ]; then
    printf '%s\n' "$recorded"
  else
    printf '%s\n' "$DEFAULT"
  fi
}

write_header() {
  printf '%s\n' '# <crate> <ceiling>, one per line, sorted by crate name.'
  printf '%s\n' '# A crate not listed uses the default of 8000.'
}

tighten_file() {
  local crate measured recorded tmp
  tmp="${CEILINGS}.tmp"
  {
    write_header
    for crate in $(list_crates); do
      measured=$(measure "$crate")
      recorded=$(recorded_ceiling "$crate")
      if [ -z "$recorded" ]; then
        continue
      fi
      if [ "$measured" -le "$DEFAULT" ]; then
        continue
      fi
      if [ "$measured" -lt "$recorded" ]; then
        printf '%s %s\n' "$crate" "$measured"
      else
        printf '%s %s\n' "$crate" "$recorded"
      fi
    done
  } > "$tmp"
  mv "$tmp" "$CEILINGS"
}

if [ "$tighten" -eq 1 ]; then
  tighten_file
fi

failed=0
for crate in $(list_crates); do
  measured=$(measure "$crate")
  ceiling=$(ceiling_of "$crate")
  if [ "$measured" -gt "$ceiling" ]; then
    printf '%s %s %s\n' "$crate" "$measured" "$ceiling"
    failed=1
  elif [ "$verbose" -eq 1 ]; then
    printf '%s %s %s\n' "$crate" "$measured" "$ceiling"
  fi
done

exit "$failed"
