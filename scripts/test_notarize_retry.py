#!/usr/bin/env python3
"""Exercise signing retries with isolated commands, without Apple or Keychain access."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class RetryTests(unittest.TestCase):
    def run_fixture(self, backend, error, *, transient=False, max_attempts=None, build=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            scripts = root / "scripts"
            scripts.mkdir()
            bin_dir = root / "bin"
            bin_dir.mkdir()
            (root / "dist/Kaku.app").mkdir(parents=True)
            (root / "dist/Kaku.dmg").touch()
            (root / "api.json").write_text("{}")
            for name, source in {
                "security": "exit 99",
                "sleep": "exit 0",
                "codesign": '''if [[ "$1" == -dvvvv ]]; then
 echo 'Authority=Developer ID Application: Fixture'
 echo 'TeamIdentifier=ABCDEFGHIJ'
fi
exit 0''',
                "rcodesign": '''echo rcodesign >> "$CALLS"
if [[ "$BACKEND" == fallback ]]; then echo 'unsupported key format' >&2; exit 1; fi
count=$(grep -c '^rcodesign$' "$CALLS")
if [[ "$TRANSIENT" == 1 && "$count" == 2 ]]; then exit 0; fi
echo "$ERROR" >&2
exit 1''',
                "xcrun": '''echo "$*" >> "$CALLS"
if [[ "$1" == stapler ]]; then exit 68; fi
count=$(grep -c '^notarytool submit' "$CALLS")
if [[ "$TRANSIENT" == 1 && "$count" == 2 ]]; then echo 'status: Accepted'; exit 0; fi
echo "$ERROR" >&2
exit 1''',
            }.items():
                path = bin_dir / name
                path.write_text("#!/bin/bash\n" + source + "\n")
                path.chmod(0o755)
            env = dict(os.environ, PATH=f"{bin_dir}:/usr/bin:/bin", CALLS=str(root / "calls"),
                       BACKEND=backend, ERROR=error, TRANSIENT=str(int(transient)),
                       KAKU_ASC_API_KEY_PATH=str(root / "api.json") if backend != "notarytool" else "/dev/null",
                       KAKU_NOTARYTOOL_PROFILE="fixture", NOTARY_SUBMIT_RETRY_DELAY="0", OUT_DIR="dist")
            env.pop("NOTARY_SUBMIT_MAX_ATTEMPTS", None)
            if max_attempts:
                env["NOTARY_SUBMIT_MAX_ATTEMPTS"] = str(max_attempts)
            if build:
                source = (ROOT / "scripts/build.sh").read_text()
                source = source[source.index("CODESIGN_TIMESTAMP_ERROR_PATTERN="):source.index("\nensure_rust_targets()")]
                source += '\ncodesign_with_retry fixture\n'
                (bin_dir / "codesign").write_text('#!/bin/bash\necho codesign >> "$CALLS"\necho "$ERROR" >&2\nexit 23\n')
            else:
                source = (ROOT / "scripts/notarize.sh").read_text()
            script = scripts / "notarize.sh"
            script.write_text(source)
            result = subprocess.run(["bash", str(script)], env=env, text=True, capture_output=True, timeout=10)
            calls = (root / "calls").read_text().splitlines() if (root / "calls").exists() else []
            return result, calls

    def test_rcodesign_network_stops_without_fallback(self):
        for error in ("s3 upload error: HTTP connect timeout occurred after 3.1s", "ThroughputBelowMinimum"):
            with self.subTest(error=error):
                result, calls = self.run_fixture("rcodesign", error)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, ["rcodesign"] * 2)
                self.assertIn(error, result.stdout + result.stderr)

    def test_override_cannot_expand_network_budget(self):
        _, calls = self.run_fixture("rcodesign", "dispatch failure", max_attempts=5)
        self.assertEqual(calls, ["rcodesign"] * 2)

    def test_lower_retry_budget_is_preserved(self):
        for backend in ("rcodesign", "notarytool"):
            with self.subTest(backend=backend):
                _, calls = self.run_fixture(backend, "ThroughputBelowMinimum", max_attempts=1)
                self.assertEqual(len(calls), 1)

    def test_notarytool_keeps_transport_and_stops(self):
        _, calls = self.run_fixture("notarytool", "connectTimeout")
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[0], calls[1])
        self.assertNotIn("--no-s3-acceleration", calls[1])

    def test_nonnetwork_fallback_is_preserved(self):
        _, calls = self.run_fixture("fallback", "deadlineExceeded")
        self.assertEqual(calls[0], "rcodesign")
        self.assertEqual(len(calls), 3)
        self.assertTrue(all(c.startswith("notarytool submit") for c in calls[1:]))

    def test_nonnetwork_notarytool_failure_is_not_retried(self):
        _, calls = self.run_fixture("notarytool", "Invalid credentials")
        self.assertEqual(len(calls), 1)

    def test_retry_success_reaches_stapling(self):
        for backend in ("rcodesign", "notarytool"):
            with self.subTest(backend=backend):
                _, calls = self.run_fixture(backend, "ThroughputBelowMinimum", transient=True)
                self.assertEqual(len(calls), 3)
                self.assertTrue(calls[-1].startswith("stapler staple"))

    def test_nonnetwork_signing_error_preserves_exit_without_retry(self):
        result, calls = self.run_fixture("", "invalid signing identity", build=True)
        self.assertEqual(result.returncode, 23)
        self.assertEqual(calls, ["codesign"])

    def test_timestamp_failure_retries_once_and_preserves_exit(self):
        for error in ("timestamp service is not available", "A timestamp was expected but was not found"):
            with self.subTest(error=error):
                result, calls = self.run_fixture("", error, build=True)
                self.assertEqual(result.returncode, 23)
                self.assertEqual(calls, ["codesign"] * 2)


class TapRetryTests(unittest.TestCase):
    def run_fixture(self, mode):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Kaku.dmg").touch()
            gh = root / "gh"
            gh.write_text("""#!/bin/bash
