"""Emit a minimal valid PNG (solid orange) for `pnpm exec tauri icon`."""
from pathlib import Path

# 512×512 RGBA orange — enough pixels for tauri-icon upscaling.
W = H = 512
row = b"\xff\x88\x00\xff" * W
raw = b"".join([b"\x00" + row for _ in range(H)])

import struct
import zlib


def chunk(tag: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + tag
        + data
        + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )


ihdr = struct.pack(">IIBBBBB", W, H, 8, 6, 0, 0, 0)
png = (
    b"\x89PNG\r\n\x1a\n"
    + chunk(b"IHDR", ihdr)
    + chunk(b"IDAT", zlib.compress(raw, 9))
    + chunk(b"IEND", b"")
)

out = Path(__file__).resolve().parent / "app-icon.png"
out.write_bytes(png)
print(out, out.stat().st_size)
