#!/usr/bin/env python3
"""Exercise backup discovery across launch, fixed and read-only volumes."""

import argparse
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys


HERE = Path(__file__).resolve().parent


def run(*args, **kwargs):
    return subprocess.run([str(a) for a in args], check=True, **kwargs)


def gpt_bytes(path):
    with path.open("rb") as disk:
        first = disk.read(34 * 512)
        disk.seek(-33 * 512, 2)
        return first + disk.read()


def exercise(case, root, efi):
    directory = root / case
    directory.mkdir(parents=True, exist_ok=True)
    # Re-running a case must not inherit files outside the rebuilt images.
    snapshot = directory / "gpt-007.bkp"
    snapshot.unlink(missing_ok=True)
    run(HERE / "mkimages.sh", directory, efi, "none", stdout=subprocess.DEVNULL)
    disk = directory / "test.img"
    before = gpt_bytes(disk)
    run(sys.executable, HERE / "deck-corrupt.py", "save", disk,
        "-o", snapshot, "--allow-same-disk", stdout=subprocess.DEVNULL)

    internal = f"{directory / 'boot.img'}@@{2048 * 512}"
    usb = str(directory / "usb.img")
    source = usb if case == "readonly" else internal
    run("mmd", "-i", source, "::/BOOTFIXR")
    run("mcopy", "-i", source, snapshot, "::/BOOTFIXR/gpt-007.bkp")
    if case == "rescue":
        run("mmd", "-i", usb, "::/EFI", "::/EFI/BOOT")
        run("mcopy", "-i", usb, efi, "::/EFI/BOOT/BOOTX64.EFI")
    if case == "source-error":
        # A filesystem that can be opened but whose backup directory cannot
        # be listed must not hide the good source on the internal ESP.
        run("mcopy", "-i", usb, snapshot, "::/BOOTFIXR")

    # OVMF repairs damaged main GPTs before the app runs, but not a bad PMBR.
    with disk.open("r+b") as target:
        target.seek(458)
        target.write((12345).to_bytes(4, "little"))
    (directory / "corruption").write_text("bad-mbr\n")
    usb_before = hashlib.sha256(Path(usb).read_bytes()).digest()

    env = dict(os.environ, USB="1", RES="none", EXPECT="change",
               BOOT_USB="1" if case == "rescue" else "0",
               USB_READONLY="on" if case == "readonly" else "off")
    script = "restore-source-error" if case == "source-error" else "restore"
    log = directory / "sources.log"
    print(f"Running {case}; log: {log}", flush=True)
    with log.open("w") as output:
        run(HERE / "run-qemu.sh", directory, script, env=env,
            stdout=output, stderr=subprocess.STDOUT)
    text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", log.read_text(errors="replace"))
    assert "1 snapshot(s) in" in text, "source omitted or launch volume duplicated"
    source_name = ("RESCUE" if case == "readonly" else
                   "BOOTFIXR" if case == "rescue" else
                   "the volume this program was launched from")
    assert f"found on {source_name}" in text, "incorrect source attribution"
    if case == "source-error":
        assert "Some files could not be read" in text, "unreadable source was hidden"
    assert gpt_bytes(disk) == before, "restore did not recover the original GPT and PMBR"

    # The automatic snapshot must number past the file on the other source,
    # including when it lives on read-only media.
    launch = usb if case == "rescue" else internal
    saved = run("mdir", "-i", launch, "::/BOOTFIXR", capture_output=True, text=True).stdout
    assert "gpt-008" in saved.lower(), "numbering ignored the discovered backup"
    if case == "readonly":
        assert hashlib.sha256(Path(usb).read_bytes()).digest() == usb_before
    print(f"{case}: discovery, attribution, numbering and restore passed", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path, help="scratch image directory (rebuilt per case)")
    parser.add_argument("efi", type=Path)
    parser.add_argument("--case", choices=["rescue", "readonly", "launch", "source-error"])
    args = parser.parse_args()
    for case in [args.case] if args.case else ["rescue", "readonly", "launch", "source-error"]:
        exercise(case, args.directory.resolve(), args.efi.resolve())


if __name__ == "__main__":
    main()
