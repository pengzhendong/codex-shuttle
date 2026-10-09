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

# Rewrite only the fixed app resources root in a disposable copy, so these
# exercise the installer itself without creating files under /Applications.
resources="$temporary/App Bundle.app/Contents/Resources"
installer="$temporary/install-fixture.sh"
sed "s|/Applications/ChatGPT.app/Contents/Resources|$resources|g" \
  "$root/install.sh" >"$installer"
mkdir -p "$resources/codex-cli/bin"

assert_bundled_install() {
  output=$(env -u CXS_CODEX_PATH PATH="$fake_bin:$PATH" \
    CXS_INSTALL_DIR="$install_dir" sh "$installer")
  grep -Fq 'Detected bundled ChatGPT Desktop Codex 1.2.3.' <<<"$output"
  grep -Fq 'fake cxs binary' "$install_dir/cxs"
}

# The new layout must work without any legacy binary.
cp "$fake_bin/codex" "$resources/codex-cli/bin/codex"
assert_bundled_install

# Prefer the new layout even if an older bundled binary still exists.
printf '#!/bin/sh\nprintf "codex-cli 4.5.6\\n"\n' >"$resources/codex"
chmod +x "$resources/codex"
assert_bundled_install

# Continue to support the legacy layout when the new one is absent or invalid.
rm "$resources/codex-cli/bin/codex"
cp "$fake_bin/codex" "$resources/codex"
assert_bundled_install
cp "$fake_bin/codex" "$resources/codex-cli/bin/codex"
chmod -x "$resources/codex-cli/bin/codex"
assert_bundled_install
rm "$resources/codex-cli/bin/codex"
mkdir "$resources/codex-cli/bin/codex"
assert_bundled_install

# An invalid explicit override must fail, not silently choose a bundled binary.
if output=$(PATH="$fake_bin:$PATH" CXS_CODEX_PATH="$temporary/missing-codex" \
  CXS_INSTALL_DIR="$install_dir" sh "$installer" 2>&1); then
  echo 'installer accepted a missing explicit override' >&2
  exit 1
fi
grep -Fq "was not found at $temporary/missing-codex" <<<"$output"

# Never substitute the unrelated codex on PATH when no bundled binary exists.
rm "$resources/codex"
rmdir "$resources/codex-cli/bin/codex"
if output=$(env -u CXS_CODEX_PATH PATH="$fake_bin:$PATH" \
  CXS_INSTALL_DIR="$install_dir" sh "$installer" 2>&1); then
  echo 'installer selected an unrelated PATH binary' >&2
  exit 1
fi
grep -Fq "was not found at $resources/codex-cli/bin/codex" <<<"$output"
printf '%s\n' 'Installer override and macOS bundled-layout regressions passed.'
