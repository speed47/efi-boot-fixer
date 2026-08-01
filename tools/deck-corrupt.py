#!/usr/bin/env python3
"""Reproduce, on purpose, the GPT corruption seen on a Steam Deck.

This exists so the repair path in efigptfix can be tested against the real
failure rather than a synthetic one. The damage it inflicts is exactly what
was found on the affected disk: `PartitionEntryLBA` in the primary header
moved from 2 to `FirstUsableLBA - entry_array_blocks`, with the header CRC
recomputed so the header still verifies. Six bytes. Nothing else changes,
and no partition data is touched.

    sudo ./deck-corrupt.py inspect /dev/nvme0n1
    sudo ./deck-corrupt.py save    /dev/nvme0n1 -o /esp/EFIGPTFIX/gpt-before.bin
    sudo ./deck-corrupt.py break   /dev/nvme0n1 -o /esp/EFIGPTFIX/gpt-before.bin
    sudo ./deck-corrupt.py restore /dev/nvme0n1 -i /esp/EFIGPTFIX/gpt-before.bin
    ./deck-corrupt.py show /esp/EFIGPTFIX/gpt.001         # no root, no device

`break` refuses to run unless it has first written a snapshot it can read
back, and unless *both* tables and the protective MBR are healthy going in
-- there is no point corrupting a disk whose backup GPT could not repair it
afterwards.

The snapshot is written in efigptfix's own archive format, so it can be
restored three ways: with `restore` here, with the EFI application's
"Restore GPTs from a saved backup", or by hand from the sector dump inside
it. Put it somewhere you can still reach when the machine will not boot --
the ESP is the obvious choice, which is where efigptfix looks:

    /esp/EFIGPTFIX/         (SteamOS mounts the ESP at /esp)

After `break`, reboot to see the failure. The running kernel already has
the partition table in memory, so nothing goes wrong until then.
"""

import argparse
import os
import stat
import struct
import sys
import zlib

SECTOR = 512
GPT_SIG = b"EFI PART"

# efigptfix archive format. Must match crates/gptcore/src/backup.rs.
MAGIC = b"EFIGPTBK"
VERSION = 2          # 2 added the metadata section; 1 is still readable
MIN_VERSION = 1
FIXED_LEN = 52
ROLE_MBR, ROLE_PRI_ENTRIES, ROLE_PRI_HEADER, ROLE_BAK_ENTRIES, ROLE_BAK_HEADER = 1, 2, 3, 4, 5
HEALTH_HEALTHY, HEALTH_MBR_ONLY = 0, 1

# Header field offsets, UEFI spec table 5.5.
OFF_HEADER_SIZE = 12
OFF_HEADER_CRC = 16
OFF_MY_LBA = 24
OFF_ALTERNATE_LBA = 32
OFF_FIRST_USABLE = 40
OFF_LAST_USABLE = 48
OFF_DISK_GUID = 56
OFF_ENTRY_LBA = 72
OFF_NUM_ENTRIES = 80
OFF_ENTRY_SIZE = 84
OFF_ENTRY_CRC = 88


class Fatal(Exception):
    pass


def crc32(data):
    return zlib.crc32(data) & 0xFFFFFFFF


def guid_str(raw):
    d1, d2, d3 = struct.unpack_from("<IHH", raw, 0)
    return "%08X-%04X-%04X-%02X%02X-%s" % (
        d1, d2, d3, raw[8], raw[9], raw[10:16].hex().upper()
    )


# --------------------------------------------------------------- the disk


