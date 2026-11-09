"""Minimal reader for waypost index files (format v1), for tools only.

Record layouts mirror crates/waypost-core/src/model.rs.
"""
import json
import mmap
import struct

RECORDS = {
    # name: (struct format, size)
    "pieces": ("<QII", 16),
    "nodes": ("<ii", 8),
    "addrs": ("<ii10I", 48),
    "places": ("<iiIB3x", 16),
}


class Index:
    def __init__(self, path):
        self._f = open(path, "rb")
        self.buf = mmap.mmap(self._f.fileno(), 0, access=mmap.ACCESS_READ)
        if self.buf[:8] != b"WAYPOST\0":
            raise ValueError("not a waypost index")
        meta_off, meta_len = struct.unpack_from("<QQ", self.buf, 16)
        self.meta = json.loads(self.buf[meta_off:meta_off + meta_len])
        self.sections = {s["name"]: s for s in self.meta["sections"]}

    def count(self, name):
        s = self.sections[name]
        return s["len"] // s["elem_size"]

    def record(self, name, i):
        fmt, size = RECORDS[name]
        return struct.unpack_from(fmt, self.buf, self.sections[name]["offset"] + i * size)