if [[ "$*" != *contents/Casks* ]]; then exit 0; fi
echo request >> "$CALLS"
count=$(wc -l < "$CALLS" | tr -d ' ')
case "$MODE" in
network) echo 'error connecting to api.github.com' >&2; exit 1 ;;
permission) echo 'gh: HTTP 403: Forbidden' >&2; exit 1 ;;
transient) if [[ "$count" == 1 ]]; then echo 'dial tcp: i/o timeout' >&2; exit 1; fi ;;
esac
if [[ "$MODE" == normal && "$count" -lt 4 ]]; then
 echo '  version "0.0.1"'
else
 echo '  version "1.0.0"'
fi
""")
            gh.chmod(0o755)
            source = (ROOT / "scripts/release.sh").read_text()
            source = source[source.index("update_homebrew_tap() {"):source.index("# Main release flow")]
            source = """set -euo pipefail
log_info() { echo "$*"; }
log_warn() { echo "$*" >&2; }
die() { echo "$*" >&2; exit 1; }
sleep() { :; }
""" + source + '\nupdate_homebrew_tap 1.0.0\n'
            script = root / "test.sh"
            script.write_text(source)
            env = dict(os.environ, PATH=f"{root}:/usr/bin:/bin", CALLS=str(root / "calls"),
                       MODE=mode, OUT_DIR=str(root), HOMEBREW_TAP_TOKEN="fixture", HOMEBREW_TAP_REPO="fixture/tap",
                       REQUIRE_HOMEBREW_TAP_UPDATE="1", HOMEBREW_TAP_VERIFY_ATTEMPTS="4",
                       HOMEBREW_TAP_VERIFY_SLEEP_SECONDS="0")
            result = subprocess.run(["bash", str(script)], env=env, text=True, capture_output=True, timeout=10)
            return result, len((root / "calls").read_text().splitlines())

    def test_network_failure_stops_after_one_retry(self):
        result, count = self.run_fixture("network")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(count, 2)
        self.assertIn("error connecting to api.github.com", result.stderr)

    def test_permission_failure_is_not_retried(self):
        result, count = self.run_fixture("permission")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(count, 1)
        self.assertIn("HTTP 403", result.stderr)

    def test_normal_version_polling_keeps_its_budget(self):
        result, count = self.run_fixture("normal")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(count, 4)

    def test_transient_network_failure_recovers(self):
        result, count = self.run_fixture("transient")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(count, 2)


if __name__ == "__main__":
    unittest.main()
