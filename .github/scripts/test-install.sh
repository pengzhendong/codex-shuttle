#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
temporary=$(mktemp -d "${TMPDIR:-/tmp}/cxs-installer-test.XXXXXX")
trap 'rm -rf "$temporary"' EXIT
fake_bin="$temporary/bin"
install_dir="$temporary/install"
mkdir -p "$fake_bin"

cat >"$fake_bin/uname" <<'EOF'
#!/bin/sh
case "$1" in
  -s) printf '%s\n' Darwin ;;
  -m) printf '%s\n' arm64 ;;
  *) exit 1 ;;
esac
EOF

cat >"$fake_bin/codex" <<'EOF'
#!/bin/sh
printf '%s\n' 'codex-cli 1.2.3-alpha.4'
EOF

cat >"$fake_bin/curl" <<'EOF'
#!/bin/sh
while test "$#" -gt 0; do
  case "$1" in
    --output) output=$2; shift 2 ;;
    -*) shift ;;
    *) url=$1; shift ;;
  esac
done
case "$url" in
  */releases.atom)
    printf '%s\n' '<feed><id>v9.8.7-codex.1.2.3</id></feed>' >"$output"
    ;;
  */v9.8.7-codex.1.2.3/cxs-cli-macos-aarch64)
    printf '%s\n' 'fake cxs binary' >"$output"
    ;;
  */v9.8.7-codex.1.2.3/SHA256SUMS)
    printf '%s\n' 'test-checksum  cxs-cli-macos-aarch64' >"$output"
    ;;
  *)
    printf 'unexpected URL: %s\n' "$url" >&2
    exit 1
    ;;
esac
EOF

cat >"$fake_bin/shasum" <<'EOF'
#!/bin/sh
printf 'test-checksum  %s\n' "$3"
EOF

chmod +x "$fake_bin/uname" "$fake_bin/codex" "$fake_bin/curl" "$fake_bin/shasum"

output=$(PATH="$fake_bin:$PATH" CXS_CODEX_PATH="$fake_bin/codex" \
  CXS_INSTALL_DIR="$install_dir" sh "$root/install.sh")
grep -Fq 'Selected Shuttle release v9.8.7-codex.1.2.3.' <<<"$output"
grep -Fq 'fake cxs binary' "$install_dir/cxs"
