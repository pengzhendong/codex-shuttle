# Troubleshooting

Start with:

```bash
cxs status <profile>
cxs doctor <profile>
```

`doctor` is the source of truth for the complete local Codex, SSH, remote runtime, execution-environment, and filesystem path.

## SSH still asks for a password

Shuttle requires non-interactive key authentication because the desktop app cannot reliably answer an SSH password prompt.

```bash
ssh -o BatchMode=yes <ssh-host> true
```

Fix the original host entry in `~/.ssh/config`, then refresh the stored SSH snapshot:

```bash
cxs add <ssh-host> --name <profile>
cxs doctor <profile>
```

Shuttle writes only its managed `cxs-*` aliases. It does not rewrite the original host entry.

## The Codex app shows a Mac path

Choose the generated host alias, normally `cxs-<profile>`, and open a Linux path such as `/home/me/project`. A project opened through the ordinary local connection remains local; Shuttle does not reinterpret an existing Mac path as a remote one.

## No matching runtime release

The installed `cxs` binary is bound to one immutable GitHub Release. Confirm the local Codex source baseline and select the matching Shuttle Release. If the server cannot reach GitHub, use Mac-side download:

```bash
cxs install <profile> --local-download
```

For development builds, provide a locally built package explicitly:

```bash
cxs install <profile> \
  --runtime-package ./cxs-runtime-*.tar.gz \
  --shim ./cxs-shim
```

## A session is missing after changing Provider

Close Codex before changing its local rollout or SQLite state, then run:

```bash
cxs repair
```

The command creates a backup before updating matching Provider and working-directory metadata. If the session exists only on the Linux host, import it first:

```bash
cxs sync <profile>
```

`sync` never overwrites an existing local thread ID and never copies the remote SQLite database over the Mac database.

## The bridge stopped after SSH config changed

Verify the original alias, refresh the profile snapshot, and restart the bridge:

```bash
ssh -o BatchMode=yes <ssh-host> true
cxs add <ssh-host> --name <profile>
cxs down <profile>
cxs up <profile>
cxs doctor <profile>
```

If the App reports `Failed to update connection` immediately after a Shuttle
upgrade, update the remote shim as well. Current releases use a versioned App
control socket so a stale process from an older release cannot receive the new
WebSocket proxy connection.

## An update broke the remote runtime

Switch to the previous verified remote release:

```bash
cxs rollback <profile>
cxs doctor <profile>
```

Rollback changes only Shuttle's managed remote release symlink. It does not modify the server's system packages or the original SSH host configuration.

## Collecting diagnostics

Before filing an issue, include:

- `cxs --version`
- local Codex version
- `cxs status <profile>` output
- the failing `cxs doctor <profile>` check
- Mac and remote CPU architectures

Remove hostnames, usernames, paths, tokens, SSH options, and session content. Never post `~/.ssh/config`, profile tokens, Codex credentials, or private rollout files publicly.
