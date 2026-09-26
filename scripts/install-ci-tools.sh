#!/usr/bin/env bash
# Install the pinned cargo-deny and cargo-nextest binaries into ~/.local/bin.
#
# Release binaries, not `cargo install`: compiling either takes minutes of a CI job, and
# `cargo install` resolves its own dependencies afresh each time. Each archive is refused unless
# its sha256 matches, so a replaced release asset cannot run in CI. scripts/gate.sh uses both
# when they are on PATH, so a desk that runs this gets the same gate CI does.
set -euo pipefail

deny_version="0.20.2"
deny_sha256="9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f"
nextest_version="0.9.146"
nextest_sha256="682c21b777c333e96fd532e114d3a5a894e0729ab88d94c0a9f20f8419695428"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) ;;
  *)
    echo "install-ci-tools.sh pins the linux x86_64 archives only; use cargo install" >&2
    exit 1
    ;;
esac

bin="${HOME}/.local/bin"
mkdir -p "${bin}"
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

fetch() { # url sha256 dest
  curl -fsSL -o "$3" "$1"
  echo "$2  $3" | sha256sum -c --quiet -
}

if ! "${bin}/cargo-deny" --version 2>/dev/null | grep -qx "cargo-deny ${deny_version}"; then
  name="cargo-deny-${deny_version}-x86_64-unknown-linux-musl"
  fetch "https://github.com/EmbarkStudios/cargo-deny/releases/download/${deny_version}/${name}.tar.gz" \
    "${deny_sha256}" "${tmp}/deny.tgz"
  tar -xzf "${tmp}/deny.tgz" -C "${tmp}"
  install -m 755 "${tmp}/${name}/cargo-deny" "${bin}/cargo-deny"
fi

if ! "${bin}/cargo-nextest" nextest --version 2>/dev/null | grep -q "^cargo-nextest ${nextest_version} "; then
  fetch "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-${nextest_version}/cargo-nextest-${nextest_version}-x86_64-unknown-linux-gnu.tar.gz" \
    "${nextest_sha256}" "${tmp}/nextest.tgz"
  tar -xzf "${tmp}/nextest.tgz" -C "${bin}"
fi

"${bin}/cargo-deny" --version
"${bin}/cargo-nextest" nextest --version | head -1
echo "${bin}"
