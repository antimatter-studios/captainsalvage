#!/usr/bin/env python3
"""Damage recipes for the FAT and exFAT oracle images (validation/fat-oracle.sh runs this).

    damage.py IMAGE RECIPE

changes the image in place and prints one line, `touched<TAB>names`: the root-folder files the
damage can make read wrong, comma separated, or `*` when it can reach any file. Everything else in
the image must still read exactly as before, which is what the Rust test checks.

The recipes work on the root folder of an image whose layout fat-oracle.sh made (files `fraga.bin`
and `fragb.bin`, grown one cluster at a time in turn so that each is split into pieces, and
`deepfile.bin`, several clusters long and in one piece). They fail when the image does not have the
shape a recipe needs, so that no recipe quietly damages nothing.

FAT12, FAT16 and FAT32 (a chain is in the FAT, and every copy of the FAT is changed unless the
recipe is about the copies differing):
  fat-zeroed          the first FAT copy zeroed, the second left whole
  chain-cut           the chain of fraga.bin ends after its first cluster
  chain-loop          the last cluster of deepfile.bin points back to its first
  chain-crosslinked   the first cluster of fragb.bin continues into the chain of fraga.bin
  differing-fats      the second cluster of deepfile.bin is free in the first copy only
exFAT (the FAT is used only by files that are not in one piece):
  chain-cut, chain-loop, chain-crosslinked   as above, on fraga.bin and fragb.bin
  entry-checksum      the checksum of the entry set of readme.txt is wrong
"""
import struct
import sys

END = 0x0FFFFFFF


def u16(data, at):
    return struct.unpack_from("<H", data, at)[0]


def u32(data, at):
    return struct.unpack_from("<I", data, at)[0]


def u64(data, at):
    return struct.unpack_from("<Q", data, at)[0]


def fail(message):
    sys.exit(f"damage.py: {message}")