class Disk:
    def __init__(self, path, write=False):
        self.path = path
        flags = os.O_RDWR if write else os.O_RDONLY
        try:
            self.fd = os.open(path, flags)
        except OSError as e:
            raise Fatal("cannot open %s: %s" % (path, e.strerror))

        st = os.fstat(self.fd)
        self.is_block = stat.S_ISBLK(st.st_mode)
        if self.is_block:
            name = os.path.basename(os.path.realpath(path))
            if os.path.exists("/sys/class/block/%s/partition" % name):
                raise Fatal(
                    "%s is a partition, not a whole disk -- pass the disk itself, "
                    "e.g. /dev/nvme0n1 rather than /dev/nvme0n1p3" % path
                )
        elif stat.S_ISREG(st.st_mode):
            # Disk images are accepted so the whole thing can be rehearsed
            # against a copy before it is pointed at real hardware.
            print("note: %s is a regular file, not a block device" % path)
        else:
            raise Fatal("%s is neither a block device nor a disk image" % path)

        self.size = os.lseek(self.fd, 0, os.SEEK_END)
        if self.size % SECTOR:
            raise Fatal("%s is not a whole number of 512-byte sectors" % path)
        self.last_block = self.size // SECTOR - 1

    def read(self, lba, count=1):
        os.lseek(self.fd, lba * SECTOR, os.SEEK_SET)
        data = os.read(self.fd, count * SECTOR)
        if len(data) != count * SECTOR:
            raise Fatal("short read of %d sectors at LBA %d" % (count, lba))
        return data

    def write(self, lba, data):
        if len(data) % SECTOR:
            raise Fatal("refusing to write %d bytes, not a sector multiple" % len(data))
        os.lseek(self.fd, lba * SECTOR, os.SEEK_SET)
        n = os.write(self.fd, data)
        if n != len(data):
            raise Fatal("short write at LBA %d" % lba)

    def sync(self):
        os.fsync(self.fd)

    def close(self):
        os.close(self.fd)


class Header:
    """A GPT header, parsed and checked against the block it came from."""

    def __init__(self, block, read_from, last_block):
        self.raw = block
        self.read_from = read_from
        self.problems = []

        if block[:8] != GPT_SIG:
            self.problems.append("signature is not 'EFI PART'")
            self.valid_shape = False
            return
        self.valid_shape = True

        self.header_size = struct.unpack_from("<I", block, OFF_HEADER_SIZE)[0]
        self.stored_crc = struct.unpack_from("<I", block, OFF_HEADER_CRC)[0]
        self.my_lba = struct.unpack_from("<Q", block, OFF_MY_LBA)[0]
        self.alternate_lba = struct.unpack_from("<Q", block, OFF_ALTERNATE_LBA)[0]
        self.first_usable = struct.unpack_from("<Q", block, OFF_FIRST_USABLE)[0]
        self.last_usable = struct.unpack_from("<Q", block, OFF_LAST_USABLE)[0]
        self.disk_guid = block[OFF_DISK_GUID:OFF_DISK_GUID + 16]
        self.entry_lba = struct.unpack_from("<Q", block, OFF_ENTRY_LBA)[0]
        self.num_entries = struct.unpack_from("<I", block, OFF_NUM_ENTRIES)[0]
        self.entry_size = struct.unpack_from("<I", block, OFF_ENTRY_SIZE)[0]
        self.entry_crc = struct.unpack_from("<I", block, OFF_ENTRY_CRC)[0]

        if not (92 <= self.header_size <= SECTOR):
            self.problems.append("HeaderSize is %d" % self.header_size)
        if self.computed_crc() != self.stored_crc:
            self.problems.append(
                "header CRC is %#010x, recomputes to %#010x"
                % (self.stored_crc, self.computed_crc())
            )
        if self.my_lba != read_from:
            self.problems.append(
                "MyLBA says %d but this header is at LBA %d" % (self.my_lba, read_from)
            )
        if not (1 <= self.num_entries <= 8192):
            self.problems.append("NumberOfPartitionEntries is %d" % self.num_entries)
        if self.entry_size < 128 or self.entry_size % 8:
            self.problems.append("SizeOfPartitionEntry is %d" % self.entry_size)
        if not (0 < self.first_usable <= self.last_usable <= last_block):
            self.problems.append(
                "usable range %d..%d is not sane" % (self.first_usable, self.last_usable)
            )

    def computed_crc(self):
        size = max(92, min(self.header_size, SECTOR))
        block = bytearray(self.raw[:size])
        struct.pack_into("<I", block, OFF_HEADER_CRC, 0)
        return crc32(bytes(block))

    @property
    def array_bytes(self):
        return self.num_entries * self.entry_size

    @property
    def array_blocks(self):
        return (self.array_bytes + SECTOR - 1) // SECTOR

    def check_array(self, disk):
        """Read the entry array this header points at and verify its CRC."""
        try:
            raw = disk.read(self.entry_lba, self.array_blocks)
        except Fatal as e:
            self.problems.append("entry array unreadable: %s" % e)
            return None
        found = crc32(raw[: self.array_bytes])
        if found != self.entry_crc:
            self.problems.append(
                "entry array CRC is %#010x, recomputes to %#010x" % (self.entry_crc, found)
            )
        return raw

    @property
    def ok(self):
        return self.valid_shape and not self.problems


