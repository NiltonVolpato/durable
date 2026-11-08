#!/usr/bin/env python3
"""Decode a parked task's snapshot using only the driver's layout dump.

usage: decode_snapshot.py <layout.txt> <snap.bin> <root coroutine substring>
"""
import re
import struct
import sys


def parse_layout(path):
    cos, cur, var = {}, None, None
    for line in open(path):
        if m := re.match(r"coroutine (.*?) size=(\d+)", line):
            cur = cos[m[1]] = {"size": int(m[2]), "variants": {}}
        elif m := re.match(r"  tag: (\d+) B @ (\d+)", line):
            cur["tag"] = (int(m[1]), int(m[2]))
        elif m := re.match(r"  variant (\d+) \((.*)\)", line):
            var = cur["variants"][int(m[1])] = {"src": m[2], "fields": []}
        elif m := re.match(r"    (\S+): (.*) @ (\d+) \(local", line):
            var["fields"].append((m[1], m[2], int(m[3])))
    return cos


def value(ty, b, off, cos, depth):
    if ty in cos:
        return "\n" + frame(cos, ty, b, off, depth + 1)
    if ty == "u64":
        return struct.unpack_from("<Q", b, off)[0]
    if m := re.fullmatch(r"\[u64; (\d+)\]", ty):
        return list(struct.unpack_from(f"<{m[1]}Q", b, off))
    if m := re.fullmatch(r"\[u8; (\d+)\]", ty):
        return b[off:off + int(m[1])].hex()
    if ty == "durable_poc::Slow":
        return f"Slow {{ id: {struct.unpack_from('<Q', b, off)[0]} }}"
    return f"<{ty}>"


def frame(cos, name, b, base, depth=0):
    co = cos[name]
    size, toff = co["tag"]
    tag = int.from_bytes(b[base + toff:base + toff + size], "little")
    v = co["variants"][tag]
    pad = "  " * depth
    lines = [f"{pad}{name}: state {tag}, suspended at {v['src']}"]
    for fname, ty, off in v["fields"]:
        lines.append(f"{pad}  {fname} = {value(ty, b, base + off, cos, depth)}")
    return "\n".join(lines)


def main():
    cos = parse_layout(sys.argv[1])
    data = open(sys.argv[2], "rb").read()
    arena = data[32:]  # skip the snapshot file header
    root_off = struct.unpack_from("<I", arena, 28)[0]  # Header.root_off
    root = next(n for n in cos if sys.argv[3] in n)
    print(frame(cos, root, arena, root_off))


main()
