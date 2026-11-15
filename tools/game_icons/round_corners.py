"""Rounds the corners of the game icons (transparent corners), the way Discord draws its
activity art, so the tile reads as a rounded square on the row instead of a full square.

Usage: python tools/game_icons/round_corners.py <icons dir> [size] [radius]

Pure standard library: the rounded-rect mask is written as a PNG (supersampled 4x for a
smooth edge) and ffmpeg multiplies it into each icon's alpha.
"""
import struct
import subprocess
import sys
import zlib
from pathlib import Path

FF = r"C:\Users\Administrator\tools\ff\node_modules\ffmpeg-static\ffmpeg.exe"


def write_png(path: Path, w: int, h: int, rgba: bytes) -> None:
    raw = b"".join(b"\x00" + rgba[y * w * 4:(y + 1) * w * 4] for y in range(h))
    def chunk(tag: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    Path(path).write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


def rounded_mask(size: int, radius: float, ss: int = 4) -> bytes:
    """An RGBA mask: white and opaque inside the rounded rect, transparent outside."""
    big = size * ss
    r = radius * ss
    out = bytearray(big * big * 4)
    for y in range(big):
        for x in range(big):
            # Distance from the pixel centre to the rect, then to the nearest corner disc.
            dx = max(r - x, x - (big - 1 - r), 0.0)
            dy = max(r - y, y - (big - 1 - r), 0.0)
            dist = (dx * dx + dy * dy) ** 0.5 if (dx or dy) else 0.0
            cover = min(max(r - dist + 0.5, 0.0), 1.0) if dist else 1.0
            i = (y * big + x) * 4
            v = int(round(cover * 255))
            out[i] = out[i + 1] = out[i + 2] = 255
            out[i + 3] = v
    # Box-downscale by ss: the coverage becomes the alpha of each final pixel.
    small = bytearray(size * size * 4)
    for y in range(size):
        for x in range(size):
            total = 0
            for sy in range(ss):
                for sx in range(ss):
                    total += out[((y * ss + sy) * big + (x * ss + sx)) * 4 + 3]
            i = (y * size + x) * 4
            a = total // (ss * ss)
            small[i] = small[i + 1] = small[i + 2] = 255
            small[i + 3] = a
    return bytes(small)


def main() -> None:
    icons = Path(sys.argv[1])
    size = int(sys.argv[2]) if len(sys.argv) > 2 else 64
    # Discord rounds activity icons at roughly a quarter of their side; the same fraction here.
    radius = size * 0.25
    mask = icons.parent / "corner-mask.png"
    write_png(mask, size, size, rounded_mask(size, radius))
    done = 0
    for p in sorted(icons.glob("*.png")):
        tmp = p.with_suffix(".rounded.png")
        r = subprocess.run([
            FF, "-hide_banner", "-loglevel", "error", "-y",
            "-i", str(p), "-i", str(mask),
            "-filter_complex", "[0][1]blend=all_mode=multiply",
            str(tmp),
        ], capture_output=True)
        if r.returncode != 0:
            raise SystemExit(f"ffmpeg falhou em {p.name}: {r.stderr.decode(errors='replace')}")
        tmp.replace(p)
        done += 1
    mask.unlink()
    print(f"{done} icones com cantos arredondados (raio {radius:.0f}px de {size}px)")


if __name__ == "__main__":
    main()