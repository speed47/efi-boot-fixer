#!/usr/bin/env python3
"""Rebuild a full-size sparse disk image from raw head/tail sector dumps.

    tools/reconstruct.py <dump-dir> <out.img> [--scrub]

`<dump-dir>` holds head.bin (LBA 0..33), tail.bin (the last 33 LBAs) and
sectors.txt, as produced by the dd recipe in docs/testing.md. The output is a
sparse file of the original size, so a 1 TB disk costs ~34 KB on disk and
the GPT sits at exactly the offsets it had on the real hardware.

--scrub replaces the disk GUID and every unique partition GUID with
deterministic placeholders and recomputes all four CRCs. Type GUIDs,
names and extents are preserved, because those are the parts that carry
the layout information worth testing against. The result is a fixture
that can be committed without publishing which physical drive it came
from.
"""

import os
import struct
import sys
import zlib

SECTOR = 512
HEAD_SECTORS = 34
TAIL_SECTORS = 33


def crc32(data: bytes) -> int:
    return zlib.crc32(data) & 0xFFFFFFFF


def placeholder_guid(tag: int) -> bytes:
    """A stable, obviously-fake GUID: 00000000-0000-4000-8000-0000000000NN."""
    return struct.pack("<IHH", 0, 0, 0x4000) + b"\x80\x00" + b"\x00" * 5 + bytes([tag])


def scrub(header: bytearray, entries: bytearray) -> None:
    n, size = struct.unpack_from("<II", header, 80)
    header[56:72] = placeholder_guid(0xDD)  # disk GUID
    for i in range(n):
        off = i * size
        if entries[off : off + 16] == b"\x00" * 16:
            continue
        entries[off + 16 : off + 32] = placeholder_guid(i + 1)  # unique GUID


def reseal(header: bytearray, entries: bytes) -> None:
    """Recompute the entry-array CRC and then the header CRC."""
    n, size = struct.unpack_from("<II", header, 80)
    struct.pack_into("<I", header, 88, crc32(entries[: n * size]))
    struct.pack_into("<I", header, 16, 0)
    hsize = struct.unpack_from("<I", header, 12)[0]
    struct.pack_into("<I", header, 16, crc32(bytes(header[:hsize])))


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    src, out = sys.argv[1], sys.argv[2]
    do_scrub = "--scrub" in sys.argv[3:]

    head = bytearray(open(os.path.join(src, "head.bin"), "rb").read())
    tail = bytearray(open(os.path.join(src, "tail.bin"), "rb").read())
    sectors = int(open(os.path.join(src, "sectors.txt")).read().strip())

    if len(head) != HEAD_SECTORS * SECTOR:
        raise SystemExit(f"head.bin is {len(head)} bytes, expected {HEAD_SECTORS * SECTOR}")
    if len(tail) != TAIL_SECTORS * SECTOR:
        raise SystemExit(f"tail.bin is {len(tail)} bytes, expected {TAIL_SECTORS * SECTOR}")

    if do_scrub:
        # Primary: header at LBA 1, entries at LBA 2..33.
        p_header = bytearray(head[SECTOR : 2 * SECTOR])
        p_entries = bytearray(head[2 * SECTOR :])
        scrub(p_header, p_entries)
        reseal(p_header, p_entries)
        head[SECTOR : 2 * SECTOR] = p_header
        head[2 * SECTOR :] = p_entries

        # Backup: entries first, header in the final LBA.
        b_entries = bytearray(tail[: 32 * SECTOR])
        b_header = bytearray(tail[32 * SECTOR :])
        scrub(b_header, b_entries)
        reseal(b_header, b_entries)
        tail[: 32 * SECTOR] = b_entries
        tail[32 * SECTOR :] = b_header

    with open(out, "wb") as f:
        f.truncate(sectors * SECTOR)
        f.seek(0)
        f.write(head)
        f.seek((sectors - TAIL_SECTORS) * SECTOR)
        f.write(tail)

    print(f"wrote {out}: {sectors} sectors ({sectors * SECTOR / 2**30:.1f} GiB){' scrubbed' if do_scrub else ''}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
