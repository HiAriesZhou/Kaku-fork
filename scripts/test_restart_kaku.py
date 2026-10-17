#!/usr/bin/env python3
"""Run restart branches with fake processes; never signal or open a real app."""
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]


class RestartTests(unittest.TestCase):
    def run_fixture(self, pids, owner=None, detached=True, extra_env=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            gui = root / "Kaku.app/Contents/MacOS/kaku-gui"
            gui.parent.mkdir(parents=True)
            gui.touch()
            gui.chmod(0o755)
            (root / "Library/Logs").mkdir(parents=True)
            (root / "running").write_text("".join(f"{pid}\n" for pid in pids))
            if owner is not None:
                session = root / ".config/kaku/last_session.json"
                session.parent.mkdir(parents=True)
                session.write_text(f'{{"content_dir":"session-{owner}-fixture"}}')
            # Override every process-facing command, including bash's kill.
            # The absolute open call is replaced only in this temporary copy.
            source = (ROOT / "scripts/restart_kaku.sh").read_text()
            self.assertEqual(source.count('/usr/bin/open "$app"'), 1)
            source = source.replace('/usr/bin/open "$app"', 'fixture_open "$app"')
            self.assertEqual(source.count("/usr/bin/nohup "), 1)
            source = source.replace("/usr/bin/nohup ", "fixture_nohup ")
            mocks = r'''
ps() {
    if [[ "$*" == *"-p "* ]]; then
        [[ -n "${!#}" ]] || return 1
    fi
    while read -r pid; do
        printf '%s %s/Contents/MacOS/kaku-gui\n' "$pid" "$FIXTURE_APP"
    done < "$HOME/running"
}
kill() {
    if [[ "$1" == -0 ]]; then grep -qx "$2" "$HOME/running"; return; fi
    printf '%s %s\n' "$1" "$2" >> "$HOME/calls"
    awk -v pid="$2" '$0 != pid' "$HOME/running" > "$HOME/next"
    mv "$HOME/next" "$HOME/running"
}
sleep() { :; }
fixture_open() {
    echo open >> "$HOME/calls"
    echo 999 > "$HOME/running"
}
fixture_nohup() {
    printf 'detach env=%s args=%s\n' "${KAKU_RESTART_DETACHED:-unset}" "${*: -2}" >> "$HOME/calls"
}
'''
            script = root / "restart.sh"
            script.write_text(mocks + source)
            env = dict(os.environ, HOME=str(root), XDG_CONFIG_HOME=str(root / ".config"),
                       FIXTURE_APP=str(gui.parents[2]), KAKU_RESTART_DELAY="0",
                       **(extra_env or {}))
            args = ["--detached"] if detached else []
            result = subprocess.run(["bash", str(script), *args, str(gui.parents[2])],
                                    env=env, capture_output=True, text=True, timeout=5)
            if not detached:
                # The detach launcher runs in the background; give it a moment.
                for _ in range(40):
                    if (root / "calls").exists():
                        break
                    time.sleep(0.05)
            calls = (root / "calls").read_text().splitlines() if (root / "calls").exists() else []
            return result, calls

    def test_no_running_process_still_opens(self):
        result, calls = self.run_fixture([])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, ["open"])

    def test_missing_snapshot_terminates_all_then_opens(self):
        result, calls = self.run_fixture([101, 102])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, ["-TERM 101", "-TERM 102", "open"])

    def test_only_snapshot_owner_saves(self):
        result, calls = self.run_fixture([101, 102], owner=102)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, ["-TERM 102", "-KILL 101", "open"])

    def test_stale_owner_terminates_all(self):
        result, calls = self.run_fixture([101, 102], owner=103)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, ["-TERM 101", "-TERM 102", "open"])

    def test_leaked_detach_variable_still_detaches(self):
        # `open` hands its environment to the app, so Kaku reopened by an
        # older version of this script carries KAKU_RESTART_DETACHED=1 into
        # every shell. That must not skip detaching, and must not be passed on.
        result, calls = self.run_fixture([101], detached=False,
                                         extra_env={"KAKU_RESTART_DETACHED": "1"})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Restarting", result.stdout)
        self.assertEqual(len(calls), 1, calls)
        self.assertTrue(calls[0].startswith("detach env=unset args=--detached "), calls)


if __name__ == "__main__":
    unittest.main()