def mbr_health(block, last_block):
    """(is_protective, description). Mirrors gptcore::mbr::inspect."""
    if struct.unpack_from("<H", block, 510)[0] != 0xAA55:
        return False, "no boot signature"
    records = [block[446 + i * 16 : 446 + (i + 1) * 16] for i in range(4)]
    protective = [i for i, r in enumerate(records) if r[4] == 0xEE]
    if not protective:
        return False, "no protective (0xEE) record"
    idx = protective[0]
    if any(i != idx and any(r) for i, r in enumerate(records)):
        return False, "HYBRID MBR -- efigptfix refuses to touch this disk"
    start = struct.unpack_from("<I", records[idx], 8)[0]
    size = struct.unpack_from("<I", records[idx], 12)[0]
    expected = min(last_block, 0xFFFFFFFF)
    if start != 1 or size != expected:
        return False, "SizeInLBA is %d, expected %d" % (size, expected)
    return True, "OK"


# ------------------------------------------------------------- the archive


def sysfs(disk_path, attr):
    """A /sys/block/<disk>/device/<attr> value, or None.

    Linux can name the drive where UEFI cannot: NVMe DiskInfo returns
    namespace data with no model string, so this is the one chance to
    record what the hardware actually is.
    """
    name = os.path.basename(os.path.realpath(disk_path))
    try:
        with open("/sys/block/%s/device/%s" % (name, attr)) as f:
            return f.read().strip() or None
    except OSError:
        return None


def metadata(disk):
    meta = [("tool", "deck-corrupt.py %d" % VERSION), ("device", disk.path)]
    if disk.is_block:
        for key, attr in (("model", "model"), ("serial", "serial"), ("firmware", "firmware_rev")):
            value = sysfs(disk.path, attr)
            if value:
                meta.append((key, value))
    meta.append(("capacity", "%d blocks x %d B" % (disk.last_block + 1, SECTOR)))
    try:
        u = os.uname()
        meta.append(("host", "%s %s %s" % (u.nodename, u.sysname, u.release)))
    except OSError:
        pass
    return meta


def encode_meta(meta):
    out = bytearray()
    for k, v in meta:
        if "\t" in k or "\n" in k or "\n" in v:
            continue
        out += k.encode("utf-8") + b"\t" + v.encode("utf-8") + b"\n"
    return bytes(out)


def decode_meta(raw):
    out = []
    for line in raw.decode("utf-8", "replace").splitlines():
        if "\t" in line:
            k, v = line.split("\t", 1)
            out.append((k, v))
    return out


