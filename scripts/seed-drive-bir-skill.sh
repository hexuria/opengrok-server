#!/usr/bin/env bash
# Seed the first-class `drive-bir` skill onto a live OpenGrok account.
#
# A skill is owned by an account, not the deployment, so this cannot be a
# migration. POST /skills with a body already creates an enabled row and
# version 1; this script still POSTs a version and PUTs enabled so a re-run
# updates the body and the switch without a second write path.
#
# Auth is the caller's and is never read from the repo: OG_ACCESS_TOKEN
# (Bearer) or OG_COOKIE (Cookie header, typically `og_access=...` from the
# local :1447 session). Dry-run needs neither.

set -euo pipefail

cd "$(dirname "$0")/.."

NAME=drive-bir
BODY_CAP=8000
DESCRIPTION_CAP=300
FILE=docs/skills/drive-bir.md
BASE="${OG_BASE:-http://127.0.0.1:1447}"
DRY_RUN=0

fail() { echo "FAIL: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null || fail "$1 is required"; }

usage() {
  cat <<'EOF'
Seed drive-bir onto the signed-in OpenGrok account.

  ./scripts/seed-drive-bir-skill.sh --dry-run
  OG_ACCESS_TOKEN=… ./scripts/seed-drive-bir-skill.sh
  OG_COOKIE='og_access=…' ./scripts/seed-drive-bir-skill.sh

Options:
  --dry-run     print POST /skills, POST /skills/{id}/versions, PUT enabled
  --base URL    default OG_BASE or http://127.0.0.1:1447
  --file PATH   default docs/skills/drive-bir.md
  -h, --help

Auth (live run only; never commit these):
  OG_ACCESS_TOKEN   Authorization Bearer
  OG_COOKIE         Cookie header. Copy og_access from the local :1447 session.

The standing role is not patched here. After this skill exists, Uriah can
PATCH /coworkers/{id} with a role that names drive-bir — only when he confirms
the text.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --base)
      [[ $# -ge 2 ]] || fail "--base needs a URL"
      BASE=$2
      shift 2
      ;;
    --file)
      [[ $# -ge 2 ]] || fail "--file needs a path"
      FILE=$2
      shift 2
      ;;
    -h|--help) usage; exit 0 ;;
    *) fail "unknown argument: $1" ;;
  esac
done

BASE="${BASE%/}"

need python3
need curl
need jq
[[ -f "$FILE" ]] || fail "no skill file at $FILE"

parsed=$(python3 - "$FILE" "$NAME" "$BODY_CAP" "$DESCRIPTION_CAP" <<'PY'
import json, sys

path, expected_name, body_cap, description_cap = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
text = open(path, encoding="utf-8").read()
if text.startswith("\ufeff"):
    text = text[1:]
trimmed = text.lstrip()
name = None
description = None
closed = True
if trimmed.startswith("---"):
    consumed = None
    seen = 0
    for index, line in enumerate(trimmed.splitlines(keepends=True)):
        seen += len(line)
        if index == 0:
            continue
        if line.rstrip("\r\n") == "---":
            consumed = seen
            break
        content = line.rstrip("\r\n")
        if content.startswith("name:"):
            name = content[5:].strip().strip("\"'")
        if content.startswith("description:"):
            description = content[12:].strip().strip("\"'")
    if consumed is None:
        closed = False
        body = trimmed
    else:
        body = trimmed[consumed:].strip()
else:
    body = trimmed.strip()

errors = []
if not closed:
    errors.append("SKILL.md opens with --- and never closes it")
if not name:
    errors.append("frontmatter needs a name")
elif name != expected_name:
    errors.append(f"frontmatter name is {name!r}, expected {expected_name!r}")
if not description:
    errors.append("frontmatter needs a description")
body_chars = len(body)
description_chars = len(description or "")
if body_chars > body_cap:
    errors.append(f"the skill body is {body_chars} characters, over the {body_cap} allowed")
if description_chars > description_cap:
    errors.append(f"the skill description is {description_chars} characters, over the {description_cap} allowed")
if not body.strip():
    errors.append("a version needs a body")

out = {
    "name": name or "",
    "description": description or "",
    "body": body,
    "bodyChars": body_chars,
    "descriptionChars": description_chars,
    "errors": errors,
}
print(json.dumps(out, ensure_ascii=False))
if errors:
    print("FAIL: " + "; ".join(errors), file=sys.stderr)
    sys.exit(2)
PY
) || fail "skill file $FILE is not seedable"

name=$(printf '%s' "$parsed" | jq -r .name)
description=$(printf '%s' "$parsed" | jq -r .description)
body=$(printf '%s' "$parsed" | jq -r .body)
body_chars=$(printf '%s' "$parsed" | jq -r .bodyChars)
description_chars=$(printf '%s' "$parsed" | jq -r .descriptionChars)

create_json=$(jq -n \
  --arg name "$name" \
  --arg description "$description" \
  --arg body "$body" \
  '{name: $name, description: $description, body: $body, source: "authored"}')
version_json=$(jq -n \
  --arg body "$body" \
  --arg note "seed from $FILE" \
  '{body: $body, note: $note, kind: "authored"}')
enable_json='{"enabled": true}'

echo "body characters: $body_chars (cap $BODY_CAP)"
echo "description characters: $description_chars (cap $DESCRIPTION_CAP)"
echo

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "POST /skills"
  printf '%s\n' "$create_json" | jq .
  echo
  echo "POST /skills/{id}/versions"
  printf '%s\n' "$version_json" | jq .
  echo
  echo "PUT /skills/{id}"
  printf '%s\n' "$enable_json" | jq .
  echo
  echo "dry-run: no request sent to $BASE"
  exit 0
fi

auth_args=()
if [[ -n "${OG_ACCESS_TOKEN:-}" ]]; then
  auth_args=(-H "Authorization: Bearer ${OG_ACCESS_TOKEN}")
elif [[ -n "${OG_COOKIE:-}" ]]; then
  cookie=${OG_COOKIE#Cookie: }
  cookie=${cookie#cookie: }
  case "$cookie" in
    og_access=*|*og_access=*) ;;
    *) cookie="og_access=${cookie}" ;;
  esac
  auth_args=(-H "Cookie: ${cookie}")
else
  fail "set OG_ACCESS_TOKEN or OG_COOKIE (og_access from the local :1447 session). Do not put it in the repo."
fi

# Status via -w, body via -o: one stream would glue the JSON to the code.
og_call() {
  local method=$1
  local path=$2
  local payload=${3:-}
  local tmp
  tmp=$(mktemp)
  local args=(-sS -o "$tmp" -w '%{http_code}' -X "$method" "${auth_args[@]}" "$BASE$path")
  if [[ -n "$payload" ]]; then
    args+=(-H 'content-type: application/json' --data-binary "$payload")
  fi
  local code
  code=$(curl "${args[@]}") || {
    rm -f "$tmp"
    fail "curl $method $path failed"
  }
  OG_HTTP_CODE=$code
  OG_HTTP_BODY=$(cat "$tmp")
  rm -f "$tmp"
}

og_call GET "/skills?filter=mine"
[[ "$OG_HTTP_CODE" == "200" ]] || fail "GET /skills?filter=mine -> $OG_HTTP_CODE ${OG_HTTP_BODY}"
printf '%s' "$OG_HTTP_BODY" | jq -e 'type == "array"' >/dev/null \
  || fail "GET /skills?filter=mine is not an array: $OG_HTTP_BODY"

id=$(printf '%s' "$OG_HTTP_BODY" | jq -r --arg name "$name" '[.[] | select(.name == $name) | .id] | first // empty')

if [[ -z "$id" || "$id" == "null" ]]; then
  echo "POST /skills"
  og_call POST /skills "$create_json"
  if [[ "$OG_HTTP_CODE" == "409" ]]; then
    og_call GET "/skills?filter=mine"
    [[ "$OG_HTTP_CODE" == "200" ]] || fail "GET /skills after 409 -> $OG_HTTP_CODE ${OG_HTTP_BODY}"
    id=$(printf '%s' "$OG_HTTP_BODY" | jq -r --arg name "$name" '[.[] | select(.name == $name) | .id] | first // empty')
    [[ -n "$id" && "$id" != "null" ]] || fail "POST /skills 409 but $name is not in GET /skills?filter=mine"
  else
    [[ "$OG_HTTP_CODE" == "200" ]] || fail "POST /skills -> $OG_HTTP_CODE ${OG_HTTP_BODY}"
    id=$(printf '%s' "$OG_HTTP_BODY" | jq -r .id)
    [[ -n "$id" && "$id" != "null" ]] || fail "POST /skills returned no id: $OG_HTTP_BODY"
  fi
else
  echo "POST /skills skipped ($name already exists as $id)"
fi

echo "POST /skills/${id}/versions"
og_call POST "/skills/${id}/versions" "$version_json"
[[ "$OG_HTTP_CODE" == "200" ]] || fail "POST /skills/${id}/versions -> $OG_HTTP_CODE ${OG_HTTP_BODY}"
version=$(printf '%s' "$OG_HTTP_BODY" | jq -r .version)
[[ "$version" != "null" && -n "$version" ]] || fail "version reply has no version: $OG_HTTP_BODY"

echo "PUT /skills/${id} enabled=true"
og_call PUT "/skills/${id}" "$enable_json"
[[ "$OG_HTTP_CODE" == "200" ]] || fail "PUT /skills/${id} -> $OG_HTTP_CODE ${OG_HTTP_BODY}"
printf '%s' "$OG_HTTP_BODY" | jq -e --arg name "$name" '
  .name == $name
  and .enabled == true
  and .draft == false
  and (.versionCount | type == "number")
  and .versionCount >= 1
  and (.body | type == "string")
  and (.body | length) > 0
' >/dev/null || fail "enabled skill shape is wrong: $OG_HTTP_BODY"

printf '%s\n' "$OG_HTTP_BODY" | jq '{id, name, enabled, draft, version, versionCount, description, bodyChars: (.body | length)}'
echo "seeded $name as $id version $version on $BASE"
