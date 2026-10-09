#!/usr/bin/env python3
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("prepare_release", Path(__file__).with_name("prepare-release.py"))
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)
TARGETS = ["aarch64-apple-darwin", "x86_64-unknown-linux-musl"]


def ready_release():
    names = [f"codex-package-{target}.tar.gz" for target in TARGETS] + ["codex-package_SHA256SUMS"]
    return {
        "draft": False, "prerelease": False, "published_at": "2026-10-07T15:58:45Z",
        "assets": [{"name": name, "state": "uploaded", "size": 100} for name in names],
    }


class PreparationTests(unittest.TestCase):
    def select(self, releases, *, requested="", published=(), refs=None):
        calls = []

        def fake_api(endpoint, **kwargs):
            calls.append(endpoint)
            if "matching-refs" in endpoint:
                return [[{"ref": f"refs/tags/rust-v{version}"} for version in (refs or releases)]]
            if "repos/example/shuttle/" in endpoint:
                version = endpoint.rsplit("-codex.", 1)[1]
                return {} if version in published else None
            return releases[endpoint.rsplit("rust-v", 1)[1]]

        with patch.object(prepare, "api", side_effect=fake_api), contextlib.redirect_stderr(io.StringIO()):
            result = prepare.select_version("0.3.2", "0.150.0", TARGETS, requested, "example/shuttle")
        return result, calls

    def test_tag_without_release_is_skipped(self):
        result, _ = self.select({"0.161.0": None})
        self.assertEqual(result, ("", False))

    def test_incomplete_release_does_not_block_later_ready_version(self):
        incomplete = ready_release()
        incomplete["assets"].pop()
        result, _ = self.select({"0.161.0": incomplete, "0.162.0": ready_release()})
        self.assertEqual(result, ("0.162.0", True))

    def test_missing_release_does_not_block_later_ready_version(self):
        result, _ = self.select({"0.161.0": None, "0.162.0": ready_release()})
        self.assertEqual(result, ("0.162.0", True))

    def test_oldest_ready_unpublished_version_is_selected(self):
        result, _ = self.select({"0.162.0": ready_release(), "0.161.0": ready_release()})
        self.assertEqual(result, ("0.161.0", True))

    def test_published_versions_are_skipped_before_upstream_lookup(self):
        result, calls = self.select({"0.160.1": None, "0.161.0": ready_release()}, published=("0.160.1",))
        self.assertEqual(result, ("0.161.0", True))
        self.assertNotIn("repos/openai/codex/releases/tags/rust-v0.160.1", calls)

    def test_baseline_and_prerelease_tags_are_excluded(self):
        result, _ = self.select({"0.161.0": ready_release()}, refs=["0.149.0", "0.162.0-alpha.1", "0.161.0"])
        self.assertEqual(result, ("0.161.0", True))

    def test_explicit_unready_version_fails_clearly(self):
        with self.assertRaisesRegex(RuntimeError, "Codex 0.161.0: upstream Release is not published"):
            self.select({"0.161.0": None}, requested="0.161.0")

    def test_explicit_ready_version_can_publish(self):
        result, _ = self.select({"0.161.0": ready_release()}, requested="0.161.0")
        self.assertEqual(result, ("0.161.0", True))

    def test_explicit_published_version_is_noop(self):
        result, _ = self.select({"0.161.0": None}, requested="0.161.0", published=("0.161.0",))
        self.assertEqual(result, ("0.161.0", False))

    def test_draft_prerelease_and_unpublished_release_are_unready(self):
        for field, value in [("draft", True), ("prerelease", True), ("published_at", None)]:
            with self.subTest(field=field):
                release = ready_release()
                release[field] = value
                self.assertIsNotNone(prepare.readiness_error(release, TARGETS))

    def test_empty_or_uploading_assets_are_unready(self):
        for field, value in [("size", 0), ("state", "starter")]:
            with self.subTest(field=field):
                release = ready_release()
                release["assets"][0][field] = value
                self.assertIn("assets are not ready", prepare.readiness_error(release, TARGETS))

    def test_only_404_is_treated_as_missing(self):
        for error in ["gh: Forbidden (HTTP 403)", "gh: Bad Gateway (HTTP 502)", "connection refused"]:
            with self.subTest(error=error), patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "", error)):
                with self.assertRaises(RuntimeError):
                    prepare.api("repos/example/release", allow_missing=True)
        with patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "", "gh: Not Found (HTTP 404)")):
            self.assertIsNone(prepare.api("repos/example/release", allow_missing=True))
            with self.assertRaises(RuntimeError):
                prepare.api("repos/example/release")

    def test_api_success_and_pagination(self):
        with patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps([[{"ref": "tag"}]]), "")) as run:
            self.assertEqual(prepare.api("refs", paginate=True), [[{"ref": "tag"}]])
            self.assertEqual(run.call_args.args[0], ["gh", "api", "refs", "--paginate", "--slurp"])

    def test_tag_listing_failure_is_not_a_successful_noop(self):
        with patch.object(prepare, "api", side_effect=RuntimeError("HTTP 502")):
            with self.assertRaisesRegex(RuntimeError, "HTTP 502"):
                prepare.select_version("0.3.2", "0.150.0", TARGETS, "", "example/shuttle")

    def test_push_checks_actual_tag_not_canonical_tag(self):
        with patch.object(prepare, "api", side_effect=[None, ready_release()]) as api:
            result = prepare.select_version("0.3.2", "0.150.0", TARGETS, "0.161.0", "example/shuttle", "v0.3.3-codex.0.161.0")
        self.assertEqual(result, ("0.161.0", True))
        self.assertEqual(api.call_args_list[0].args[0], "repos/example/shuttle/releases/tags/v0.3.3-codex.0.161.0")

    def test_workflow_outputs_and_push_version_selection(self):
        cases = [
            ("schedule", "", "", ("", False), "", ""),
            ("workflow_dispatch", "", "0.161.0", ("0.161.0", True), "0.161.0", "v0.3.2-codex.0.161.0"),
            ("push", "v0.3.3-codex.0.161.0", "", ("0.161.0", True), "0.161.0", "v0.3.3-codex.0.161.0"),
            ("push", "v0.3.2", "", ("0.150.1", True), "0.150.1", "v0.3.2"),
        ]
        for event, ref, requested, result, selected, tag in cases:
            with self.subTest(event=event, ref=ref), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "output"
                env = {"GITHUB_EVENT_NAME": event, "GITHUB_REF_NAME": ref, "REQUESTED_CODEX_VERSION": requested,
                       "GITHUB_REPOSITORY": "example/shuttle", "GITHUB_OUTPUT": str(output)}
                with patch.dict(os.environ, env), patch.object(prepare, "select_version", return_value=result) as select, contextlib.redirect_stdout(io.StringIO()):
                    prepare.main()
                self.assertEqual(select.call_args.args[3], selected)
                self.assertEqual(output.read_text(), f"codex-version={result[0]}\nrelease-tag={tag}\nshould-publish={str(result[1]).lower()}\n")


if __name__ == "__main__":
    unittest.main()
