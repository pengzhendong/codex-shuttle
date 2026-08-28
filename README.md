<div align="center">

# 🚀 Codex Shuttle

**Keep the Codex desktop experience on macOS or Windows. Run the work on Linux or Apple Silicon macOS over SSH.**

[简体中文](README.zh-CN.md) · [Architecture](docs/architecture.md) · [Troubleshooting](docs/troubleshooting.md) · [Releases](https://github.com/pengzhendong/codex-shuttle/releases)

[![CI](https://github.com/pengzhendong/codex-shuttle/actions/workflows/ci.yml/badge.svg)](https://github.com/pengzhendong/codex-shuttle/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/pengzhendong/codex-shuttle?display_name=tag)](https://github.com/pengzhendong/codex-shuttle/releases)
[![License](https://img.shields.io/github/license/pengzhendong/codex-shuttle)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.85%2B-dea584?logo=rust)](rust-toolchain.toml)
[![macOS](https://img.shields.io/badge/macOS-arm64%20%7C%20x86__64-000000?logo=apple)](#requirements)
[![Windows](https://img.shields.io/badge/Windows-x86__64-0078d4?logo=windows)](#requirements)
[![Linux](https://img.shields.io/badge/remote%20Linux-arm64%20%7C%20x86__64-fcc624?logo=linux&logoColor=black)](#requirements)
[![macOS server](https://img.shields.io/badge/remote%20macOS-Apple%20Silicon-000000?logo=apple)](#requirements)

> [!WARNING]
> Unofficial and under active development. Codex Shuttle depends on version-sensitive Codex App Server and experimental Exec Server interfaces.

</div>

Codex Shuttle (`cxs`) connects the Codex desktop app to an existing Linux or Apple Silicon macOS SSH host. Threads, account state, and the UI stay on your desktop; shells, files, search, Git discovery, PTYs, and tests run on the remote host.

It uses your existing OpenSSH configuration and one ordinary SSH stdio connection—no SSH daemon replacement, TCP forwarding, or remote Codex login required.

## Why Shuttle?

- **Native desktop workflow** — keep the Codex app, local account, and local thread database.
- **Real remote paths** — browse and open remote folders instead of mirrored local paths.
- **Remote execution** — commands, terminals, file operations, search, and Git metadata run on the remote host; Linux also uses the packaged sandbox.
- **Official remote Codex** — use OpenAI's version-matched package instead of a Shuttle-maintained fork.
- **One SSH connection** — Yamux carries App, Exec, and Host channels over a single SSH stdin/stdout stream.
- **Session migration** — pull server-created sessions to the desktop and repair Provider visibility metadata.
- **Version-bound releases** — new stable OpenAI Codex source tags are checked daily; old Shuttle releases remain available.

## Requirements

| Local | Remote |
| --- | --- |
| macOS on Apple Silicon/Intel, or Windows on x86_64 | Linux on arm64/x86_64, or Apple Silicon macOS |
| Codex desktop app installed | OpenSSH access with non-interactive key authentication |
| OpenSSH client | `sh`, `curl`, `tar`, and `sha256sum` (Linux) or `shasum` (macOS) |

Intel macOS is supported as a desktop client, but not as a remote server.

Shuttle uses the Codex binary bundled with the desktop app; it never resolves `codex` from `PATH`. On macOS this is `/Applications/ChatGPT.app/Contents/Resources/codex`. On Windows, Shuttle selects the newest `%LOCALAPPDATA%\OpenAI\Codex\bin\**\codex.exe`. Set `CXS_CODEX_PATH` to override discovery. The selected Shuttle release must match that binary's public source baseline. For example, a desktop build reporting `0.147.0-alpha.6.5` uses the Shuttle release for Codex `0.147.0`.

## Quick start

### 1. Install `cxs`

On macOS, the installer detects the architecture and bundled Codex version, verifies the release checksum, and installs `cxs` to `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/pengzhendong/codex-shuttle/master/install.sh | sh
```

Or with `wget`:

```bash
wget -qO- https://raw.githubusercontent.com/pengzhendong/codex-shuttle/master/install.sh | sh
```

On Windows, run in PowerShell:

```powershell
irm https://raw.githubusercontent.com/pengzhendong/codex-shuttle/master/install.ps1 | iex
```

The Windows installer places `cxs.exe` under `%LOCALAPPDATA%\Programs\codex-shuttle\bin` and prints the PATH command when needed.

You can also download and verify a binary manually from [Releases](https://github.com/pengzhendong/codex-shuttle/releases).

### 2. Prepare SSH

Create or reuse a normal host alias in `~/.ssh/config`, then confirm key-based login works without a prompt:

```bash
ssh -o BatchMode=yes my-remote-host true
```

### 3. Add and install the host

```bash
cxs add my-remote-host --name devbox
cxs install devbox
cxs doctor devbox
```

Shuttle creates the app-facing SSH alias `cxs-devbox`. In the Codex desktop app, choose that host and open a remote path such as `/home/me/project` or `/Users/me/project`.

The server downloads the matching official Codex package by default. To download it on the desktop and upload it over SSH instead:

```bash
cxs install devbox --local-download
```

## Common commands

| Command | Purpose |
| --- | --- |
| `cxs add <ssh-host> [--name <profile>]` | Create or refresh a profile from `ssh -G` |
| `cxs install <profile>` | Install matching official Codex and the Shuttle shim |
| `cxs update <profile>` | Update artifacts for the current local Codex |
| `cxs up <profile>` / `cxs down <profile>` | Start or stop the local bridge |
| `cxs doctor <profile> [--json]` | Verify Codex, SSH, remote filesystem, and remote command execution |
| `cxs list` / `cxs status <profile>` | Inspect configured profiles |
| `cxs config <profile>` | Print the generated SSH host block |
| `cxs rollback <profile>` | Switch back to the previous remote release |
| `cxs sync <profile>` | Import server-created sessions without overwriting local threads |
| `cxs repair` | Back up and repair local Provider/session metadata |
| `cxs remove <profile> [--remote]` | Remove local state and optionally remote Shuttle state |

Run `cxs <command> --help` for all user-facing options.

## Session sync

Sessions normally remain desktop-owned. If you previously ran Codex directly on the Linux host, import its rollout files with:

```bash
cxs sync devbox
# Custom remote CODEX_HOME:
cxs sync devbox --remote-home /srv/codex-home
```

`sync` deduplicates by thread ID, never overwrites an existing local session, and never replaces the desktop's SQLite database with a remote copy.

If local sessions disappear after switching `model_provider`, close Codex and run:

```bash
cxs repair
```

`repair` backs up affected rollout files and SQLite state before reconciling Provider and working-directory metadata. The Rust implementation adapts the core repair approach from the MIT-licensed [`codex-provider-sync`](https://github.com/Dailin521/codex-provider-sync); attribution is retained in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## How it works

```text
Codex desktop app
        │  app-server proxy through generated SSH alias
        ▼
 remote cxs-shim ═══ one SSH stdio connection / Yamux ═══ local cxs-bridge
        │                                                    │
        ├── version-matched Codex Exec Server ◄── exec ─────┤
        └── restricted remote App Server      ◄── host ─────┤
                                                             └── local App Server
                                                                  owns sessions
```

The local App Server remains authoritative for threads and account state. Shuttle registers a remote execution environment and routes host-facing filesystem/process RPCs to a restricted remote App Server. The remote executor is OpenAI's official [`codex-package-<target>.tar.gz`](https://github.com/openai/codex/releases), including its maintained App Server, Exec Server, platform components, code-mode host, and ripgrep. Shuttle adds only the SSH/Yamux shim.

The shim intercepts only the desktop App Server bootstrap and Shuttle's hidden agent command. Other `codex` CLI commands are delegated to the managed official binary.

Read [Architecture](docs/architecture.md) for protocol details and [Dependency boundaries](docs/dependency-boundaries.md) for what Shuttle reuses instead of reimplementing.

## Releases and compatibility

GitHub Actions checks for stable OpenAI `rust-vX.Y.Z` tags daily at 00:17 UTC and
processes every unpublished version in order. A version-bound Shuttle release
is published only after formatting, Clippy, and all workspace tests pass on macOS ARM64,
Windows x64, Linux x64, and Linux ARM64, and real-package SSH end-to-end tests pass on
every supported server platform. The matching official Codex packages and Shuttle shims
must also build successfully.
These gates prove build compatibility;
`cxs doctor` is still the live end-to-end check for a specific desktop and server.
A failed build creates no partial release and is retried by the next scheduled
run.

Each released desktop CLI embeds its complete Shuttle release tag. It downloads the shim from that immutable release and Codex from the matching immutable OpenAI release. See [Release and install flow](docs/runtime-release.md) and [Compatibility](docs/compatibility.md).

## Security model

- OpenSSH remains responsible for encryption, host keys, identities, agents, and jump hosts.
- No Codex login credential is copied to the remote host.
- Remote Exec and Host App Servers use separate private `CODEX_HOME` directories.
- Official Codex and Shuttle shim artifacts are verified with SHA-256 before activation; `doctor` also checks the installed executor digest.
- The local bridge uses a private local endpoint (Unix socket on macOS, loopback on Windows) and a per-profile random token.
- Remote releases are immutable and rollback keeps the previous verified release.
- Session import rejects unsafe archive paths and never overwrites an existing thread ID.

See [SECURITY.md](SECURITY.md) for reporting vulnerabilities and
[Troubleshooting](docs/troubleshooting.md) for common setup failures.

## Development

```bash
cargo build --workspace
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [Release and install flow](docs/runtime-release.md) before changing protocol or release code.

## Status

Codex Shuttle is an independent community project and is not affiliated with or endorsed by OpenAI. Compatibility can change when version-sensitive upstream interfaces change.

## License

[Apache License 2.0](LICENSE). Third-party notices are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