def build_archive(disk, primary, backup, health):
    chunks = [
        (ROLE_MBR, 0, disk.read(0, 1)),
        (ROLE_PRI_ENTRIES, primary.entry_lba, disk.read(primary.entry_lba, primary.array_blocks)),
        (ROLE_PRI_HEADER, 1, primary.raw),
        (ROLE_BAK_ENTRIES, backup.entry_lba, disk.read(backup.entry_lba, backup.array_blocks)),
        (ROLE_BAK_HEADER, disk.last_block, backup.raw),
    ]

    import time

    t = time.localtime()
    out = bytearray()
    out += MAGIC
    out += struct.pack("<I", VERSION)
    out += struct.pack("<I", SECTOR)
    out += struct.pack("<Q", disk.last_block)
    out += primary.disk_guid
    out += struct.pack("<H", t.tm_year)
    out += bytes([t.tm_mon, t.tm_mday, t.tm_hour, t.tm_min, t.tm_sec, health])
    out += struct.pack("<I", len(chunks))
    assert len(out) == FIXED_LEN, len(out)

    for role, lba, data in chunks:
        out += struct.pack("<IQQQ", role, lba, len(data) // SECTOR, len(data))
        out += data

    meta = encode_meta(metadata(disk))
    out += struct.pack("<I", len(meta)) + meta

    out += struct.pack("<I", crc32(bytes(out)))
    return bytes(out)


def parse_archive(blob):
    if len(blob) < FIXED_LEN + 4:
        raise Fatal("file is too short to be a GPT snapshot")
    if blob[:8] != MAGIC:
        raise Fatal("not an efigptfix GPT snapshot (bad magic)")
    version = struct.unpack_from("<I", blob, 8)[0]
    if not MIN_VERSION <= version <= VERSION:
        raise Fatal("snapshot format version %d is not supported" % version)

    body, stored = blob[:-4], struct.unpack_from("<I", blob, len(blob) - 4)[0]
    if crc32(body) != stored:
        raise Fatal(
            "snapshot checksum %#010x does not match %#010x -- the file is damaged"
            % (stored, crc32(body))
        )

    block_size = struct.unpack_from("<I", blob, 12)[0]
    if block_size != SECTOR:
        raise Fatal("snapshot is from a %d-byte-sector disk" % block_size)
    info = {
        "block_size": block_size,
        "last_block": struct.unpack_from("<Q", blob, 16)[0],
        "disk_guid": blob[24:40],
        "time": "%04d-%02d-%02d %02d:%02d:%02d"
        % (struct.unpack_from("<H", blob, 40)[0], blob[42], blob[43], blob[44], blob[45], blob[46]),
        "health": blob[47],
    }

    count = struct.unpack_from("<I", blob, 48)[0]
    chunks, off = [], FIXED_LEN
    for _ in range(count):
        if off + 28 > len(body):
            raise Fatal("snapshot is truncated")
        role, lba, _blocks, length = struct.unpack_from("<IQQQ", body, off)
        off += 28
        if off + length > len(body):
            raise Fatal("snapshot is truncated")
        chunks.append((role, lba, body[off : off + length]))
        off += length
    info["chunks"] = chunks

    info["meta"] = []
    info["version"] = version
    if version >= 2:
        if off + 4 > len(body):
            raise Fatal("snapshot is truncated")
        length = struct.unpack_from("<I", body, off)[0]
        off += 4
        if off + length > len(body):
            raise Fatal("snapshot is truncated")
        info["meta"] = decode_meta(body[off : off + length])
    return info


# ------------------------------------------------------------- operations


HEALTH_NAMES = {
    0: "healthy",
    1: "tables healthy, protective MBR wrong",
    2: "PRIMARY WAS CORRUPT",
    3: "BACKUP WAS CORRUPT",
    4: "BOTH TABLES WERE CORRUPT",
    5: "unusual (hybrid MBR or similar)",
}

ROLE_NAMES = {
    ROLE_MBR: "protective MBR",
    ROLE_PRI_ENTRIES: "primary partition entry array",
    ROLE_PRI_HEADER: "primary GPT header",
    ROLE_BAK_ENTRIES: "backup partition entry array",
    ROLE_BAK_HEADER: "backup GPT header",
}


def survey(disk):
    """Read and check everything. Returns (primary, backup, mbr_ok, mbr_text)."""
    primary = Header(disk.read(1), 1, disk.last_block)
    backup = Header(disk.read(disk.last_block), disk.last_block, disk.last_block)
    for h in (primary, backup):
        if h.valid_shape:
            h.check_array(disk)
    mbr_ok, mbr_text = mbr_health(disk.read(0), disk.last_block)
    return primary, backup, mbr_ok, mbr_text


def print_survey(disk, primary, backup, mbr_ok, mbr_text):
    print("Device:       %s" % disk.path)
    print("Sectors:      %d x %d B = %.1f GiB" % (disk.last_block + 1, SECTOR, disk.size / 2**30))
    print("Protective MBR: %s" % mbr_text)
    for name, h in (("Primary GPT", primary), ("Backup GPT", backup)):
        if h.ok:
            print("%-14s: OK" % name)
        else:
            print("%-14s: PROBLEMS" % name)
            for p in h.problems:
                print("    - %s" % p)
    if primary.valid_shape:
        print()
        print("Disk GUID:        %s" % guid_str(primary.disk_guid))
        print("FirstUsableLBA:   %d" % primary.first_usable)
        print("PartitionEntryLBA: %d" % primary.entry_lba)
        print(
            "Entry array:      %d entries x %d B = %d sectors"
            % (primary.num_entries, primary.entry_size, primary.array_blocks)
        )


def target_entry_lba(primary):
    """The value the real corruption produced: FirstUsableLBA - array size."""
    return primary.first_usable - primary.array_blocks


def cmd_inspect(args):
    disk = Disk(args.device)
    try:
        primary, backup, mbr_ok, mbr_text = survey(disk)
        print_survey(disk, primary, backup, mbr_ok, mbr_text)
        if primary.valid_shape:
            print()
            proposed = target_entry_lba(primary)
            if primary.entry_lba != 2:
                print("This disk is ALREADY corrupted in the way 'break' would corrupt it.")
                print("Use 'restore', or efigptfix's repair, to fix it.")
            elif proposed == 2:
                print("There is no gap between the entry array and the first usable")
                print("block, so the corruption being reproduced is not possible here:")
                print("the faulty arithmetic would produce the correct answer, 2.")
            else:
                print("'break' would set PartitionEntryLBA to %d." % proposed)
    finally:
        disk.close()
    return 0


def snapshot(disk, path, allow_same_disk):
    """Write a verified snapshot, or raise. Nothing else touches the disk
    until this has succeeded and been read back."""
    primary, backup, mbr_ok, mbr_text = survey(disk)
    if not primary.ok or not backup.ok:
        raise Fatal(
            "refusing to snapshot a disk whose tables do not verify -- "
            "run 'inspect' to see what is wrong"
        )

    directory = os.path.dirname(os.path.abspath(path)) or "."
    if not os.path.isdir(directory):
        raise Fatal("%s does not exist" % directory)
    if not allow_same_disk and same_disk(directory, disk.path):
        raise Fatal(
            "%s is on %s itself.\n"
            "A snapshot stored on the disk you are about to break is not much of a\n"
            "safety net. Write it to another device, or pass --allow-same-disk if\n"
            "you have a copy elsewhere. The ESP is a reasonable choice *only*\n"
            "because efigptfix can read it back without booting an OS."
            % (directory, disk.path)
        )

    health = HEALTH_HEALTHY if mbr_ok else HEALTH_MBR_ONLY
    blob = build_archive(disk, primary, backup, health)

    with open(path, "wb") as f:
        f.write(blob)
        f.flush()
        os.fsync(f.fileno())

    # Read it back through the parser. A snapshot that cannot be parsed is
    # not a snapshot, and finding that out now costs nothing.
    with open(path, "rb") as f:
        check = parse_archive(f.read())
    if check["last_block"] != disk.last_block:
        raise Fatal("snapshot read back with the wrong geometry")

    print("Snapshot written: %s (%d bytes)" % (path, len(blob)))
    for role, lba, data in check["chunks"]:
        print("    LBA %-12d %s (%d sectors)" % (lba, ROLE_NAMES.get(role, role), len(data) // SECTOR))
    if not mbr_ok:
        print("    note: protective MBR was not correct (%s) and is saved as found" % mbr_text)
    return primary, backup


def same_disk(directory, device):
    """True if `directory` lives on a partition of `device`."""
    try:
        dev = os.stat(directory).st_dev
        major, minor = os.major(dev), os.minor(dev)
        holder = os.path.realpath("/sys/dev/block/%d:%d" % (major, minor))
        disk_name = os.path.basename(os.path.realpath(device))
        # /sys/dev/block/259:3 -> .../nvme0n1/nvme0n1p3
        return ("/%s/" % disk_name) in holder + "/"
    except OSError:
        return False


def cmd_save(args):
    disk = Disk(args.device)
    try:
        snapshot(disk, args.output, args.allow_same_disk)
    finally:
        disk.close()
    return 0


def cmd_break(args):
    disk = Disk(args.device, write=not args.dry_run)
    try:
        primary, backup, mbr_ok, mbr_text = survey(disk)
        print_survey(disk, primary, backup, mbr_ok, mbr_text)
        print()

        if not primary.ok:
            raise Fatal("the primary GPT is already not healthy; nothing to reproduce here")
        if not backup.ok:
            raise Fatal(
                "the BACKUP GPT does not verify.\n"
                "Corrupting the primary would leave nothing to repair from. Refusing."
            )
        if not mbr_ok:
            raise Fatal("the protective MBR is not right (%s). Refusing." % mbr_text)
        if primary.entry_lba != 2:
            raise Fatal("PartitionEntryLBA is already %d, not 2" % primary.entry_lba)

        proposed = target_entry_lba(primary)
        if proposed == 2:
            raise Fatal(
                "FirstUsableLBA is %d and the entry array is %d sectors, so the\n"
                "arithmetic that caused the real corruption gives 2 -- the correct\n"
                "answer. This disk cannot exhibit that failure."
                % (primary.first_usable, primary.array_blocks)
            )

        primary_snapshot, _ = snapshot(disk, args.output, args.allow_same_disk)
        print()

        block = bytearray(primary.raw)
        struct.pack_into("<Q", block, OFF_ENTRY_LBA, proposed)
        struct.pack_into("<I", block, OFF_HEADER_CRC, 0)
        size = max(92, min(primary.header_size, SECTOR))
        struct.pack_into("<I", block, OFF_HEADER_CRC, crc32(bytes(block[:size])))

        changed = [i for i in range(SECTOR) if block[i] != primary.raw[i]]
        print("About to change %d bytes in LBA 1 of %s:" % (len(changed), disk.path))
        print("    PartitionEntryLBA  %d -> %d   (offset 72)" % (primary.entry_lba, proposed))
        print(
            "    HeaderCRC32        %#010x -> %#010x   (offset 16)"
            % (primary.stored_crc, struct.unpack_from("<I", block, OFF_HEADER_CRC)[0])
        )
        print()
        print("The header will still verify. The entry array is untouched at LBA 2,")
        print("and the backup GPT is untouched, so efigptfix can repair this.")
        print("No partition or filesystem is modified.")
        print()

        if args.dry_run:
            print("--dry-run: nothing was written.")
            return 0

        if not args.yes:
            print('Type exactly:  break %s' % disk.path)
            try:
                answer = input("> ").strip()
            except (EOFError, KeyboardInterrupt):
                print()
                raise Fatal("aborted")
            if answer != "break %s" % disk.path:
                raise Fatal("aborted: confirmation did not match")

        disk.write(1, bytes(block))
        disk.sync()
        print()
        print("Done. LBA 1 now points its entry array at %d." % proposed)
        print("Nothing breaks until you reboot: the running kernel already has")
        print("the partition table in memory.")
        print()
        print("To undo without rebooting:")
        print("    sudo %s restore %s -i %s" % (sys.argv[0], disk.path, args.output))
    finally:
        disk.close()
    return 0


def cmd_show(args):
    """Everything inside a snapshot, for reading one years later."""
    with open(args.file, "rb") as f:
        archive = parse_archive(f.read())

    print("File:         %s" % args.file)
    print("Format:       version %d" % archive["version"])
    print("Taken:        %s" % archive["time"])
    print("State then:   %s" % HEALTH_NAMES.get(archive["health"], "unknown"))
    print("Disk GUID:    %s" % guid_str(archive["disk_guid"]))
    print(
        "Geometry:     %d blocks x %d B = %.1f GiB"
        % (archive["last_block"] + 1, archive["block_size"],
           (archive["last_block"] + 1) * archive["block_size"] / 2**30)
    )

    print()
    print("Recorded when it was written:")
    if archive["meta"]:
        for k, v in archive["meta"]:
            print("    %-13s %s" % (k, v))
    else:
        print("    nothing: format version %d predates provenance" % archive["version"])

    by_role = {role: (lba, data) for role, lba, data in archive["chunks"]}
    header = by_role.get(ROLE_PRI_HEADER, by_role.get(ROLE_BAK_HEADER))
    array = by_role.get(ROLE_PRI_ENTRIES, by_role.get(ROLE_BAK_ENTRIES))
    if header and array:
        block = header[1]
        count = struct.unpack_from("<I", block, OFF_NUM_ENTRIES)[0]
        stride = struct.unpack_from("<I", block, OFF_ENTRY_SIZE)[0]
        print()
        print("Usable range: %d..%d" % (
            struct.unpack_from("<Q", block, OFF_FIRST_USABLE)[0],
            struct.unpack_from("<Q", block, OFF_LAST_USABLE)[0],
        ))
        print("Entry array:  %d entries x %d B at LBA %d" % (
            count, stride, struct.unpack_from("<Q", block, OFF_ENTRY_LBA)[0]))
        print()
        print("Partitions:")
        print("    %2s %14s %14s  %-20s %s" % ("#", "Start", "End", "Name", "Unique GUID"))
        data = array[1]
        for i in range(min(count, len(data) // stride)):
            e = data[i * stride : (i + 1) * stride]
            if e[:16] == b"\x00" * 16:
                continue
            start, end = struct.unpack_from("<QQ", e, 32)
            name = e[56:128].decode("utf-16-le", "replace").split("\x00")[0]
            print("    %2d %14d %14d  %-20s %s" % (i + 1, start, end, name, guid_str(e[16:32])))

    print()
    print("Sectors stored:")
    for role, lba, data in archive["chunks"]:
        print("    LBA %-12d %s (%d sectors)" % (lba, ROLE_NAMES.get(role, role), len(data) // SECTOR))
    return 0


def cmd_restore(args):
    with open(args.input, "rb") as f:
        archive = parse_archive(f.read())

    disk = Disk(args.device, write=not args.dry_run)
    try:
        if archive["last_block"] != disk.last_block:
            raise Fatal(
                "snapshot is from a disk of %d sectors, this one has %d"
                % (archive["last_block"] + 1, disk.last_block + 1)
            )

        print("Snapshot taken %s, disk GUID %s" % (archive["time"], guid_str(archive["disk_guid"])))
        if archive["health"] not in (HEALTH_HEALTHY, HEALTH_MBR_ONLY):
            print("WARNING: this snapshot was taken from a table that was already damaged.")

        by_role = {role: (lba, data) for role, lba, data in archive["chunks"]}
        for role in (ROLE_PRI_HEADER, ROLE_BAK_HEADER):
            if role not in by_role:
                raise Fatal("snapshot does not contain both GPT headers")

        # Same ordering rule the tool uses: an entry array must be on the
        # medium before any header claims it is there with a given CRC.
        order = [ROLE_PRI_ENTRIES, ROLE_BAK_ENTRIES, None, ROLE_MBR, ROLE_PRI_HEADER, ROLE_BAK_HEADER]
        for role in order:
            if role is None:
                if not args.dry_run:
                    disk.sync()
                print("    flush")
                continue
            if role not in by_role:
                continue
            lba, data = by_role[role]
            if lba + len(data) // SECTOR > disk.last_block + 1:
                raise Fatal("a section of the snapshot falls outside this disk")
            print("    LBA %-12d %s (%d sectors)" % (lba, ROLE_NAMES[role], len(data) // SECTOR))
            if not args.dry_run:
                disk.write(lba, data)
        if not args.dry_run:
            disk.sync()
        print("    flush")

        if args.dry_run:
            print("--dry-run: nothing was written.")
            return 0

        print()
        primary, backup, mbr_ok, mbr_text = survey(disk)
        print_survey(disk, primary, backup, mbr_ok, mbr_text)
        if primary.ok and backup.ok:
            print()
            print("Restored.")
    finally:
        disk.close()
    return 0


def main():
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n")[0],
        epilog="Run 'inspect' first. It never writes anything.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("inspect", help="report the state of both GPTs; writes nothing")
    p.add_argument("device")
    p.set_defaults(func=cmd_inspect)

    p = sub.add_parser("save", help="write a snapshot of both GPTs; writes nothing to the disk")
    p.add_argument("device")
    p.add_argument("-o", "--output", required=True, help="snapshot file to create")
    p.add_argument("--allow-same-disk", action="store_true")
    p.set_defaults(func=cmd_save)

    p = sub.add_parser("break", help="snapshot, then reproduce the corruption")
    p.add_argument("device")
    p.add_argument("-o", "--output", required=True, help="snapshot file to create first")
    p.add_argument("--allow-same-disk", action="store_true")
    p.add_argument("--dry-run", action="store_true", help="show the change, write nothing")
    p.add_argument("--yes", action="store_true", help="skip the typed confirmation")
    p.set_defaults(func=cmd_break)

    p = sub.add_parser("show", help="print everything inside a snapshot file")
    p.add_argument("file")
    p.set_defaults(func=cmd_show)

    p = sub.add_parser("restore", help="write a snapshot back")
    p.add_argument("device")
    p.add_argument("-i", "--input", required=True, help="snapshot file to restore")
    p.add_argument("--dry-run", action="store_true", help="show the writes, perform none")
    p.set_defaults(func=cmd_restore)

    args = parser.parse_args()
    try:
        return args.func(args)
    except Fatal as e:
        print("error: %s" % e, file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