class Fat:
    """FAT12, FAT16 or FAT32: the boot sector says where the tables and the root folder are."""

    def __init__(self, image):
        self.image = image
        image.seek(0)
        boot = image.read(512)
        self.sector = u16(boot, 11)
        self.per_cluster = boot[13]
        self.reserved = u16(boot, 14)
        self.copies = boot[16]
        root_entries = u16(boot, 17)
        total = u16(boot, 19) or u32(boot, 32)
        self.fat_sectors = u16(boot, 22) or u32(boot, 36)
        root_sectors = (root_entries * 32 + self.sector - 1) // self.sector
        self.root_start = (self.reserved + self.copies * self.fat_sectors) * self.sector
        self.root_bytes = root_sectors * self.sector
        self.data_start = self.root_start + self.root_bytes
        clusters = (total * self.sector - self.data_start) // (self.sector * self.per_cluster)
        self.kind = 12 if clusters < 4085 else 16 if clusters < 65525 else 32
        self.root_cluster = u32(boot, 44) if self.kind == 32 else None
        self.end = {12: 0xFFF, 16: 0xFFFF, 32: END}[self.kind]

    def fat_at(self, copy):
        return (self.reserved + copy * self.fat_sectors) * self.sector

    def get(self, cluster, copy=0):
        base = self.fat_at(copy)
        if self.kind == 12:
            self.image.seek(base + cluster * 3 // 2)
            word = u16(self.image.read(2), 0)
            return word & 0xFFF if cluster % 2 == 0 else word >> 4
        width = 2 if self.kind == 16 else 4
        self.image.seek(base + cluster * width)
        raw = self.image.read(width)
        return u16(raw, 0) if width == 2 else u32(raw, 0) & END

    def put(self, cluster, value, copies=None):
        for copy in range(self.copies) if copies is None else copies:
            base = self.fat_at(copy)
            if self.kind == 12:
                at = base + cluster * 3 // 2
                self.image.seek(at)
                word = u16(self.image.read(2), 0)
                if cluster % 2 == 0:
                    word = (word & 0xF000) | value
                else:
                    word = (word & 0x000F) | (value << 4)
                self.image.seek(at)
                self.image.write(struct.pack("<H", word))
            else:
                width = 2 if self.kind == 16 else 4
                self.image.seek(base + cluster * width)
                self.image.write(struct.pack("<H" if width == 2 else "<I", value))
        self.image.flush()

    def is_end(self, value):
        return value >= self.end - 7

    def chain(self, first):
        seen = []
        cluster = first
        while 2 <= cluster and cluster not in seen:
            seen.append(cluster)
            cluster = self.get(cluster)
            if self.is_end(cluster):
                break
        return seen

    def cluster_start(self, cluster):
        return self.data_start + (cluster - 2) * self.per_cluster * self.sector

    def root(self):
        if self.root_cluster is None:
            self.image.seek(self.root_start)
            return self.image.read(self.root_bytes)
        data = b""
        for cluster in self.chain(self.root_cluster):
            self.image.seek(self.cluster_start(cluster))
            data += self.image.read(self.per_cluster * self.sector)
        return data

    def find(self, short):
        """The first cluster of the root-folder file with this 8.3 name (`FRAGA   BIN`)."""
        data = self.root()
        for at in range(0, len(data), 32):
            entry = data[at:at + 32]
            if entry[0] == 0:
                break
            if entry[0] == 0xE5 or entry[11] & 0x0F == 0x0F or entry[11] & 0x08:
                continue
            if entry[:11] == short.encode("ascii"):
                return (u16(entry, 20) << 16 | u16(entry, 26)) if self.kind == 32 else u16(entry, 26)
        fail(f"{short.strip()} is not in the root folder")


def fat_recipe(image, recipe):
    fat = Fat(image)
    fraga = lambda: fat.chain(fat.find("FRAGA   BIN"))
    fragb = lambda: fat.chain(fat.find("FRAGB   BIN"))
    deep = lambda: fat.chain(fat.find("DEEPFILEBIN"))
    if recipe == "fat-zeroed":
        image.seek(fat.fat_at(0))
        image.write(bytes(fat.fat_sectors * fat.sector))
        return "*"
    if recipe == "chain-cut":
        chain = fraga()
        need(len(chain) >= 3, "fraga.bin has fewer than three clusters")
        fat.put(chain[0], fat.end)
        return "fraga.bin"
    if recipe == "chain-loop":
        chain = deep()
        need(len(chain) >= 3, "deepfile.bin has fewer than three clusters")
        fat.put(chain[-1], chain[0])
        return "deepfile.bin"
    if recipe == "chain-crosslinked":
        a, b = fraga(), fragb()
        need(len(a) >= 3 and len(b) >= 3, "fraga.bin and fragb.bin need three clusters each")
        fat.put(b[0], a[1])
        return "fraga.bin,fragb.bin"
    if recipe == "differing-fats":
        need(fat.copies >= 2, "the volume has one FAT copy")
        chain = deep()
        need(len(chain) >= 3, "deepfile.bin has fewer than three clusters")
        fat.put(chain[1], 0, copies=[0])
        return "deepfile.bin"
    fail(f"unknown FAT recipe {recipe}")


class Exfat:
    def __init__(self, image):
        self.image = image
        image.seek(0)
        boot = image.read(512)
        self.sector = 1 << boot[108]
        self.cluster = self.sector << boot[109]
        self.fat = u32(boot, 80) * self.sector
        self.fat_bytes = u32(boot, 84) * self.sector
        self.heap = u32(boot, 88) * self.sector
        self.root_cluster = u32(boot, 96)
        self.copies = boot[110]

    def get(self, cluster):
        self.image.seek(self.fat + cluster * 4)
        return u32(self.image.read(4), 0)

    def put(self, cluster, value):
        for copy in range(self.copies):
            self.image.seek(self.fat + copy * self.fat_bytes + cluster * 4)
            self.image.write(struct.pack("<I", value))
        self.image.flush()

    def chain(self, first):
        seen = []
        cluster = first
        while 2 <= cluster < 0xFFFFFFF7 and cluster not in seen:
            seen.append(cluster)
            cluster = self.get(cluster)
        return seen

    def start(self, cluster):
        return self.heap + (cluster - 2) * self.cluster

    def sets(self):
        """Every entry set of the root folder: name, flags, first cluster, length, file offset."""
        clusters = self.chain(self.root_cluster)
        data = b""
        for cluster in clusters:
            self.image.seek(self.start(cluster))
            data += self.image.read(self.cluster)
        found = {}
        at = 0
        while at < len(data) and data[at] != 0:
            if data[at] != 0x85:
                at += 32
                continue
            count = data[at + 1]
            stream = data[at + 32:at + 64]
            if stream[0] != 0xC0:
                fail("an entry set has no stream entry")
            units = b"".join(
                data[at + 64 + 32 * n + 2:at + 64 + 32 * n + 32] for n in range(count - 1)
            )
            name = units[:stream[3] * 2].decode("utf-16-le")
            offset = self.start(clusters[at // self.cluster]) + at % self.cluster
            found[name] = (stream[1], u32(stream, 20), u64(stream, 24), offset)
            at += 32 * (count + 1)
        return found

    def file_chain(self, name):
        flags, first, _, _ = self.sets().get(name) or fail(f"{name} is not in the root folder")
        need(flags & 2 == 0, f"{name} is in one piece, so it has no FAT chain to damage")
        return self.chain(first)


def exfat_recipe(image, recipe):
    volume = Exfat(image)
    if recipe == "chain-cut":
        chain = volume.file_chain("fraga.bin")
        need(len(chain) >= 3, "fraga.bin has fewer than three clusters")
        volume.put(chain[0], 0xFFFFFFFF)
        return "fraga.bin"
    if recipe == "chain-loop":
        chain = volume.file_chain("fragb.bin")
        need(len(chain) >= 3, "fragb.bin has fewer than three clusters")
        volume.put(chain[-1], chain[0])
        return "fragb.bin"
    if recipe == "chain-crosslinked":
        a, b = volume.file_chain("fraga.bin"), volume.file_chain("fragb.bin")
        need(len(a) >= 3 and len(b) >= 3, "fraga.bin and fragb.bin need three clusters each")
        volume.put(b[0], a[1])
        return "fraga.bin,fragb.bin"
    if recipe == "entry-checksum":
        _, _, _, offset = volume.sets().get("readme.txt") or fail("readme.txt is not in the root folder")
        image.seek(offset + 2)
        checksum = u16(image.read(2), 0)
        image.seek(offset + 2)
        image.write(struct.pack("<H", checksum ^ 0xFFFF))
        return "readme.txt"
    fail(f"unknown exFAT recipe {recipe}")


def need(condition, message):
    if not condition:
        fail(message)


def main():
    if len(sys.argv) != 3:
        fail("usage: damage.py IMAGE RECIPE")
    with open(sys.argv[1], "r+b") as image:
        image.seek(3)
        exfat = image.read(8) == b"EXFAT   "
        touched = (exfat_recipe if exfat else fat_recipe)(image, sys.argv[2])
        image.flush()
    print(f"touched\t{touched}")


if __name__ == "__main__":
    main()
