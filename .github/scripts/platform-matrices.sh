#!/usr/bin/env bash
set -euo pipefail

manifest=${1:-runtime/platforms.json}
output=${2:-${GITHUB_OUTPUT:-}}

test -f "$manifest"
jq -e '
  .platforms | length > 0 and
  (all(.[];
    (.id | type == "string" and length > 0) and
    (.runner | type == "string" and length > 0) and
    (.runner_os | type == "string" and length > 0) and
    (.runner_arch | type == "string" and length > 0) and
    (.rust_target | type == "string" and length > 0) and
    (.remote | type == "boolean") and
    (.requires_bwrap | type == "boolean") and
    (if .remote then
      (.kernel | type == "string" and length > 0) and
      (.uname_arches | type == "array" and length > 0) and
      (.shim_asset | type == "string" and length > 0) and
      (.shim_builder == "cargo" or .shim_builder == "zig")
    else
      .shim_asset == null and .shim_builder == null
    end) and
    (if .cli_asset != null then .cli_source != null else .cli_source == null end)
  ))
' "$manifest" >/dev/null

quality=$(jq -c '{include: [.platforms[] | {
  runner,
  expected_os: .runner_os,
  expected_arch: .runner_arch
}]}' "$manifest")
remote=$(jq -c '{include: [.platforms[] | select(.remote) | {
  id,
  runner,
  target: .rust_target,
  kernel,
  uname_arches: (.uname_arches | join(","))
}]}' "$manifest")
cli=$(jq -c '{include: [.platforms[] | select(.cli_asset != null) | {
  id,
  runner,
  target: .rust_target,
  asset: .cli_asset,
  source: .cli_source
}]}' "$manifest")
shim=$(jq -c '{include: [.platforms[] | select(.remote) | {
  id,
  runner,
  target: .rust_target,
  asset: .shim_asset,
  builder: .shim_builder
}]}' "$manifest")
remote_targets=$(jq -c '[.platforms[] | select(.remote) | .rust_target]' "$manifest")

if test -n "$output"; then
  {
    printf 'quality=%s\n' "$quality"
    printf 'remote=%s\n' "$remote"
    printf 'cli=%s\n' "$cli"
    printf 'shim=%s\n' "$shim"
    printf 'remote-targets=%s\n' "$remote_targets"
  } >> "$output"
else
  jq -n \
    --argjson quality "$quality" \
    --argjson remote "$remote" \
    --argjson cli "$cli" \
    --argjson shim "$shim" \
    --argjson remote_targets "$remote_targets" \
    '{quality: $quality, remote: $remote, cli: $cli, shim: $shim, remote_targets: $remote_targets}'
fi
