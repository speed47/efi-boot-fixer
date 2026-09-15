#!/usr/bin/env python3
"""Check harness termination and exit-status handling without booting a VM."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HARNESS = Path(__file__).resolve().with_name("run-qemu.sh")


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "test.img").write_bytes(bytes(64 * 512))
        (self.root / "pristine.fd").write_bytes(b"firmware variables")
        self.executable("sleep", "#!/bin/sh\nexit 0\n")
        self.executable("qemu-system-x86_64", """#!/usr/bin/env python3
import os
import sys
import time

mode = os.environ['FAKE_QEMU']
if mode == 'crash':
    sys.exit(7)
if mode == 'eof':
    sys.stdin.buffer.read()
    sys.exit(0)
received = bytearray()
while True:
    byte = sys.stdin.buffer.read(1)
    if not byte:
        break
    received.extend(byte)
    if received.endswith(b'\\x01x') and mode == 'quit':
        assert '-nographic' in sys.argv
        assert sys.argv[sys.argv.index('-serial') + 1] == 'mon:stdio'
        sys.exit(0)
# EOF is not a QEMU shutdown command, and a stuck VM ignores the quit too.
time.sleep(60)
""")

    def executable(self, name, content):
        path = self.root / name
        path.write_text(content)
        path.chmod(0o755)

    def run_harness(self, mode, script="none"):
        env = dict(os.environ, PATH=f"{self.root}:{os.environ['PATH']}",
                   FAKE_QEMU=mode, VARS_SRC=str(self.root / "pristine.fd"),
                   BOOT_WAIT="0", STEP="0", TIMEOUT="1", SHOTS="",
                   USB="0", BOOT_USB="0", KEEP_VARS="0", EXPECT="no-change", RES="none")
        return subprocess.run(["bash", str(HARNESS), str(self.root), script],
                              env=env, capture_output=True, text=True, timeout=10)

    def test_completed_walk_quits_without_waiting_for_watchdog(self):
        result = self.run_harness("quit")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("qemu exited with 0", result.stdout)
        self.assertIn("test disk untouched", result.stdout)

    def test_watchdog_is_a_failure_even_for_a_read_only_walk(self):
        result = self.run_harness("stuck")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("watchdog", result.stderr)
        self.assertNotIn("test disk untouched", result.stdout)

    def test_qemu_crash_is_a_failure(self):
        result = self.run_harness("crash")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("qemu exited abnormally (exit 7)", result.stderr)

    def test_successful_qemu_exit_cannot_hide_a_failed_input_script(self):
        result = self.run_harness("eof", "unknown-script")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("scripted input did not complete (exit 1)", result.stderr)


if __name__ == "__main__":
    unittest.main()
