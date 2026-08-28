# Architecture

```text
Codex desktop app
       | SSH runs `codex app-server proxy`
       v
remote cxs-shim control socket
       | logical `app` channel
       v
 remote cxs-agent ═══ one ordinary SSH stdin/stdout session ═══ local cxs-bridge
       |                                                              |
       ├─ remote codex exec-server <──── logical `exec` channel ──────┤
       └─ restricted Host App Server <─ logical `host` channel ───────┤
                                                                      └─ local App Server
                           host RPC routing + merged initialize + environment/add
```

## Stable project-owned boundary

Shuttle owns profile state, SSH discovery, managed configuration, artifact checks, authenticated handshakes, typed Yamux streams, and child-process cleanup. It delegates SSH to the user's OpenSSH client so existing `ProxyJump`, `ProxyCommand`, agent, hardware-key, host-key, and control-socket behavior remains authoritative. Yamux owns multiplexing, flow control, and stream closure. Tokio/Tungstenite own async I/O and WebSocket framing. Shuttle does not request SSH TCP or Unix-socket forwarding and does not implement any of those protocols itself.

The shim does not parse App Server messages. It supports both the older direct stdio invocation and the current desktop bootstrap sequence (`codex -c ... app-server --listen unix://`, followed by `codex app-server proxy`). For the latter it exposes the control socket expected by the App and connects to the per-profile agent socket on the remote Unix host.

