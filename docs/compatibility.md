# Compatibility contract

Shuttle verifies each Codex source version instead of assuming internal protocol compatibility. A released artifact set is always bound to one source tag.

## Compatibility levels

- **Build-compatible**: the workspace tests, both Mac builds, both Linux shim builds, and both packaged-runtime smoke tests passed. Automated Releases provide this level.
- **Ready on a host**: `cxs doctor` completed the live SSH handshake, App Server initialization, external Exec Server readiness check, remote directory read, and a Linux `command/exec` probe for one profile.
- **Matrix-verified**: the extended filesystem, process, PTY, diff, sandbox, resume/fork, and disconnect-cleanup matrix passed on both supported Linux architectures.

Build compatibility is necessary but does not replace the per-host readiness check. A version must not be described as matrix-verified without recorded live results.

## Baseline validated contract: `codex-cli 0.147.0`

Verified locally on macOS arm64:

- `codex app-server --stdio` accepts an initialize request with the `experimentalApi` capability.
- The generated experimental schema contains `environment/add`, `environment/status`, `environment/info`, `TurnEnvironmentParams`, and sticky environments on `ThreadStartParams`.
- `environment/add` accepts a loopback `ws://` Exec Server URL.
- `codex exec-server --listen ws://127.0.0.1:<port>` reaches `ready` through `environment/status`.
- `environment/info` returns the Exec Server's shell and cwd, proving that App Server is querying the external execution environment rather than its own host.
- The generated client schema contains the Host RPCs Shuttle routes remotely: `fs/readDirectory`, `fs/readFile`, `fs/watch`, `fuzzyFileSearch/sessionStart`, `process/spawn`, and `command/exec`.
- The macOS desktop SSH bootstrap currently probes `codex` through an interactive login shell, starts `codex -c features.code_mode_host=true app-server --listen unix://`, and then runs `codex app-server proxy`. Shuttle's shim emulates this control-socket lifecycle while exiting after the proxy disconnects.
- Shuttle carries App, Exec Server, and Host App Server traffic as independent Yamux streams over one ordinary SSH command's stdin/stdout. Each stream begins with Shuttle's versioned type header. Compatibility does not depend on `AllowTcpForwarding` or `AllowStreamLocalForwarding`.
- The installer may reuse an existing remote Codex only when `codex --version` exactly matches the pinned local version and `codex exec-server --help` succeeds. Readiness still requires live execution-environment and remote filesystem probes; version matching alone is never sufficient.

The automated `cxs-probe` regenerates the experimental schema and checks the required fields. The extended matrix remains a separate verification level beyond the automatic build gates.

## Failure policy

If any required method or field is absent, `doctor` fails. Shuttle must not silently fall back to local execution because doing so would violate its data-location and security guarantees.
