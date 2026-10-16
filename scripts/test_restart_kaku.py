#!/usr/bin/env python3
"""Run restart branches with fake processes; never signal or open a real app."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class RestartTests(unittest.TestCase):
    def run_fixture(self, pids, owner=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            gui = root / "Kaku.app/Contents/MacOS/kaku-gui"
            gui.parent.mkdir(parents=True)
            gui.touch()
            gui.chmod(0o755)
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
'''
            script = root / "restart.sh"
            script.write_text(mocks + source)
            env = dict(os.environ, HOME=str(root), XDG_CONFIG_HOME=str(root / ".config"),
                       FIXTURE_APP=str(gui.parents[2]), KAKU_RESTART_DETACHED="1",
                       KAKU_RESTART_DELAY="0")
            result = subprocess.run(["bash", str(script), str(gui.parents[2])],
                                    env=env, capture_output=True, text=True, timeout=5)
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


if __name__ == "__main__":
    unittest.main()
