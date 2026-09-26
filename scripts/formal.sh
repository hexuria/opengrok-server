#!/usr/bin/env bash
# Re-run the harness's formal models: the Lean proofs, and every TLC configuration in formal/tla.
#
# A configuration whose first comment line says EXPECTED TO FAIL is the counterexample a fix
# exists for (or a limit the design states); it passes this script only while TLC still finds
# the violation, and only while it is the property named on its `\* VIOLATES:` line: a kept
# counterexample that starts failing for some other reason (a typo in the model, a new
# invariant it trips first) no longer shows what it was kept to show. Everything else must
# check clean. formal/README.md says what each one shows.
#
# CI's `formal` job runs this with --require, after scripts/install-tla.sh and
# scripts/install-lean.sh, on every change that is not docs only. It takes under a minute. It
# used to be local only ("CI does not carry a JVM"), so a change to the loop or the run
# lifecycle could break a property TLC had proved and still merge green. Without --require a
# missing tool is skipped loudly, so a desk without Java can still run the half it has.
#
# Usage:
#   scripts/install-tla.sh && scripts/install-lean.sh && scripts/formal.sh --require
#   TLA2TOOLS_JAR=/path/tla2tools.jar LEAN=/path/lean scripts/formal.sh
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

require=false
[ "${1:-}" = --require ] && require=true
missing() {
  echo "$1"
  if $require; then
    echo "FAIL  --require: $2 is not installed" >&2
    status=1
  fi
}

TLA2TOOLS_JAR=${TLA2TOOLS_JAR:-$HOME/.local/tla/tla2tools.jar}
if [ -z "${LEAN:-}" ] && ! command -v lean >/dev/null 2>&1; then
  LEAN=$(ls -d "$HOME"/.local/lean/lean-*/bin/lean 2>/dev/null | tail -1)
fi

status=0
LEAN_BIN=${LEAN:-lean}
if ! command -v "$LEAN_BIN" >/dev/null 2>&1; then
  missing "skipping Lean: run scripts/install-lean.sh (or set LEAN to a Lean 4 binary)" "Lean"
elif (cd formal/lean && "$LEAN_BIN" Harness.lean); then
  echo "ok    Harness.lean                     proved"
else
  echo "FAIL  Harness.lean"
  status=1
fi

if [ ! -f "$TLA2TOOLS_JAR" ] || ! command -v java >/dev/null 2>&1; then
  missing "skipping TLA+: run scripts/install-tla.sh (or set TLA2TOOLS_JAR) and install java" "TLC"
else
  for cfg in formal/tla/*.cfg; do
    base=$(basename "$cfg" .cfg)
    spec="formal/tla/${base%%_*}.tla"
    expect=pass
    head -1 "$cfg" | grep -q 'EXPECTED TO FAIL' && expect=fail
    violates=$(sed -n 's/^\\\* VIOLATES: *//p' "$cfg" | head -1)
    if [ "$expect" = fail ] && [ -z "$violates" ]; then
      printf 'FAIL  %-32s EXPECTED TO FAIL without a \\* VIOLATES: line naming the property\n' "$base"
      status=1
      continue
    fi
    out=$(cd formal/tla && java -XX:+UseParallelGC -cp "$TLA2TOOLS_JAR" tlc2.TLC -workers auto \
      -metadir "${TMPDIR:-/tmp}/tlc-$base" -config "$(basename "$cfg")" "$(basename "$spec")" 2>&1)
    if grep -q 'No error has been found' <<<"$out"; then got=pass
    elif [ "$expect" = fail ] && grep -q "Invariant $violates is violated" <<<"$out"; then got=fail
    elif grep -qE 'is violated|were violated' <<<"$out"; then got="another violation"
    else got=error; fi
    states=$(grep -oE '[0-9]+ distinct states found' <<<"$out" | tail -1)
    if [ "$got" = "$expect" ]; then
      printf 'ok    %-32s %-5s (%s)\n' "$base" "$got" "$states"
    else
      printf 'FAIL  %-32s expected %s, got %s\n' "$base" "$expect" "$got"
      grep -E 'Error|violated' <<<"$out" | head -5
      status=1
    fi
  done
  rm -f formal/tla/*_TTrace_*
fi

exit $status
