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
| Remote execution semantics | OpenAI's official Codex Linux package | version qualification, activation, and relay |
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

The installer always selects OpenAI's official source-version and
architecture-matched `codex-package-<target>.tar.gz`. The SSH host downloads it
by default; `--local-download` moves that download to the Mac and uploads the
same verified archive. There is no custom runtime, existing-binary fallback,
or system-wide Codex dependency.

The public source baseline is used for desktop prereleases (for example,
`0.147.0-alpha.6.5` selects official `0.147.0`). The official package and
Shuttle shim both retain SHA-256 fingerprints and must pass the live App
Server-to-Exec Server plus remote-filesystem readiness probe.
