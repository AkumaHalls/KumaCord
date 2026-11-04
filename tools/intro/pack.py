"""Packs the launch cinematic frames into app/assets/intro/intro.pack.

Layout (little endian):
  b"AKUI1" | u16 width | u16 height | u16 fps | u32 count | u32 count * frame len | blobs

Usage: python tools/intro/pack.py <frames dir> <out.pack> [width] [height] [fps]

The app streams the frames from the pack (it reads the table once and each
image on demand), so the whole movie can be shipped without living inside the
exe. `tools/intro/build.ps1` runs ffmpeg and this script end to end.
"""
import struct
import sys
from pathlib import Path

if len(sys.argv) < 3:
    raise SystemExit(__doc__)

src = Path(sys.argv[1])
out = Path(sys.argv[2])
W = int(sys.argv[3]) if len(sys.argv) > 3 else 960
H = int(sys.argv[4]) if len(sys.argv) > 4 else 405
FPS = int(sys.argv[5]) if len(sys.argv) > 5 else 15

files = sorted(src.glob("f_*.webp"))
if not files:
    raise SystemExit("no frames")
blobs = [p.read_bytes() for p in files]
for p, b in zip(files, blobs):
    if not b:
        raise SystemExit(f"empty frame: {p}")

head = b"AKUI1" + struct.pack("<HHHI", W, H, FPS, len(blobs))
head += b"".join(struct.pack("<I", len(b)) for b in blobs)
out.parent.mkdir(parents=True, exist_ok=True)
with out.open("wb") as f:
    f.write(head)
    for b in blobs:
        f.write(b)
total = out.stat().st_size
print(f"{len(blobs)} frames, {total} bytes ({total / 1024 / 1024:.2f} MB), header {len(head)}")
