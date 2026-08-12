# Security policy

Codex Shuttle is under active development. Every release is bound to one Codex source version as documented in `docs/compatibility.md`; compatibility is not assumed across versions.

Please report vulnerabilities privately through GitHub's security-advisory feature for this repository. Do not include credentials, private SSH configuration, profile tokens, or remote-host data in a public issue.

Security-sensitive changes must preserve these invariants:

- OpenSSH remains responsible for host identity and transport security.
- Profile tokens and remote configuration stay user-private.
- No Mac Codex authentication state is copied to the remote host.
- Thread and turn persistence remains owned by the Mac App Server.
- The Linux Host App Server uses an isolated private `CODEX_HOME`, disables plugins/apps, and receives only the allowlisted host RPC family.
- Remote instruction files are bounded by count and size before their contents are injected into the Mac-owned thread.
- Remote execution never silently falls back to the Mac.
- Downloaded and reused executors are verified before activation.
- Installer removal is limited to paths owned by Codex Shuttle.
