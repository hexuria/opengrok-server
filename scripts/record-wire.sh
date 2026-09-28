#!/usr/bin/env bash
# The wire corpus NativeChat vendors (#255): every AG-UI frame and REST body it reads, recorded
# from this server's own tests, in tests/fixtures/wire/.
#
#   scripts/record-wire.sh             record the corpus and write it to tests/fixtures/wire/
#   scripts/record-wire.sh --check     record it and fail if tests/fixtures/wire/ is stale
#   scripts/record-wire.sh --from DIR  build from a recording already made (the gate's test run
#                                      sets OG_RECORD_WIRE=DIR), with or without --check
#
# NEEDS POSTGRES AND NEXTEST. Most frames come from tests that skip without OG_DATABASE_URL, and
# a test's name is read from nextest's `--exact`: without them the corpus is short or misnamed,
# and --check fails for a reason that is the environment, not the code.
set -euo pipefail
cd "$(dirname "$0")/.." || exit 1

check=0
from=""
while [ $# -gt 0 ]; do
  case "$1" in
    --check) check=1 ;;
    --from) from="$2"; shift ;;
    *) printf 'unknown option: %s\n' "$1" >&2; exit 2 ;;
  esac
  shift
done

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

if [ -z "$from" ]; then
  : "${OG_DATABASE_URL:?the wire corpus needs OG_DATABASE_URL: most frames come from tests that skip without it}"
  command -v cargo-nextest >/dev/null 2>&1 || { echo "the wire corpus needs cargo-nextest (scripts/install-ci-tools.sh)" >&2; exit 1; }
  from="$scratch/recording"
  # The whole workspace, as the gate runs it: the feature is unified across it, so any crate's
  # tests that drive the router record too, and the two recordings must be the same.
  OG_RECORD_WIRE="$from" cargo nextest run --workspace --no-fail-fast
fi

# server_sha names the code that produced the corpus, which NativeChat pins what it vendors by:
# the last commit that changed the server, the recorder or the corpus builder. Not HEAD: the
# commit that only adds the corpus would otherwise stamp a sha, and a builder fix would leave two
# different corpora under one.
sha=$(git log -1 --format=%H -- crates Cargo.lock scripts/record-wire.sh)
if ! git diff --quiet HEAD -- crates Cargo.lock scripts/record-wire.sh; then
  echo "note: the code has uncommitted changes, so server_sha $sha does not name exactly what was recorded" >&2
fi
cargo run -q -p opengrok-server --example wire_corpus -- \
  build "$from" "$scratch/corpus" "$sha"

if [ "$check" = 1 ]; then
  cargo run -q -p opengrok-server --example wire_corpus -- check "$scratch/corpus" tests/fixtures/wire
  echo "the wire corpus is current"
else
  rm -rf tests/fixtures/wire
  mkdir -p tests/fixtures
  cp -R "$scratch/corpus" tests/fixtures/wire
  echo "wrote tests/fixtures/wire/"
fi
