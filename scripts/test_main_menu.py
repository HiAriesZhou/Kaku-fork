#!/usr/bin/env python3
"""Exercise the built CLI menu in a real terminal, checking the advertised exit shortcut."""

import errno
import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import sys
import tempfile
import termios
import time
import unittest


BINARY = str(Path(sys.argv.pop(1) if len(sys.argv) > 1 else "dist/Kaku.app/Contents/MacOS/kaku").absolute())
ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


class Menu:
    def __init__(self, cols, rows):
        self.home = tempfile.TemporaryDirectory(prefix="kaku-menu-test-")
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
            env = {**os.environ, "HOME": self.home.name, "TERM": "xterm-256color"}
            env.pop("NO_COLOR", None)
            os.execve(BINARY, [BINARY], env)

    def frame(self):
        data = b""
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if not select.select([self.fd], [], [], 0.1)[0]:
                continue
            try:
                chunk = os.read(self.fd, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            data += chunk
            if b"Esc" in data and data.endswith(b"\r\n"):
                return ANSI.sub("", data.decode())
        raise AssertionError("The menu did not render its complete footer")

    def close(self):
        if self.pid:
            os.kill(self.pid, signal.SIGTERM)
            os.waitpid(self.pid, 0)
        os.close(self.fd)
        self.home.cleanup()


class MainMenuTests(unittest.TestCase):
    def test_advertised_exit_keys_close_the_menu(self):
        for key in (b"q", b"Q", b"\x1b"):
            with self.subTest(key=key):
                menu = Menu(100, 24)
                try:
                    self.assertIn("Q Quit", menu.frame())
                    os.write(menu.fd, key)
                    deadline = time.monotonic() + 2
                    while time.monotonic() < deadline:
                        pid, status = os.waitpid(menu.pid, os.WNOHANG)
                        if pid:
                            menu.pid = None
                            self.assertEqual(os.waitstatus_to_exitcode(status), 0)
                            break
                        time.sleep(0.02)
                    else:
                        self.fail("The advertised exit key did not close the menu")
                finally:
                    menu.close()


if __name__ == "__main__":
    unittest.main()
