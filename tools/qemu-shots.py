#!/usr/bin/env python3
"""Photograph a running QEMU's screen at intervals, over QMP.

The graphical backend can only really be judged by looking at it, and the
hardware it is for is not on this desk. QEMU can be told to present a
portrait framebuffer of exactly the Steam Deck's shape, so the rotation, the
font and the layout can all be checked here — but only if something takes
the pictures. That is this.

Screendumps come out as PPM, which nothing views but everything converts.
Waits for the socket to appear, so it can be started alongside QEMU.
"""

import json
import os
import socket
import sys
import time


class QemuGone(Exception):
    """QEMU closed the QMP connection: the normal end of a run."""


def connect(path, deadline):
    """Wait for QEMU to create the socket, then complete the QMP handshake."""
    while time.time() < deadline:
        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.connect(path)
            return sock
        except (FileNotFoundError, ConnectionRefusedError):
            time.sleep(0.25)
    raise SystemExit(f"qemu-shots: no QMP socket at {path}")


class Qmp:
    def __init__(self, path, deadline):
        self.sock = connect(path, deadline)
        self.buf = b""
        self.read()  # the greeting
        self.command("qmp_capabilities")

    def read(self):
        while b"\n" not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise QemuGone
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def command(self, name, **arguments):
        request = {"execute": name}
        if arguments:
            request["arguments"] = arguments
        self.sock.sendall(json.dumps(request).encode() + b"\n")
        while True:
            reply = self.read()
            # Events arrive unsolicited and are not the answer to anything.
            if "event" not in reply:
                return reply


def main():
    if len(sys.argv) != 5:
        raise SystemExit("usage: qemu-shots.py <qmp.sock> <outdir> <every-s> <count>")
    sock_path, outdir, every, count = sys.argv[1:]
    every, count = float(every), int(count)

    os.makedirs(outdir, exist_ok=True)
    qmp = Qmp(sock_path, time.time() + 120)

    for shot in range(count):
        time.sleep(every)
        dest = os.path.abspath(os.path.join(outdir, f"shot-{shot:02d}.ppm"))
        reply = qmp.command("screendump", filename=dest)
        if "error" in reply:
            print(f"qemu-shots: {reply['error']}", file=sys.stderr)
            return
        print(f"qemu-shots: {dest}", file=sys.stderr)


if __name__ == "__main__":
    try:
        main()
    except (BrokenPipeError, ConnectionResetError, QemuGone):
        # QEMU exiting first is the normal end of a run, not a failure.
        # SystemExit deliberately propagates: swallowing it silenced this
        # script's own error messages (bad usage, no socket) too.
        pass
