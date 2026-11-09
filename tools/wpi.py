"""Minimal reader for waypost index files (format v1 and v2), for tools only.

Record layouts mirror crates/waypost-core/src/model.rs. The accuracy gate
reads the base branch's index with the head branch's tools, so keep the
layouts of older versions here.
"""
import json
import mmap
import struct

# version -> name -> (struct format, size). Only the fields tools read.
RECORDS = {
    1: {
        "pieces": ("<QII", 16),
        "nodes": ("<ii", 8),
        "addrs": ("<ii10I", 48),
        "places": ("<iiIB3x", 16),
    },
    2: {
        "pieces": ("<QII", 16),
        "nodes": ("<ii", 8),
        "addrs": ("<ii10IQB7x", 64),
        "places": ("<iiIB3xQ", 24),
    },
}


class Index:
    def __init__(self, path):
        self._f = open(path, "rb")
        self.buf = mmap.mmap(self._f.fileno(), 0, access=mmap.ACCESS_READ)
        if self.buf[:8] != b"WAYPOST\0":
            raise ValueError("not a waypost index")
        (self.version,) = struct.unpack_from("<I", self.buf, 8)
        if self.version not in RECORDS:
            raise ValueError(f"index format version {self.version}, tools read {sorted(RECORDS)}")
        self.records = RECORDS[self.version]
        meta_off, meta_len = struct.unpack_from("<QQ", self.buf, 16)
        self.meta = json.loads(self.buf[meta_off:meta_off + meta_len])
        self.sections = {s["name"]: s for s in self.meta["sections"]}
        for name, (fmt, size) in self.records.items():
            assert struct.calcsize(fmt) == size, name
            if self.sections[name]["elem_size"] != size:
                raise ValueError(f"{name}: record size {self.sections[name]['elem_size']}, expected {size}")

    def count(self, name):
        s = self.sections[name]
        return s["len"] // s["elem_size"]

    def record(self, name, i):
        fmt, size = self.records[name]
        return struct.unpack_from(fmt, self.buf, self.sections[name]["offset"] + i * size)
