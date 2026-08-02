#!/usr/bin/env python3
"""Read UEFI variables out of an OVMF varstore image.

The QEMU harness gives each run a fresh copy of `OVMF_VARS_4M.fd`, which is
the firmware's whole NVRAM. This walks it from the host so a boot entry can
be looked at, or captured as a test fixture, without booting anything.

    tools/dump-efivars.py build/qemu/vars.fd                 # list
    tools/dump-efivars.py build/qemu/vars.fd Boot0000 > x.bin  # extract

The EDK II variable store has two header layouts — one with authentication
fields and one without — and which is present depends on how the firmware
was built. Rather than guess, this tries both and keeps whatever yields a
sane name and length. That is good enough for reading; it is not a general
varstore parser and does not pretend to be.
"""
import struct
import sys
import uuid

EFI_GLOBAL_VARIABLE = uuid.UUID("8be4df61-93ca-11d2-aa0d-00e098032b8c")

# The record marker, little-endian 0x55AA.
START_ID = b"\xaa\x55"
# All valid bits set: the record is live, rather than deleted or half-written.
VAR_ADDED = 0x3F

# StartId, State, Reserved, Attributes, NameSize, DataSize, VendorGuid
PLAIN = ("<HBBIII16s", 4, 5, 6)
# The same, with MonotonicCount, TimeStamp and PubKeyIndex in the middle.
AUTH = ("<HBBIQ16sIII16s", 7, 8, 9)


def records(blob, layout):
    fmt, name_i, data_i, guid_i = layout
    header_len = struct.calcsize(fmt)
    at = 0
    while True:
        at = blob.find(START_ID, at)
        if at < 0:
            return
        start, at = at, at + 2
        if start + header_len > len(blob):
            continue
        fields = struct.unpack_from(fmt, blob, start)
        name_size, data_size = fields[name_i], fields[data_i]
        if not (0 < name_size <= 512 and 0 <= data_size <= 65536):
            continue
        if start + header_len + name_size + data_size > len(blob):
            continue
        raw = blob[start + header_len : start + header_len + name_size]
        try:
            name = raw.decode("utf-16-le").rstrip("\0")
        except UnicodeDecodeError:
            continue
        if not name or not name.isprintable():
            continue
        data = blob[start + header_len + name_size :][:data_size]
        yield name, uuid.UUID(bytes_le=fields[guid_i]), fields[1], data


def read_store(path):
    blob = open(path, "rb").read()
    found = {}
    for layout in (PLAIN, AUTH):
        for name, vendor, state, data in records(blob, layout):
            if state == VAR_ADDED and vendor == EFI_GLOBAL_VARIABLE:
                found.setdefault(name, data)
    return found


def main(argv):
    if not 2 <= len(argv) <= 3:
        sys.exit(__doc__)
    found = read_store(argv[1])
    if len(argv) == 2:
        for name in sorted(found):
            print(f"{name:24} {len(found[name]):5} bytes")
        return
    want = argv[2]
    if want not in found:
        sys.exit(f"{want} is not in {argv[1]}")
    sys.stdout.buffer.write(found[want])


main(sys.argv)