`cxs up` opens one long-lived ordinary SSH command running the hidden agent mode of `cxs-shim`. `cxs-mux` runs the standard [Yamux protocol](https://github.com/hashicorp/yamux/blob/master/spec.md) over that command's stdin/stdout. The bridge is the Yamux client and the agent is the server; both may open bidirectional streams. A five-byte Shuttle header (`CXS2` plus a type byte) identifies each stream as `app`, `exec`, or `host`. Agent-originated `app` streams reach the local adapter. Bridge-originated `exec` streams reach the remote Exec Server, while `host` streams start restricted JSONL App Servers on the remote host. The stream wrapper explicitly wakes the Yamux driver before writes so an agent-originated first stream cannot stall while waiting for unrelated traffic.

The adapter caps each session at 32 concurrent streams and 32 MiB of aggregate receive-window growth. Stream identifiers, frame splitting, flow control, backpressure, and close semantics come from the maintained Yamux implementation rather than a project-specific framing protocol.

The bridge terminates the desktop WebSocket locally and opens both a supervised local App Server and a restricted remote Host App Server. Session methods such as initialize, thread, turn, account, and configuration remain on the desktop. `fs/*`, `fuzzyFileSearch*`, `process/*`, and `command/exec*` are routed remotely. The initialize request is sent to both servers; the bridge keeps the local response but replaces its host identity fields with the remote `userAgent`, `codexHome`, `platformFamily`, and `platformOs`. The supervised App Server uses a private Unix socket on macOS and a loopback WebSocket on Windows.

The same adapter enables the experimental API, registers the execution environment, waits for `environment/status=ready`, and attaches it to `thread/start`/`turn/start`. Requests arriving during initialization are bounded and queued. Ping, pong, close, and non-JSON frames are forwarded without semantic changes. The older direct JSONL transport remains available for execution-environment compatibility; full dual-App-Server host routing targets the desktop WebSocket bootstrap.

## Version-sensitive upstream boundary

The baseline validated Codex version, 0.147.0, exposes an experimental `exec-server` WebSocket transport. Its generated experimental App Server schema includes `environment/add`, `environment/status`, and sticky `environments` fields on thread and turn requests. Shuttle uses these generated contracts instead of relying on the draft `[[environments]] program/args` configuration. Each Shuttle release is qualified against one matching `rust-vX.Y.Z` release because generated schemas and executable behavior can differ by version.

After the desktop client initializes App Server, the bridge sends:

```json
{
  "method": "environment/add",
  "params": {
    "environmentId": "cxs-gpu",
    "execServerUrl": "ws://127.0.0.1:<local-mux-port>"
  }
}
```

The bridge accepts that desktop-local loopback connection, opens an `exec` mux channel, and the remote agent connects it to the Exec Server's private loopback listener. Each agent chooses a free loopback port if the configured preferred port is occupied, so a stale older agent cannot block recovery. The bridge waits until App Server reports the environment as ready, then adds the environment id and remote `cwd` to `thread/start`; later turns inherit the sticky selection.

The local App Server is the authority for thread and turn persistence. The remote Host App Server uses an isolated `CODEX_HOME`, has plugins and apps disabled, and exists only to provide Codex's maintained host RPC implementations. This gives the App remote paths and remote browsing without moving the session database or credentials off the desktop.

Pre-existing sessions created directly on a server are imported explicitly by
`cxs sync`. Shuttle copies rollout files only; it never replaces the desktop's
SQLite database with a remote database. Codex's local thread scanner indexes
new rollouts, while `cxs repair` transactionally aligns provider and workspace
metadata for rows that already exist.

Shuttle does not synthesize Desktop-private project assignments. Imported
Remote paths remain remote paths and become usable when the session is resumed
through the Shuttle host/environment. Treating them as desktop-local projects
would route filesystem work to the wrong machine and couple Shuttle to an
unstable Electron state format.

For `thread/start`, Shuttle first asks the remote Host App Server to create an ephemeral shadow thread with the same remote `cwd`. The Host App Server performs Codex's native instruction hierarchy and Git discovery. Shuttle reads the returned instruction sources through `fs/readFile`, injects their contents into the local request's `developerInstructions`, and later merges only environment-native response metadata (`cwd`, runtime roots, instruction-source paths, and Git info). The local thread id and persistence fields remain authoritative. Discovery is capped at 32 instruction files, 256 KiB per file, 512 KiB total, and a three-second timeout. Because the shadow is ephemeral, it is never materialized as a remote session.

Reimplementing Codex's process, filesystem, PTY, HTTP, and sandbox RPC semantics in Shuttle is explicitly out of scope.

## Profile states

- `prepared`: local profile and SSH facts exist; no compatible executor has been proven.
- `installed`: matching remote artifacts are installed and verified.
- `ready`: the generated SSH alias completed the shim handshake, merged App Server initialization, remote Exec Server `environment/status=ready`, and a remote directory-read probe.
- `broken`: a previously ready profile no longer passes its pinned-version checks.

Only `ready` profiles should be exposed as usable App connections.

Filesystem, process, PTY, diff, sandbox, resume/fork, and disconnect-cleanup qualification remains a release test matrix in addition to the per-install readiness probe.

## Remote layout

```text
~/.local/bin/codex -> ~/.local/lib/codex-shuttle/current/cxs-shim
~/.local/lib/codex-shuttle/
├── current -> releases/<release>
├── previous -> releases/<release>
├── releases/<release>/
│   ├── cxs-shim
│   ├── bin/codex
│   ├── bin/codex-code-mode-host
│   ├── codex-path/rg
│   ├── codex-resources/bwrap
│   ├── executor.path
│   ├── shim.json
│   └── install.json
└── profiles/<profile>/
    ├── token
    ├── agent.sock
    ├── app-<shim-digest>.sock
    └── codex-home/
        └── host-app-server/
```

The active release contains the small Shuttle shim, OpenAI's official Codex package, and metadata. The official package stays private to Shuttle and is not installed as a system package. Rollback validates the recorded executor before switching symlinks.

The App control socket includes the shim digest. After an update, a newly
started desktop connection cannot accidentally reuse a still-running control
listener from an older shim protocol; the idle older listener exits on its own.

This split is deliberate: official Codex continues to own its Exec Server, App Server, process, filesystem, PTY, HTTP, sandbox, and approval semantics. Shuttle owns only the remote transport and environment adaptation.
