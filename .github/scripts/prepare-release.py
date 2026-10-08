#!/usr/bin/env python3
"""Select an unpublished Codex version whose upstream packages are ready."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys


def api(endpoint, *, allow_missing=False, paginate=False):
    command = ["gh", "api", endpoint]
    if paginate:
        command += ["--paginate", "--slurp"]
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    if result.returncode:
        if allow_missing and "(HTTP 404)" in result.stderr:
            return None
        raise RuntimeError(f"GitHub API {endpoint}: {result.stderr.strip()}")
    return json.loads(result.stdout)


def readiness_error(release, targets):
    if release is None:
        return "upstream Release is not published yet"
    if release.get("draft") or release.get("prerelease") or not release.get("published_at"):
        return "upstream Release is not a published stable release"
    required = {f"codex-package-{target}.tar.gz" for target in targets}
    required.add("codex-package_SHA256SUMS")
    uploaded = {
        asset["name"]
        for asset in release.get("assets", [])
        if asset.get("state") == "uploaded" and asset.get("size", 0) > 0
    }
    missing = sorted(required - uploaded)
    if missing:
        return f"upstream assets are not ready: {', '.join(missing)}"
    return None


def select_version(shuttle_version, baseline, targets, requested, repository, release_tag=None):
    if requested:
        if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", requested):
            raise RuntimeError(f"Invalid Codex version: {requested}")
        tag = release_tag or f"v{shuttle_version}-codex.{requested}"
        if api(f"repos/{repository}/releases/tags/{tag}", allow_missing=True) is not None:
            return requested, False
        release = api(f"repos/openai/codex/releases/tags/rust-v{requested}", allow_missing=True)
        error = readiness_error(release, targets)
        if error:
            raise RuntimeError(f"Codex {requested}: {error}")
        return requested, True

    pages = api("repos/openai/codex/git/matching-refs/tags/rust-v", paginate=True)
    versions = set()
    for page in pages:
        for ref in page:
            match = re.fullmatch(r"refs/tags/rust-v([0-9]+\.[0-9]+\.[0-9]+)", ref["ref"])
            if match:
                versions.add(match[1])
    version_key = lambda value: tuple(map(int, value.split(".")))
    for candidate in sorted(versions, key=version_key):
        if version_key(candidate) < version_key(baseline):
            continue
        tag = f"v{shuttle_version}-codex.{candidate}"
        if api(f"repos/{repository}/releases/tags/{tag}", allow_missing=True) is not None:
            continue
        release = api(f"repos/openai/codex/releases/tags/rust-v{candidate}", allow_missing=True)
        error = readiness_error(release, targets)
        if error:
            print(f"Skipping Codex {candidate}: {error}.", file=sys.stderr)
            continue
        return candidate, True
    return "", False


def main():
    cargo = Path("Cargo.toml").read_text()
    workspace = cargo.split("[workspace.package]", 1)[1].split("\n[", 1)[0]
    shuttle_version = re.search(r'^version = "([^"]+)"', workspace, re.MULTILINE)[1]
    versions = re.findall(r"^[0-9]+\.[0-9]+\.[0-9]+$", Path("runtime/codex-versions.txt").read_text(), re.MULTILINE)
    platforms = json.loads(Path("runtime/platforms.json").read_text())["platforms"]
    targets = [platform["rust_target"] for platform in platforms if platform["remote"]]
    requested = os.environ.get("REQUESTED_CODEX_VERSION", "")
    push_tag = ""
    if os.environ["GITHUB_EVENT_NAME"] == "push":
        push_tag = os.environ["GITHUB_REF_NAME"]
        requested = requested or (push_tag.rsplit("-codex.", 1)[1] if "-codex." in push_tag else versions[-1])
    version, publish = select_version(shuttle_version, versions[0], targets, requested, os.environ["GITHUB_REPOSITORY"], push_tag)
    tag = (push_tag or f"v{shuttle_version}-codex.{version}") if version else ""
    print(f"Codex version: {version or 'none ready'}; release: {tag}; publish: {publish}")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
        output.write(f"codex-version={version}\nrelease-tag={tag}\nshould-publish={str(publish).lower()}\n")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, ValueError, KeyError, IndexError, OSError) as error:
        print(f"Release preparation failed: {error}", file=sys.stderr)
        sys.exit(1)
