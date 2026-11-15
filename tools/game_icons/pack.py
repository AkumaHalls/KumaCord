"""Packs the downloaded game icons into app/assets/game-icons.pack.

Usage: python tools/game_icons/pack.py <manifest.json> <icons dir> <out.pack>

Layout (little endian):
  b"KUIG" | u8 version | u32 count | per entry:
    u32 key_len | key bytes | u32 img_len | image bytes (PNG)

The app reads this file with include_bytes! and decodes on demand: a missing or
damaged pack simply leaves the whole feature off (the Discord/Steam/emoji sources
keep working). Keys are normalised game names (lowercase alphanumerics only), so
"Counter-Strike 2" and "cs2" are two separate entries when the catalogue spells
them differently - both point at the same art.
"""
import json
import struct
import sys
from pathlib import Path

FORMAT = 1


def main() -> None:
    manifest, icons_dir, out = sys.argv[1], sys.argv[2], sys.argv[3]
    entries = json.load(open(manifest, encoding='utf-8'))
    root = Path(icons_dir)
    blobs = []
    for e in entries:
        p = root / f"{e['key']}.png"
        if not p.exists():
            raise SystemExit(f"icone faltando: {p} (baixe antes de empacotar)")
        blobs.append((e['key'].encode(), p.read_bytes()))

    data = b"KUIG" + struct.pack("<BI", FORMAT, len(blobs))
    for k, b in blobs:
        # one entry: the normalised key, then the PNG bytes.
        data += struct.pack("<I", len(k)) + k
        data += struct.pack("<I", len(b)) + b
    Path(out).parent.mkdir(parents=True, exist_ok=True)
    Path(out).write_bytes(data)
    print(f"{len(blobs)} icones, {len(data)} bytes ({len(data) / 1024 / 1024:.2f} MB)")


if __name__ == "__main__":
    main()