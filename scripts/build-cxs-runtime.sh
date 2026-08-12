#!/bin/sh
set -eu

if test "$#" -ne 3; then
  echo "usage: $0 CODEX_VERSION TARGET OUTPUT_DIR" >&2
  exit 2
fi

version=$1
target=$2
output_dir=$3
repo_root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
work_root=${RUNNER_TEMP:-"$repo_root/target/runtime-build"}
source_dir="$work_root/codex-$version"
source_archive="$work_root/codex-$version.tar.gz"
target_dir=${CXS_RUNTIME_TARGET_DIR:-"$work_root/cxs-runtime-target-$target"}
tag="rust-v$version"

mkdir -p "$work_root" "$output_dir"
if test ! -f "$source_dir/codex-rs/Cargo.toml"; then
  rm -rf "$source_dir"
  curl --http1.1 --fail --location --retry 5 --retry-all-errors \
    --output "$source_archive.partial" \
    "https://codeload.github.com/openai/codex/tar.gz/refs/tags/$tag"
  mv "$source_archive.partial" "$source_archive"
  mkdir "$source_dir"
  tar -xzf "$source_archive" -C "$source_dir" --strip-components=1
fi

rm -rf "$source_dir/codex-rs/cxs-runtime"
cp -R "$repo_root/runtime/cxs-runtime" "$source_dir/codex-rs/cxs-runtime"
python_bin=${PYTHON_BIN:-python3}
"$python_bin" - "$source_dir/codex-rs/Cargo.toml" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text()
needle = 'members = [\n'
if '    "cxs-runtime",\n' not in text:
    text = text.replace(needle, needle + '    "cxs-runtime",\n', 1)
path.write_text(text)
PY

export CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export CARGO_NET_RETRY=5
export CARGO_PROFILE_RELEASE_DEBUG=false
# The runtime is an RPC host rather than a compute-heavy CLI. Avoid upstream's
# ThinLTO release setting and restore Cargo's normal parallel code generation;
# this materially reduces first-build time without changing protocol behavior.
export CARGO_PROFILE_RELEASE_LTO=${CARGO_PROFILE_RELEASE_LTO:-off}
export CARGO_PROFILE_RELEASE_OPT_LEVEL=${CARGO_PROFILE_RELEASE_OPT_LEVEL:-2}
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${CARGO_PROFILE_RELEASE_CODEGEN_UNITS:-16}
export CARGO_TARGET_DIR="$target_dir"
if test "${CXS_RUNTIME_CARGO:-cargo}" = cargo-zigbuild; then
  cargo zigbuild --manifest-path "$source_dir/codex-rs/Cargo.toml" --release --target "$target" -p cxs-runtime
else
  "${CXS_RUNTIME_CARGO:-cargo}" build --manifest-path "$source_dir/codex-rs/Cargo.toml" --release --target "$target" -p cxs-runtime
fi

package="cxs-runtime-codex-$version-$target"
stage="$work_root/$package"
archive="$output_dir/$package.tar.gz"
rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/codex-path" "$stage/codex-resources"
install -m 0755 "$target_dir/$target/release/cxs-runtime" "$stage/bin/cxs-runtime"
bwrap_name="bwrap-$target.tar.gz"
bwrap_archive="$work_root/bwrap-$version-$target.tar.gz"
if test ! -f "$bwrap_archive"; then
  curl --http1.1 --fail --location --retry 5 --retry-all-errors \
    --output "$bwrap_archive.partial" \
    "https://github.com/openai/codex/releases/download/$tag/bwrap-$target.tar.gz"
  mv "$bwrap_archive.partial" "$bwrap_archive"
fi
bwrap_dir="$work_root/bwrap-$version-$target"
find "$bwrap_dir" -depth -delete 2>/dev/null || true
mkdir -p "$bwrap_dir"
tar -xzf "$bwrap_archive" -C "$bwrap_dir"
bwrap_bin=$(find "$bwrap_dir" -type f -name bwrap -print -quit)
test -n "$bwrap_bin"
expected_bwrap_sha=$(curl --http1.1 --fail --location --retry 5 --retry-all-errors --silent --show-error \
  "https://api.github.com/repos/openai/codex/releases/tags/$tag" | \
  "$python_bin" -c 'import json,sys; wanted=sys.argv[1]; assets=json.load(sys.stdin)["assets"]; digest=next(a["digest"] for a in assets if a["name"] == wanted); print(digest.removeprefix("sha256:"))' "$bwrap_name")
actual_bwrap_sha=$(sha256sum "$bwrap_archive" | awk '{print $1}')
test "$actual_bwrap_sha" = "$expected_bwrap_sha"
install -m 0755 "$bwrap_bin" "$stage/codex-resources/bwrap"
rg_bin=$(
PYTHONPATH="$source_dir" "$python_bin" - "$target" <<'PY'
import sys
from scripts.codex_package.ripgrep import fetch_rg
from scripts.codex_package.targets import TARGET_SPECS

print(fetch_rg(TARGET_SPECS[sys.argv[1]]))
PY
)
install -m 0755 "$rg_bin" "$stage/codex-path/rg"
cat > "$stage/codex-package.json" <<EOF
{"layoutVersion":1,"version":"$version","target":"$target","variant":"cxs-runtime","entrypoint":"bin/cxs-runtime","resourcesDir":"codex-resources","pathDir":"codex-path"}
EOF
printf '%s\n' "$version" > "$stage/CODEX_VERSION"
printf '%s\n' "$tag" > "$stage/OPENAI_SOURCE_TAG"
tar -C "$stage" -czf "$archive" .
printf '%s\n' "$archive"
