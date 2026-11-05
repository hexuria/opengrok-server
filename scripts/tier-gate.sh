#!/usr/bin/env bash
# Whether one tier of a cross-repo train is ready to merge (#220): run by a person, by hand.
#
#   scripts/tier-gate.sh gol/form-mode-t1
#
# For each repo of the train it finds the open PR whose head is <branch>, says whether every
# required check on it is green, and compares the sha256 of the form-mode fixtures across the
# repos that have them: the three repos agree on the card only if they vendor the same bytes.
# It ends with one line, READY or BLOCKED, and the reasons.
#
# READ-ONLY, ON PURPOSE. The owner merges tiers by hand, so this never posts a status, comments,
# approves or merges anything. It uses your own `gh` login and needs no secret of its own.
#
# A repo with no open PR on the branch is fine: that tier changes nothing there. A repo's
# fixtures live where TIER_GATE_FIXTURES_<REPO> says (the repo name upper-cased, dashes as
# underscores), by default tests/fixtures/form-mode.
set -euo pipefail

branch="${1:-}"
if [ -z "$branch" ] || [ "$#" -ne 1 ]; then
  echo "usage: scripts/tier-gate.sh <branch>   e.g. scripts/tier-gate.sh gol/form-mode-t1" >&2
  exit 2
fi
command -v gh >/dev/null 2>&1 || { echo "tier-gate needs the gh CLI, signed in (gh auth login)" >&2; exit 2; }
command -v sha256sum >/dev/null 2>&1 || { echo "tier-gate needs sha256sum" >&2; exit 2; }

repos=(hexuria/opengrok-server hexuria/buwiz-forms hexuria/opengrok)
reasons=()
digests=()
prs=0

fixtures_dir() {
  local name var
  name="${1#*/}"
  var="TIER_GATE_FIXTURES_$(printf '%s' "$name" | tr '[:lower:]-' '[:upper:]_')"
  printf '%s\n' "${!var:-tests/fixtures/form-mode}"
}

# The sha256 of a repo's form-mode fixtures at one commit: every file's own sha256 and path,
# sorted, hashed again. Empty when the repo has none there.
fixtures_digest() {
  local repo="$1" sha="$2" dir="$3" listing path
  if ! listing=$(gh api "repos/$repo/contents/$dir?ref=$sha" --jq '.[] | select(.type == "file") | .path' 2>/dev/null); then
    return 0
  fi
  [ -n "$listing" ] || return 0
  while IFS= read -r path; do
    [ -n "$path" ] || continue
    local sum
    sum=$(gh api -H "Accept: application/vnd.github.raw" "repos/$repo/contents/$path?ref=$sha" | sha256sum | cut -d' ' -f1)
    printf '%s  %s\n' "$sum" "${path#"$dir"/}"
  done <<<"$listing" | LC_ALL=C sort | sha256sum | cut -d' ' -f1
}

for repo in "${repos[@]}"; do
  echo "== $repo"
  pr=$(gh pr list --repo "$repo" --head "$branch" --state open --json number,headRefOid,url \
    --jq '.[0] | select(. != null) | "\(.number) \(.headRefOid) \(.url)"' 2>/dev/null) || {
    echo "   cannot read its pull requests (is gh signed in, and may you read $repo?)"
    reasons+=("$repo: pull requests unreadable")
    continue
  }
  if [ -z "$pr" ]; then
    echo "   no PR"
    continue
  fi
  read -r number sha url <<<"$pr"
  prs=$((prs + 1))
  echo "   PR #$number $url"
  echo "   head $sha"

  # `gh pr checks` exits 0 when all pass, 8 while any is pending and 1 when any failed, or when
  # it has nothing to report, which is read from its words rather than its status.
  set +e
  checks=$(gh pr checks "$number" --repo "$repo" --required 2>&1)
  status=$?
  set -e
  scope="required"
  if grep -qi "no required checks" <<<"$checks"; then
    set +e
    checks=$(gh pr checks "$number" --repo "$repo" 2>&1)
    status=$?
    set -e
    scope="all (none are required)"
  fi
  if grep -qi "no checks reported" <<<"$checks"; then
    echo "   checks: none reported"
    reasons+=("$repo#$number: no checks reported")
  elif [ "$status" -eq 0 ]; then
    echo "   checks ($scope): green"
  elif [ "$status" -eq 8 ]; then
    echo "   checks ($scope): pending"
    reasons+=("$repo#$number: checks pending")
  else
    echo "   checks ($scope): not green"
    awk -F'\t' '$2 != "pass" && NF > 1 { print "     " $1 ": " $2 }' <<<"$checks"
    reasons+=("$repo#$number: checks not green")
  fi

  dir=$(fixtures_dir "$repo")
  digest=$(fixtures_digest "$repo" "$sha" "$dir")
  if [ -n "$digest" ]; then
    echo "   form-mode fixtures ($dir): sha256 $digest"
    digests+=("$digest $repo")
  else
    echo "   form-mode fixtures ($dir): none"
  fi
done

echo
[ "$prs" -gt 0 ] || reasons+=("no open PR on $branch in any repo")
distinct=$(printf '%s\n' "${digests[@]:-}" | awk 'NF { print $1 }' | LC_ALL=C sort -u | wc -l | tr -d ' ')
if [ "${#digests[@]}" -gt 1 ] && [ "$distinct" -gt 1 ]; then
  reasons+=("form-mode fixtures differ: $(printf '%s; ' "${digests[@]}")")
elif [ "${#digests[@]}" -gt 1 ]; then
  echo "form-mode fixtures: the same in ${#digests[@]} repos"
fi

if [ "${#reasons[@]}" -eq 0 ]; then
  echo "READY: $branch"
else
  printf 'BLOCKED: %s: %s\n' "$branch" "$(printf '%s; ' "${reasons[@]}" | sed 's/; $//')"
  exit 1
fi
