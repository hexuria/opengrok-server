#!/usr/bin/env bash
# Install the pinned Lean 4 release at ~/.local/lean and print its bin directory.
#
# Not elan: elan fetches toolchains from release.lean-lang.org, which sandboxed runners (the
# Claude Code cloud containers among them) cannot reach, and it resolves the version at run
# time. formal/lean uses Lean 4 core only, so the release archive on GitHub is the whole
# toolchain. The archive is refused unless its sha256 matches; formal/lean/lean-toolchain names
# the same version for editors, and the two move together.
set -euo pipefail

version="4.23.0"
sha256="ecd028d6f642b61b451c8687aeeb24dd53789fbfdcb7d4adb8f5cf60eb2022ba"
url="https://github.com/leanprover/lean4/releases/download/v${version}/lean-${version}-linux.tar.zst"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) ;;
  *)
    echo "install-lean.sh pins the linux x86_64 archive only." >&2
    echo "Install Lean ${version} from https://github.com/leanprover/lean4/releases/tag/v${version}" >&2
    exit 1
    ;;
esac

prefix="${HOME}/.local/lean"
bin="${prefix}/lean-${version}-linux/bin"
if [ -x "${bin}/lean" ] && "${bin}/lean" --version | grep -q "version ${version},"; then
  echo "${bin}"
  exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT
curl -fsSL -o "${tmp}/lean.tar.zst" "${url}"
echo "${sha256}  ${tmp}/lean.tar.zst" | sha256sum -c --quiet - >&2
mkdir -p "${prefix}"
rm -rf "${prefix:?}/lean-${version}-linux"
tar --zstd -xf "${tmp}/lean.tar.zst" -C "${prefix}"
"${bin}/lean" --version >&2
echo "${bin}"
