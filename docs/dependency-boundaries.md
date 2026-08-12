# Dependency boundaries

Codex Shuttle should own only the adaptation that is specific to connecting Codex App Server to a remote Exec Server. General protocols and operating-system integration stay with maintained upstream implementations.

| Concern | Implementation used | Shuttle-owned code |
| --- | --- | --- |
| SSH transport, keys, host checks, jumps, agents | the user's OpenSSH client | profile snapshot and generated alias only |
| Multiplexing, flow control, stream close | `yamux` | a five-byte versioned App/Exec stream label |
| Async sockets, process supervision, timeouts | Tokio and `nix` | lifecycle policy and bounded timeouts |
| WebSocket framing | `tokio-tungstenite` | narrow JSON-RPC environment adaptation |
| JSON and persistent metadata | Serde and `serde_json` | versioned profile/install schemas |
| Atomic temporary files | `tempfile` | permissions and ownership policy |
| Cryptographic digest and token comparison | `sha2`, `getrandom`, and `subtle` | artifact identity and handshake fields |
| Remote execution semantics | OpenAI `codex-exec-server` and `codex-app-server` libraries | a narrow release-only entry point, version qualification, and relay |
| Artifact transfer | OpenSSH plus `curl` on the target | retry/resume and activation policy |
| Session archive transfer | OpenSSH plus `tar` | path/size validation and thread-ID deduplication |
| Codex state repair | `rusqlite` | online backup and schema-aware Provider/CWD updates |

The small project-owned `SplitIo` adapters exist only because Yamux uses the `futures` I/O traits while the rest of the program uses Tokio. `tokio-util::compat` performs the protocol-trait conversion; Shuttle does not implement buffering or framing there.

## Explicit non-goals

- Implementing an SSH client or SSH forwarding protocol.
- Maintaining a custom multiplexing frame format.
- Recreating Codex process, PTY, filesystem, sandbox, approval, or HTTP RPCs.
- Parsing or proxying arbitrary App Server semantics.
- Copying Mac Codex credentials or state to Linux.
- Replacing the Mac SQLite database with a remote copy.

The Rust session repair adapts the core approach from the MIT-licensed
`codex-provider-sync`. Attribution and its license are retained in
`THIRD_PARTY_NOTICES.md`; Shuttle owns only the narrowed import and repair
workflow.

## Executor selection

The installer uses this order:

1. An explicit local `--runtime-package`, when provided.
2. An explicit `--remote-codex` path, when provided.
3. The public-source-version/architecture-matched `cxs-runtime` from the current Shuttle release; the SSH host downloads it by default.

There is no automatic full-Codex fallback. A missing runtime is an unsupported version and installation fails.

An explicit full remote Codex must match the local version exactly. A managed runtime matches the public source baseline (for example `0.147.0-alpha.6.5` uses `0.147.0`). Both retain a recorded SHA-256 fingerprint and must pass the live App Server-to-Exec Server plus remote-filesystem readiness probe. `cxs-runtime` reuses OpenAI's Rust libraries; Shuttle does not maintain alternate process, PTY, filesystem, or sandbox implementations.
