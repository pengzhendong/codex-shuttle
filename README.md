<div align="center">

# 🚀 Codex Shuttle

**Keep the Codex desktop experience on your Mac. Run the work on Linux over SSH.**

[简体中文](README.zh-CN.md) · [Architecture](docs/architecture.md) · [Troubleshooting](docs/troubleshooting.md) · [Releases](https://github.com/pengzhendong/codex-shuttle/releases)

[![CI](https://github.com/pengzhendong/codex-shuttle/actions/workflows/ci.yml/badge.svg)](https://github.com/pengzhendong/codex-shuttle/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/pengzhendong/codex-shuttle?display_name=tag)](https://github.com/pengzhendong/codex-shuttle/releases)
[![License](https://img.shields.io/github/license/pengzhendong/codex-shuttle)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.85%2B-dea584?logo=rust)](rust-toolchain.toml)
[![macOS](https://img.shields.io/badge/macOS-arm64%20%7C%20x86__64-000000?logo=apple)](#requirements)
[![Linux](https://img.shields.io/badge/remote%20Linux-arm64%20%7C%20x86__64-fcc624?logo=linux&logoColor=black)](#requirements)

> [!WARNING]
> Unofficial and under active development. Codex Shuttle depends on version-sensitive Codex App Server and experimental Exec Server interfaces.

</div>

Codex Shuttle (`cxs`) connects the Codex desktop app to an existing Linux SSH host. Threads, account state, and the UI stay on your Mac; shells, files, search, Git discovery, PTYs, tests, and sandboxed processes run on Linux.

It uses your existing OpenSSH configuration and one ordinary SSH stdio connection—no SSH daemon replacement, TCP forwarding, or remote Codex login required.

## Why Shuttle?

- **Native desktop workflow** — keep the Codex app, local account, and local thread database.
- **Real remote paths** — browse and open Linux folders instead of mirrored Mac paths.
- **Remote execution** — commands, terminals, file operations, search, Git metadata, and sandboxing run on Linux.
- **Small remote runtime** — install only the version-matched App Server/Exec Server runtime, not the full Codex CLI/TUI.
- **One SSH connection** — Yamux carries App, Exec, and Host channels over a single SSH stdin/stdout stream.
- **Session migration** — pull server-created sessions to the Mac and repair Provider visibility metadata.
- **Version-bound releases** — new stable OpenAI Codex source tags are checked hourly; old Shuttle releases remain available.

## Requirements

| Local | Remote |
| --- | --- |
| macOS on Apple Silicon or Intel | Linux on arm64 or x86_64 |
| ChatGPT desktop app installed in `/Applications` | OpenSSH access with non-interactive key authentication |
| OpenSSH client | `sh`, `curl`, `tar`, and `sha256sum` |

Shuttle always uses the Codex binary bundled in `/Applications/ChatGPT.app/Contents/Resources/codex`; it never resolves `codex` from `PATH`. The selected Shuttle release must match that binary's public source baseline. For example, a desktop build reporting `0.147.0-alpha.6.5` uses the Shuttle release for Codex `0.147.0`.

## Quick start

### 1. Install `cxs`

The installer detects your Mac architecture and the bundled ChatGPT Desktop Codex version, verifies the release checksum, and installs `cxs` to `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/pengzhendong/codex-shuttle/master/install.sh | sh
```

Or with `wget`:

```bash
wget -qO- https://raw.githubusercontent.com/pengzhendong/codex-shuttle/master/install.sh | sh
```

You can also download and verify a binary manually from [Releases](https://github.com/pengzhendong/codex-shuttle/releases).

### 2. Prepare SSH

Create or reuse a normal host alias in `~/.ssh/config`, then confirm key-based login works without a prompt:

```bash
ssh -o BatchMode=yes my-linux-host true
```

### 3. Add and install the host

```bash
cxs add my-linux-host --name devbox
cxs install devbox
cxs doctor devbox
```

Shuttle creates the app-facing SSH alias `cxs-devbox`. In the Codex desktop app, choose that host and open a Linux path such as `/home/me/project`.

The server downloads its matching runtime by default. To download it on the Mac and upload it over SSH instead:

```bash
cxs install devbox --local-download
```

## Common commands

| Command | Purpose |
| --- | --- |
| `cxs add <ssh-host> [--name <profile>]` | Create or refresh a profile from `ssh -G` |
| `cxs install <profile>` | Install the matching remote runtime and shim |
| `cxs update <profile>` | Update artifacts for the current local Codex |
| `cxs up <profile>` / `cxs down <profile>` | Start or stop the local bridge |
| `cxs doctor <profile>` | Verify Codex, SSH, runtime, remote filesystem, and Linux command execution |
| `cxs list` / `cxs status <profile>` | Inspect configured profiles |
| `cxs config <profile>` | Print the generated SSH host block |
| `cxs rollback <profile>` | Switch back to the previous remote release |
| `cxs sync <profile>` | Import server-created sessions without overwriting local threads |
| `cxs repair` | Back up and repair local Provider/session metadata |
| `cxs remove <profile> [--remote]` | Remove local state and optionally remote Shuttle state |

Run `cxs <command> --help` for all options, including local runtime packages and explicit remote-executor overrides.

## Session sync

Sessions normally remain Mac-owned. If you previously ran Codex directly on the Linux host, import its rollout files with:

```bash
cxs sync devbox
# Custom remote CODEX_HOME:
cxs sync devbox --remote-home /srv/codex-home
```

`sync` deduplicates by thread ID, never overwrites an existing local session, and never replaces the Mac's SQLite database with a remote copy.

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
        └── restricted Linux App Server       ◄── host ─────┤
                                                             └── Mac App Server
                                                                  owns sessions
```

The Mac App Server remains authoritative for threads and account state. Shuttle registers a Linux execution environment and routes host-facing filesystem/process RPCs to a restricted Linux App Server. The remote runtime is built from [OpenAI Codex](https://github.com/openai/codex)'s matching public `rust-vX.Y.Z` source and contains only the App Server and Exec Server entry points Shuttle needs.

Read [Architecture](docs/architecture.md) for protocol details and [Dependency boundaries](docs/dependency-boundaries.md) for what Shuttle reuses instead of reimplementing.

## Releases and compatibility

GitHub Actions checks for stable OpenAI `rust-vX.Y.Z` tags every hour and
processes every unpublished version in order. A version-bound Shuttle release
is published only after workspace tests, both Mac builds, both Linux shims, and
both Linux runtime smoke tests succeed. These gates prove build compatibility;
`cxs doctor` is still the live end-to-end check for a specific Mac and server.
A failed build creates no partial release and is retried by the next scheduled
run.

Each released Mac CLI embeds its complete release tag and downloads the shim/runtime from that same immutable release. See [Runtime release flow](docs/runtime-release.md) and [Compatibility](docs/compatibility.md).

## Security model

- OpenSSH remains responsible for encryption, host keys, identities, agents, and jump hosts.
- No Codex login credential is copied to Linux.
- Remote Exec and Host App Servers use separate private `CODEX_HOME` directories.
- Runtime and shim artifacts are verified with SHA-256 before activation.
- The local bridge uses a private Unix socket and a per-profile random token.
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

The version-pinned remote runtime is intentionally outside the normal Cargo workspace. See [CONTRIBUTING.md](CONTRIBUTING.md) and [Runtime release flow](docs/runtime-release.md) before changing protocol or release code.

## Status

Codex Shuttle is an independent community project and is not affiliated with or endorsed by OpenAI. Compatibility can change when version-sensitive upstream interfaces change.

## License

[Apache License 2.0](LICENSE). Third-party notices are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
