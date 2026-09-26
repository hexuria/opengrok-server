#!/usr/bin/env bash
# Decide which CI suites a run needs. Prints server=, checks=, formal=, web=, docs= (true|false)
# and guard= (empty, or why the branch's name and its diff disagree). docs/setup/gate.md, "CI
# suites", has the table; this is its source.
#
#   server  the whole gate and the smokes      checks  fmt, crate size, architecture, deny, clippy
#   formal  TLC + Lean on formal/              web     the console: typecheck, test, build
#   docs    crates/opengrok/tests/setup_docs.rs alone, compiled with rustc (seconds)
#
# Inputs, from the workflow: EVENT (github.event_name), BRANCH (a pull request's head branch) and
# SUITE (a manual run's choice). Run from a pull_request checkout, where HEAD is the merge commit
# and HEAD^1 the base.
#
# A job skipped by `if:` passes a required check, so a suite is left out only when that is
# certain. Anything this script cannot decide — no base to diff, a failed or empty diff — runs
# the full set, as a push to a pull request always did.
set -uo pipefail

all() { server=true; checks=false; formal=true; web=true; docs=false; }
none() { server=false; checks=false; formal=false; web=false; docs=false; }
guard=""

# Markdown anywhere, and anything under docs/ (screenshots, the diagrams' HTML): nothing builds
# from them. A test that reads one is found by tested_doc below.
is_doc() { case "$1" in *.md | docs/*) return 0 ;; *) return 1 ;; esac; }
is_formal() {
  case "$1" in
    formal/* | scripts/formal.sh | scripts/install-tla.sh | scripts/install-lean.sh) return 0 ;;
    *) return 1 ;;
  esac
}
is_web() { case "$1" in web/*) return 0 ;; *) return 1 ;; esac; }

# A doc counts as tested when a string literal in crates/ names it or a directory above it:
# setup_docs.rs holds docs/setup/, HANDOVER, GOAL, ROADMAP and more to the code. A hand-kept
# list of such files would be right until the next test that reads one.
refs=""
tested_doc() {
  [ -n "${refs}" ] ||
    refs="$(git grep -hoE '"(docs/[^"{ ]*|[A-Za-z0-9_./-]+\.md)' -- crates | tr -d '"' | sort -u)"
  local ref
  while IFS= read -r ref; do
    ref="${ref%/}"
    if [ -n "${ref}" ] && { [ "$1" = "${ref}" ] || [ "${1#"${ref}"/}" != "$1" ]; }; then
      echo "docs: $1 (the tests read ${ref})" >&2
      return 0
    fi
  done <<<"${refs}"
  return 1
}

case "${EVENT:-}" in
  workflow_dispatch)
    none
    case "${SUITE:-all}" in
      all) all; docs=true ;;
      server | checks | formal | web | docs) printf -v "${SUITE}" true ;;
      *) all; echo "unknown suite ${SUITE}; running everything" >&2 ;;
    esac
    ;;
  push)
    # A push to main is a merge its pull request already gated on this exact result; the full
    # gate runs nightly (schedule) to catch what two merges combine.
    none; checks=true; formal=true
    ;;
  pull_request)
    all
    if git rev-parse --verify --quiet HEAD^1 >/dev/null &&
      changed="$(git diff --name-only HEAD^1 HEAD)" && [ -n "${changed}" ]; then
      # The prefix names the suite; every changed file must belong to it or be a doc. A prefix may
      # narrow what runs, never skip the tests for code the branch changed.
      area=""
      case "${BRANCH:-}" in
        doc-* | docs-*) area=doc ;;
        formal-* | tla-*) area=formal ;;
        web-*) area=web ;;
      esac
      outside=""
      only_docs=true
      any_tested=false
      while IFS= read -r path; do
        if is_doc "${path}"; then
          tested_doc "${path}" && any_tested=true
          continue
        fi
        only_docs=false
        case "${area}" in
          formal) is_formal "${path}" && continue ;;
          web) is_web "${path}" && continue ;;
        esac
        outside="${outside} ${path}"
      done <<<"${changed}"

      case "${area}" in
        doc | formal | web)
          none
          if [ -n "${outside}" ]; then
            guard="branch ${BRANCH} runs only the ${area} suite, but it changes:${outside}. Rename the branch (no prefix runs everything) or move those changes."
          else
            case "${area}" in
              doc) docs=true ;;
              formal) formal=true ;;
              web) web=true ;;
            esac
          fi
          ;;
        *)
          if [ "${only_docs}" = true ]; then
            none
            docs="${any_tested}"
            echo "Only docs changed:" >&2
            printf '%s\n' "${changed}" | sed 's/^/  /' >&2
          fi
          ;;
      esac
    fi
    ;;
  *)
    # schedule, and anything new: the full set.
    all
    ;;
esac

echo "server=${server}"
echo "checks=${checks}"
echo "formal=${formal}"
echo "web=${web}"
echo "docs=${docs}"
echo "guard=${guard}"
