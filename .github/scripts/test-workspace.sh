#!/usr/bin/env bash
set -uo pipefail

mkdir -p target
log=target/cxs-ci-test.log
set +e
cargo test --locked --workspace 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
set -e

if test "$status" -ne 0; then
  detail=$(tail -n 48 "$log")
  detail=${detail//'%'/'%25'}
  detail=${detail//$'\r'/'%0D'}
  detail=${detail//$'\n'/'%0A'}
  echo "::error title=Workspace test failure::$detail"
  exit "$status"
fi
