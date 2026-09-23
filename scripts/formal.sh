#!/usr/bin/env bash
# Re-run the harness's formal models: every TLC configuration in formal/tla, and the Lean proofs.
#
# A configuration whose first comment line says EXPECTED TO FAIL is the counterexample a fix
# exists for (or a limit the design states); it passes this script only while TLC still finds
# the violation. Everything else must check clean. formal/README.md says what each one shows.
#
# Not part of the merge gate: TLC needs a JVM and Lean its toolchain, which CI does not carry.
# Skips loudly without them.
#
# Usage:
#   TLA2TOOLS_JAR=/path/tla2tools.jar LEAN=/path/lean scripts/formal.sh
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

status=0
if [ -z "${TLA2TOOLS_JAR:-}" ] || ! command -v java >/dev/null 2>&1; then
  echo "skipping TLA+: set TLA2TOOLS_JAR to tla2tools.jar (github.com/tlaplus/tlaplus releases) and install java"
else
  for cfg in formal/tla/*.cfg; do
    base=$(basename "$cfg" .cfg)
    spec="formal/tla/${base%%_*}.tla"
    expect=pass
    head -1 "$cfg" | grep -q 'EXPECTED TO FAIL' && expect=fail
    out=$(cd formal/tla && java -XX:+UseParallelGC -cp "$TLA2TOOLS_JAR" tlc2.TLC -workers auto \
      -metadir "${TMPDIR:-/tmp}/tlc-$base" -config "$(basename "$cfg")" "$(basename "$spec")" 2>&1)
    if grep -q 'No error has been found' <<<"$out"; then got=pass
    elif grep -qE 'is violated|were violated' <<<"$out"; then got=fail
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

LEAN_BIN=${LEAN:-lean}
if ! command -v "$LEAN_BIN" >/dev/null 2>&1; then
  echo "skipping Lean: set LEAN to a Lean 4 binary (github.com/leanprover/lean4 releases)"
elif (cd formal/lean && "$LEAN_BIN" Harness.lean); then
  echo "ok    Harness.lean                     proved"
else
  echo "FAIL  Harness.lean"
  status=1
fi
exit $status
