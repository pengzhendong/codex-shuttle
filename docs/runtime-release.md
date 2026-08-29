# Release and install flow

Codex Shuttle does not compile or redistribute Codex. It uses the official,
versioned packages published by OpenAI and releases only Shuttle's macOS and
Windows CLIs plus the Linux and macOS shims.

## Automatic Shuttle releases

1. Every day, GitHub Actions discovers stable OpenAI `rust-vX.Y.Z` tags at or
   above the baseline in `runtime/codex-versions.txt`.
2. It selects the oldest version without a matching
   `v<shuttle-version>-codex.<codex-version>` Shuttle release.
3. The workflow verifies that OpenAI published official packages for Linux
   x86_64, Linux arm64, Apple Silicon macOS, and Intel macOS. Formatting, Clippy,
   and all workspace tests run natively on macOS ARM64, macOS x64, Windows x64,
   Linux x64, and Linux ARM64. Real-package SSH end-to-end tests run on all four server targets.
4. It publishes two macOS CLIs, one Windows CLI, four remote shims, and
   `SHA256SUMS`. No Codex source is
   checked out and no Codex runtime is compiled or stored by Shuttle.

A failed build creates no partial release and is retried by the next scheduled
run. `workflow_dispatch` can publish a specific stable version. Older releases
remain available for older desktop-bundled Codex versions.

The supported runner, Rust target, architecture alias, CLI asset, and shim
asset mapping lives in `runtime/platforms.json`. The installer and both CI
workflows consume that manifest so a platform cannot be added to only one
stage of the release process.

## Install and activation

1. `cxs install` reads the Codex version bundled with ChatGPT Desktop, derives
   its public source baseline, and detects the remote operating system and architecture.
2. It selects OpenAI's immutable
   `rust-v<version>/codex-package-<target>.tar.gz` and
   `codex-package_SHA256SUMS` assets.
3. By default the SSH host downloads and verifies the official package while
   the desktop uploads the small Shuttle shim. With `--local-download`, the
   desktop downloads and verifies Codex first, then uploads it over SSH.
4. The installer extracts to a private staging directory and checks the
   official Codex binary, code-mode host, ripgrep, Linux bubblewrap when applicable, exact version,
   and `exec-server` entry point before atomically switching `current`. It also
   records the extracted Codex digest so later `doctor` runs detect changes.
5. `cxs up` starts one SSH stdio session. The shim starts official Codex's Exec
   Server and restricted Host App Servers; the local App Server remains the
   authority for the real thread and account state.
6. A profile becomes ready only after live environment registration, remote
   command execution, and a remote directory read succeed.

The official package is not installed system-wide. It lives under
`~/.local/lib/codex-shuttle/releases/`, while `~/.local/bin/codex` remains the
Shuttle shim expected by the desktop SSH bootstrap.
