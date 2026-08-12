# Runtime release flow

`cxs-runtime` is a version-pinned Linux executor for Shuttle. It is not part of the normal Shuttle Cargo workspace and is never compiled on the user's Mac or SSH host.

## Automatic Codex releases

1. Every 15 minutes, GitHub Actions discovers stable OpenAI `rust-vX.Y.Z`
   tags at or above the baseline in `runtime/codex-versions.txt`.
2. If `v<shuttle-version>-codex.<codex-version>` does not exist, the workflow
   selects the oldest unpublished version, runs the workspace tests, and starts
   all platform builds. This preserves every stable version even when upstream
   publishes multiple tags while a build is running.
3. `scripts/build-cxs-runtime.sh` downloads OpenAI's exact `rust-v<version>`
   source and injects `runtime/cxs-runtime` as an additional workspace member.
4. GitHub Actions cross-compiles static `x86_64-unknown-linux-musl` and
   `aarch64-unknown-linux-musl` binaries and smoke-tests both packages.
5. Only after both Mac CLIs, Linux shims, runtimes, and all tests pass does it
   publish an immutable Release with `SHA256SUMS`.

A failed build creates no Release and is retried by the next scheduled run.
`workflow_dispatch` can publish a specific stable Codex version. Older Releases
are retained so users can install an older version-bound set.

These automated gates establish build compatibility for a new source tag. They
do not have access to every user's SSH host or desktop build, so the installed
profile becomes `ready` only after `cxs doctor` completes its live App Server,
Exec Server, and remote-filesystem checks.

The runtime entry point exposes only `--version`, `app-server`, and `exec-server`. Both services call OpenAI's own Rust libraries. OpenAI's arg0 dispatcher is retained so Exec Server filesystem helpers, process helpers, and Linux sandbox self-reexec continue to work. The package also includes OpenAI's `codex-bwrap` build and the exact `ripgrep` artifact pinned by that source tag, so sandboxing and searches work on minimal Linux hosts.

## Install and activation

1. `cxs install` reads the exact local Codex version, derives its public source baseline, and detects the remote architecture.
2. It constructs the matching runtime artifact name in the same Shuttle release as the CLI.
3. By default the SSH host downloads the release checksum manifest and runtime in parallel with the Mac uploading `cxs-shim`. With `--local-download`, the Mac downloads and verifies the same artifact before uploading it.
4. The installer verifies SHA-256, extracts into a private immutable staging directory, checks the exact version and `exec-server --help`, writes `shim.json` and `install.json`, then atomically switches `current`.
5. `cxs up` starts one SSH stdio session. The remote shim starts the runtime Exec Server; Host channels start restricted runtime App Servers. The Mac bridge keeps the real thread/session App Server local.
6. Readiness is granted only after environment registration and a real remote directory read succeed.

If the runtime artifact is missing or cannot be verified, installation stops.
Each released Mac CLI embeds its full GitHub Release tag and downloads the shim
and runtime from that same Release, preventing cross-version asset mixing.

For pre-release validation, build a runtime locally and install it with `cxs install <profile> --runtime-package <archive> --shim <linux-shim>`.
