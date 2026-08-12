#!/bin/sh

set -eu

REPOSITORY="pengzhendong/codex-shuttle"
SHUTTLE_VERSION="0.1.1"
INSTALL_DIR=${CXS_INSTALL_DIR:-"$HOME/.local/bin"}
BUNDLED_CODEX="/Applications/ChatGPT.app/Contents/Resources/codex"

usage() {
  cat <<'EOF'
Install Codex Shuttle on macOS.

Usage:
  install.sh [--install-dir <directory>]

Options:
  --install-dir  Destination directory (default: ~/.local/bin)
  -h, --help     Show this help

Environment:
  CXS_INSTALL_DIR  Same as --install-dir
EOF
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

while test "$#" -gt 0; do
  case "$1" in
    --install-dir)
      test "$#" -ge 2 || fail "--install-dir requires a value"
      INSTALL_DIR=$2
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1"
      ;;
  esac
done

test "$(uname -s)" = Darwin || fail "the cxs CLI currently supports macOS only"

case "$(uname -m)" in
  arm64) target=aarch64-apple-darwin ;;
  x86_64) target=x86_64-apple-darwin ;;
  *) fail "unsupported Mac architecture: $(uname -m)" ;;
esac

download() {
  url=$1
  output=$2
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 --output "$output" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$output" "$url"
  else
    fail "curl or wget is required"
  fi
}

test -x "$BUNDLED_CODEX" ||
  fail "ChatGPT Desktop Codex was not found at $BUNDLED_CODEX"
codex_version=$("$BUNDLED_CODEX" --version 2>/dev/null | grep -Eo '[0-9]+\.[0-9]+\.[0-9]+' | head -n 1 || true)
test -n "$codex_version" ||
  fail "could not detect the bundled ChatGPT Desktop Codex version"
release_tag="v${SHUTTLE_VERSION}-codex.${codex_version}"
printf 'Detected bundled ChatGPT Desktop Codex %s.\n' "$codex_version"

asset="cxs-${target}"
release_url="https://github.com/${REPOSITORY}/releases/download/${release_tag}"

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/cxs-install.XXXXXX")
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM

printf 'Downloading %s...\n' "$asset"
download "$release_url/$asset" "$work_dir/$asset" ||
  fail "release $release_tag is unavailable; see https://github.com/${REPOSITORY}/releases"
download "$release_url/SHA256SUMS" "$work_dir/SHA256SUMS" ||
  fail "could not download SHA256SUMS for $release_tag"

expected=$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print $1; exit }' "$work_dir/SHA256SUMS")
test -n "$expected" || fail "$asset is missing from SHA256SUMS"

if command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$work_dir/$asset" | awk '{ print $1 }')
elif command -v openssl >/dev/null 2>&1; then
  actual=$(openssl dgst -sha256 "$work_dir/$asset" | awk '{ print $NF }')
else
  fail "shasum or openssl is required to verify the download"
fi
test "$actual" = "$expected" || fail "SHA-256 checksum mismatch"

mkdir -p "$INSTALL_DIR"
install -m 0755 "$work_dir/$asset" "$INSTALL_DIR/cxs"

printf 'Installed cxs to %s/cxs\n' "$INSTALL_DIR"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    printf 'Add it to your PATH with:\n  export PATH="%s:$PATH"\n' "$INSTALL_DIR"
    ;;
esac
printf 'Run cxs --version to verify the installation.\n'
