# Contributing

Codex Shuttle is version-bound to Codex App Server and experimental Exec Server interfaces. Changes that affect transport, installation, session import, or App Server adaptation should include a regression test and update the compatibility contract.

Before submitting a change, run:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

Changes to the remote shim should also be cross-compiled for both `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`. Automated releases establish build compatibility; record a release as fully verified only after an actual App Server-to-remote Exec Server probe succeeds.

Keep the dependency boundary narrow: prefer OpenSSH, Yamux, Tokio, Tungstenite, Serde, and Codex Exec Server over project-specific replacements. New dependencies should remove more maintenance or risk than they add.

Keep `README.md` as the default English entry point and mirror user-facing changes in `README.zh-CN.md`. Put protocol detail in `docs/architecture.md`, release mechanics in `docs/runtime-release.md`, and operational fixes in `docs/troubleshooting.md` instead of growing the quick start indefinitely.
