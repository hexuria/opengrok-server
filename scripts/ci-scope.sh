#!/usr/bin/env bash
# Print code=false when a pull request changes only prose no build or test reads, else code=true.
#
# Adopted from hexuria/gol. CI's Rust jobs skip when this prints code=false, and a job skipped by
# `if:` passes a required check, so the answer is false only when it is certain: pushes, a
# checkout too shallow to diff, a failed diff and an empty diff all print code=true.
#
# "Only prose" is decided, not listed. A Markdown file counts only if no string literal in
# crates/ names it or a directory above it: crates/opengrok/tests/setup_docs.rs holds
# docs/setup/, HANDOVER, GOAL, ROADMAP and more to the code, so an edit there must run the
# tests that read it. A hand-kept list of such files would be right until the next test that
# reads one. Run from a pull_request checkout, where HEAD is the merge commit and HEAD^1 the base.
set -uo pipefail

code=true
if [ "${EVENT:-}" = pull_request ] && git rev-parse --verify --quiet HEAD^1 >/dev/null; then
  if changed="$(git diff --name-only HEAD^1 HEAD)" && [ -n "${changed}" ]; then
    refs="$(git grep -hoE '"(docs/[^"{ ]*|[A-Za-z0-9_./-]+\.md)' -- crates | tr -d '"' | sort -u)"
    code=false
    while IFS= read -r path; do
      case "${path}" in
        *.md) ;;
        *) code=true; echo "code: ${path}" >&2; break ;;
      esac
      while IFS= read -r ref; do
        ref="${ref%/}"
        if [ -n "${ref}" ] && { [ "${path}" = "${ref}" ] || [ "${path#"${ref}"/}" != "${path}" ]; }; then
          code=true
          echo "code: ${path} (the tests read ${ref})" >&2
          break 2
        fi
      done <<<"${refs}"
    done <<<"${changed}"
    if [ "${code}" = false ]; then
      echo "Only prose no test reads changed:" >&2
      printf '%s\n' "${changed}" | sed 's/^/  /' >&2
    fi
  fi
fi
echo "code=${code}"
