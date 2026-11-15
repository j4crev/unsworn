#!/bin/sh
# Installs the latest Unsworn release.
#
#   curl -fsSL https://raw.githubusercontent.com/j4crev/unsworn/main/install.sh | sh
#
# Options (environment variables):
#   UNSWORN_VERSION      a release tag such as v0.1.0 (default: latest)
#   UNSWORN_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   UNSWORN_VERIFY       1 to check the release's build attestation, which
#                        needs the GitHub CLI (`gh`), signed in
#
# The checksum below guards against a broken download. It comes from the
# same release as the archive, so it can't tell whether the release itself
# was tampered with; the attestation can.
set -eu

repo="j4crev/unsworn"
version="${UNSWORN_VERSION:-latest}"
install_dir="${UNSWORN_INSTALL_DIR:-$HOME/.local/bin}"

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Linux) os_part="unknown-linux-musl" ;;
  *) echo "unsworn: unsupported OS: $os" >&2; exit 1 ;;
esac
case "$arch" in
  x86_64 | amd64) arch_part="x86_64" ;;
  arm64 | aarch64) arch_part="aarch64" ;;
  *) echo "unsworn: unsupported architecture: $arch" >&2; exit 1 ;;
esac
target="${arch_part}-${os_part}"
asset="unsworn-${target}.tar.gz"

if [ "$version" = "latest" ]; then
  base="https://github.com/${repo}/releases/latest/download"
else
  base="https://github.com/${repo}/releases/download/${version}"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "unsworn: downloading ${asset} (${version})"
curl -fsSL "${base}/${asset}" -o "${tmp}/${asset}"
curl -fsSL "${base}/${asset}.sha256" -o "${tmp}/${asset}.sha256"

expected=$(cut -d ' ' -f 1 "${tmp}/${asset}.sha256")
actual=$(sha256sum "${tmp}/${asset}" | cut -d ' ' -f 1)
if [ "$expected" != "$actual" ]; then
  echo "unsworn: checksum mismatch for ${asset}" >&2
  exit 1
fi

if [ "${UNSWORN_VERIFY:-0}" = "1" ]; then
  if ! command -v gh >/dev/null 2>&1; then
    echo "unsworn: UNSWORN_VERIFY=1 needs the GitHub CLI (https://cli.github.com)" >&2
    exit 1
  fi
  echo "unsworn: verifying the build attestation"
  if ! gh attestation verify "${tmp}/${asset}" --repo "$repo" >/dev/null; then
    echo "unsworn: ${asset} has no valid build attestation from ${repo}" >&2
    exit 1
  fi
fi

tar -xzf "${tmp}/${asset}" -C "$tmp"
mkdir -p "$install_dir"
install -m 755 "${tmp}/unsworn" "${install_dir}/unsworn"
echo "unsworn: installed to ${install_dir}/unsworn"

case ":$PATH:" in
  *":${install_dir}:"*) ;;
  *) echo "unsworn: add ${install_dir} to your PATH" ;;
esac
"${install_dir}/unsworn" --version
