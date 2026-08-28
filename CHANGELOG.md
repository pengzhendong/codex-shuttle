# Changelog

All notable changes to Codex Shuttle are documented here. The project follows semantic versioning once a stable public API exists.

## 0.1.0 - Unreleased

- Added the `cxs` profile, install, diagnostics, lifecycle, rollback, and removal CLI.
- Added a local App Server bridge and remote shim/agent.
- Added typed Yamux App, Exec, and Host streams over one ordinary OpenSSH stdio session, including driver wakeups for agent-originated first streams.
- Added dual App Server routing: Mac-owned sessions plus remote directory, file-search, process, command, and platform identity RPCs.
- Added bounded ephemeral Linux shadow-thread discovery so remote Git state and the actual remote `AGENTS.md` contents apply to Mac-owned threads without remote session persistence.
- Added safe refresh of an existing profile's resolved SSH snapshot after its source host configuration changes.
- Parallelized remote Codex package download and shim upload, while continuing to reuse a matching existing remote Codex whenever possible.
- Added exact-version Codex contract probes and real execution-environment plus remote-filesystem readiness verification.
- Extended `cxs doctor` to execute a marker command through App Server host RPC
  routing and verify the returned Linux home, kernel, and CPU architecture.
- Added dynamic remote Exec Server port fallback, restart-safe upstream socket names, and graceful SIGTERM bridge shutdown.
- Versioned the remote App control socket by shim digest so an update cannot
  reconnect the desktop proxy to a stale listener from an older release.
- Added automatic reuse of compatible remote Codex executors, with official verified package fallback.
- Added concurrent Codex transfer and shim upload with resumable remote downloads.
- Added immutable remote releases, executor-aware rollback, and reversible SSH/profile changes.
- Added macOS CLI and Linux-musl shim release artifacts for both supported architectures.
- Added a native Windows x86_64 CLI, loopback WebSocket App Server transport,
  Windows process lifecycle support, bundled SQLite, CI/release coverage, and
  a checksum-verifying PowerShell installer.
- Added `cxs sync <profile>` to import remote rollout sessions without
  overwriting local thread IDs.
- Added `cxs repair` for backed-up, transactional Provider and workspace
  metadata repair, adapted to Rust from the MIT-licensed
  `codex-provider-sync` approach.
- Added version-bound GitHub releases that discover every unpublished stable
  OpenAI Codex source tag, build all supported targets, smoke-test both remote
  runtimes, and publish only a complete checksummed artifact set.
- Cached per-architecture runtime builds, disabled costly runtime ThinLTO, and
  moved automatic upstream checks to an hourly schedule.
- Added English and Simplified Chinese project guides plus an operational
  troubleshooting guide.
