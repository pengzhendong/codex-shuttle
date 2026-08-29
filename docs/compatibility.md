# Compatibility contract

Shuttle verifies each Codex source version instead of assuming internal protocol compatibility. A released artifact set is always bound to one source tag.

## Compatibility levels

- **Build-compatible**: official Codex packages exist and formatting, Clippy, all workspace tests, all CLI/shim builds, and real-package SSH lifecycle tests passed on every supported native target. Automated Releases provide this level.
- **Ready on a host**: `cxs doctor` completed the live SSH handshake, App Server initialization, external Exec Server readiness check, remote directory read, and a remote `command/exec` probe for one profile.
- **Matrix-verified**: the extended filesystem, process, PTY, diff, platform sandbox behavior, resume/fork, and disconnect-cleanup matrix passed on Linux x64, Linux ARM64, Apple Silicon macOS, and Intel macOS.

Build compatibility is necessary but does not replace the per-host readiness check. A version must not be described as matrix-verified without recorded live results.

## Version-qualified contract

This contract was first established with `codex-cli 0.147.0`. The automatic release workflow now rechecks it for every source version admitted by `runtime/codex-versions.txt` on every supported native target:

- `codex app-server --stdio` accepts an initialize request with the `experimentalApi` capability.
- The generated experimental schema contains `environment/add`, `environment/status`, `environment/info`, `TurnEnvironmentParams`, and sticky environments on `ThreadStartParams`.
- `environment/add` accepts a loopback `ws://` Exec Server URL.
- `codex exec-server --listen ws://127.0.0.1:<port>` reaches `ready` through `environment/status`.
- `environment/info` returns the Exec Server's shell and cwd, proving that App Server is querying the external execution environment rather than its own host.
- The generated client schema contains the Host RPCs Shuttle routes remotely: `fs/readDirectory`, `fs/readFile`, `fs/watch`, `fuzzyFileSearch/sessionStart`, `process/spawn`, and `command/exec`.
- The macOS desktop SSH bootstrap currently probes `codex` through an interactive login shell, starts `codex -c features.code_mode_host=true app-server --listen unix://`, and then runs `codex app-server proxy`. Shuttle's shim emulates this control-socket lifecycle while exiting after the proxy disconnects.
- On Windows, Shuttle discovers the desktop-managed `codex.exe`, launches its local App Server on a private loopback WebSocket, and uses a per-profile readiness file instead of a Unix socket. The remote Unix shim and Yamux protocol are unchanged.
- Shuttle carries App, Exec Server, and Host App Server traffic as independent Yamux streams over one ordinary SSH command's stdin/stdout. Each stream begins with Shuttle's versioned type header. Compatibility does not depend on `AllowTcpForwarding` or `AllowStreamLocalForwarding`.
- The installer verifies OpenAI's official package checksum, requires the pinned source version, and checks `codex exec-server --help`. Readiness still requires live execution-environment and remote filesystem probes; version matching alone is never sufficient.

The automated `cxs-probe` regenerates the experimental schema and checks the required fields. The extended matrix remains a separate verification level beyond the automatic build gates.

## Failure policy

If any required method or field is absent, `doctor` fails. Shuttle must not silently fall back to local execution because doing so would violate its data-location and security guarantees.
