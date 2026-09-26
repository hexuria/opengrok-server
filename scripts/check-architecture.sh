#!/usr/bin/env bash
# Fail when the crate graph leaves what scripts/architecture.txt allows.
#
# Adopted from hexuria/gol. Reads `cargo metadata` (no build) and walks normal and build
# dependencies, so it takes seconds and runs in scripts/gate.sh as well as CI's `architecture`
# job. A workspace crate missing from the rules file fails too: a new crate starts with its
# edges written down, not with none checked.
set -euo pipefail

cd "$(dirname "$0")/.."

meta="$(mktemp)"
trap 'rm -f "${meta}"' EXIT
cargo metadata --format-version 1 --all-features >"${meta}"

python3 - "${meta}" scripts/architecture.txt <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    meta = json.load(handle)

rules = {}
current = None
with open(sys.argv[2], encoding="utf-8") as handle:
    for raw in handle:
        line = raw.split("#", 1)[0].rstrip()
        if not line.strip():
            continue
        if line.lstrip().startswith("|"):
            rules[current]["banned"] = set(line.split("|", 1)[1].split())
            continue
        crate, _, deps = line.partition(":")
        current = crate.strip()
        rules[current] = {"edges": set(deps.split()), "banned": set()}

packages = {package["id"]: package for package in meta["packages"]}
nodes = {node["id"]: node for node in meta["resolve"]["nodes"]}
workspace = {packages[member]["name"]: member for member in meta["workspace_members"]}


def runtime_deps(package_id):
    return [
        dep["pkg"]
        for dep in nodes[package_id]["deps"]
        if any(kind["kind"] in (None, "build") for kind in dep["dep_kinds"])
    ]


def reachable(root_id):
    seen = set()
    stack = [root_id]
    while stack:
        current_id = stack.pop()
        if current_id in seen:
            continue
        seen.add(current_id)
        stack.extend(runtime_deps(current_id))
    seen.discard(root_id)
    return seen


failed = False
for crate, package_id in sorted(workspace.items()):
    rule = rules.get(crate)
    if rule is None:
        print(f"{crate} is not in scripts/architecture.txt: write down its edges", file=sys.stderr)
        failed = True
        continue
    edges = {
        packages[dep]["name"] for dep in runtime_deps(package_id) if packages[dep]["name"] in workspace
    }
    for extra in sorted(edges - rule["edges"]):
        print(f"{crate} -> {extra} is a new edge; architecture.txt does not allow it", file=sys.stderr)
        failed = True
    for gone in sorted(rule["edges"] - edges):
        print(f"{crate} -> {gone} is gone from the code: delete it from architecture.txt", file=sys.stderr)
        failed = True
    reached = {packages[dep]["name"] for dep in reachable(package_id)}
    for banned in sorted(reached & rule["banned"]):
        print(f"{crate} reaches {banned}, which architecture.txt bans for it", file=sys.stderr)
        failed = True

for crate in sorted(set(rules) - set(workspace)):
    print(f"architecture.txt lists {crate}, which is not a workspace crate", file=sys.stderr)
    failed = True

if failed:
    sys.exit(1)
print(f"architecture ok ({len(workspace)} crates)")
PY
