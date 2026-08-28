# Release and install flow

Codex Shuttle does not compile or redistribute Codex. It uses the official,
versioned Linux packages published by OpenAI and releases only Shuttle's macOS
CLI and Linux shim.

## Automatic Shuttle releases

1. Every day, GitHub Actions discovers stable OpenAI `rust-vX.Y.Z` tags at or
   above the baseline in `runtime/codex-versions.txt`.
2. It selects the oldest version without a matching
   `v<shuttle-version>-codex.<codex-version>` Shuttle release.
3. The workflow verifies that OpenAI published official Linux packages for
   x86_64 and arm64, runs all workspace tests, and builds two macOS CLIs plus
   two static Linux shims.
4. It publishes those four small binaries and `SHA256SUMS`. No Codex source is
   checked out and no Codex runtime is compiled or stored by Shuttle.

A failed build creates no partial release and is retried by the next scheduled
run. `workflow_dispatch` can publish a specific stable version. Older releases
remain available for older desktop-bundled Codex versions.

## Install and activation

1. `cxs install` reads the Codex version bundled with ChatGPT Desktop, derives
   its public source baseline, and detects the remote Linux architecture.
2. It selects OpenAI's immutable
   `rust-v<version>/codex-package-<target>.tar.gz` and
   `codex-package_SHA256SUMS` assets.
3. By default the SSH host downloads and verifies the official package while
   the Mac uploads the small Shuttle shim. With `--local-download`, the Mac
   downloads and verifies Codex first, then uploads it over SSH.
4. The installer extracts to a private staging directory and checks the
   official Codex binary, code-mode host, ripgrep, bubblewrap, exact version,
   and `exec-server` entry point before atomically switching `current`.
5. `cxs up` starts one SSH stdio session. The shim starts official Codex's Exec
   Server and restricted Host App Servers; the Mac App Server remains the
   authority for the real thread and account state.
6. A profile becomes ready only after live environment registration, Linux
   command execution, and a remote directory read succeed.

The official package is not installed system-wide. It lives under
`~/.local/lib/codex-shuttle/releases/`, while `~/.local/bin/codex` remains the
Shuttle shim expected by the desktop SSH bootstrap.
