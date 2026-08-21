#!/usr/bin/env python3
"""Generate every Schist logo asset from one geometry definition.

The mark is an S cut from banded rock.  Schist is a foliated stone and Schist
is a layered editor, so the letter is filled with parallel strata rather than
a flat colour -- the same joke twice.  The banding coarsens and then drops
away entirely as the icon gets smaller, because foliation at 16px is just
noise; see `detail`.

    python3 tools/logo.py

writes assets/logo/*.svg, packaging/macos/schist.icns,
packaging/windows/schist.ico and packaging/linux/schist.png.

Needs Pillow.  The SVG and the rasters come from the constants below, so
they cannot drift apart -- edit the geometry here, never the output.
"""

from __future__ import annotations

import io
import math
import os
import struct

from PIL import Image, ImageDraw

# --- geometry, in a 512x512 design space ------------------------------------

TILE_INSET = 26
TILE_RADIUS = 104

# The S, as cubic beziers: (start, (c1, c2, end), ...).
S_PATH = (
    (328.0, 186.0),
    ((328.0, 152.0), (294.0, 130.0), (252.0, 130.0)),
    ((210.0, 130.0), (178.0, 152.0), (178.0, 184.0)),
    ((178.0, 216.0), (206.0, 232.0), (256.0, 242.0)),
    ((310.0, 253.0), (336.0, 274.0), (336.0, 314.0)),
    ((336.0, 356.0), (300.0, 384.0), (250.0, 384.0)),
    ((206.0, 384.0), (176.0, 366.0), (172.0, 336.0)),
)

STROKE = 50.0

# Foliation: bands run across the mark at this angle, in this repeating ramp
# of tones.  Deep slate through to the pale glint of mica.
BAND_ANGLE = -32.0
BAND_WIDTH = 36.0
BAND_PHASE = 10.0
BANDS = ("#EDEFF3", "#8FB2D8", "#4A80BC", "#2A4E78", "#4A80BC", "#8FB2D8")
# Below 128px the full ramp turns to speckle, so the mark simplifies: a single
# broad highlight band at 64 and 96, one flat tone at 48 and below.
FLAT_TONE = "#6EA0DC"

GROUND_TOP = "#23262C"
GROUND_BOTTOM = "#131519"
# A hairline of light along the tile edge, the way a polished slab catches it.
RIM = (255, 255, 255, 28)


# --- raster ------------------------------------------------------------------


def rgb(color: str) -> tuple[int, int, int]:
    return tuple(int(color[i : i + 2], 16) for i in (1, 3, 5))


def flatten(scale: float, spacing: float) -> list[tuple[float, float]]:
    """The S path as a polyline with points no more than `spacing` px apart."""
    points = [tuple(c * scale for c in S_PATH[0])]
    current = points[0]
    for c1, c2, end in S_PATH[1:]:
        p0 = current
        p1, p2, p3 = (tuple(c * scale for c in p) for p in (c1, c2, end))
        # Curve length is at most the control polygon's, so this many steps
        # always keeps successive points within `spacing`.
        rough = sum(
            abs(a[0] - b[0]) + abs(a[1] - b[1])
            for a, b in zip((p0, p1, p2), (p1, p2, p3))
        )
        steps = max(2, int(rough / spacing) + 1)
        for step in range(1, steps + 1):
            t = step / steps
            u = 1.0 - t
            points.append(
                (
                    u * u * u * p0[0]
                    + 3 * u * u * t * p1[0]
                    + 3 * u * t * t * p2[0]
                    + t * t * t * p3[0],
                    u * u * u * p0[1]
                    + 3 * u * u * t * p1[1]
                    + 3 * u * t * t * p2[1]
                    + t * t * t * p3[1],
                )
            )
        current = p3
    return points


def mark_mask(size: int) -> Image.Image:
    """The S as a coverage mask.

    Pillow's thick `line` shreds tight curves, so the stroke is stamped as a
    dense run of discs instead -- which is what a round cap and join are.
    """
    scale = size / 512.0
    radius = STROKE * scale / 2.0
    mask = Image.new("L", (size, size), 0)
    draw = ImageDraw.Draw(mask)
    for x, y in flatten(scale, spacing=max(0.35, radius / 8.0)):
        draw.ellipse((x - radius, y - radius, x + radius, y + radius), fill=255)
    return mask


def detail(size: int) -> tuple[tuple[str, ...], float]:
    """The tones and band width to use at a given icon size."""
    if size <= 48:
        return (FLAT_TONE,), BAND_WIDTH
    if size <= 96:
        return ("#8FB2D8", "#3E76B2"), BAND_WIDTH * 4.5
    return BANDS, BAND_WIDTH


def foliation(size: int, tones: tuple[str, ...], band_width: float) -> Image.Image:
    """Parallel bands of rock, running across the whole tile."""
    scale = size / 512.0
    width = band_width * scale
    theta = math.radians(BAND_ANGLE)
    # Distance along the band normal decides which tone a pixel takes.
    nx, ny = math.sin(theta), -math.cos(theta)
    phase = BAND_PHASE * scale
    rgb_tones = [rgb(c) for c in tones]

    image = Image.new("RGB", (size, size))
    draw = ImageDraw.Draw(image)
    # Bands are straight, so drawing each as one long rotated rectangle beats
    # touching every pixel.
    reach = size * 1.5
    lo = int(-reach / width) - 1
    hi = int(reach / width) + 1
    for i in range(lo, hi + 1):
        d = i * width + phase
        cx, cy = size / 2.0 + nx * d, size / 2.0 + ny * d
        # Along the band, and across it.
        ax, ay = -ny * reach, nx * reach
        bx, by = nx * width / 2.0, ny * width / 2.0
        draw.polygon(
            [
                (cx - ax - bx, cy - ay - by),
                (cx + ax - bx, cy + ay - by),
                (cx + ax + bx, cy + ay + by),
                (cx - ax + bx, cy - ay + by),
            ],
            fill=rgb_tones[i % len(rgb_tones)],
        )
    return image


def ground(size: int) -> Image.Image:
    """The tile: a vertical gradient, rounded off, with a lit rim."""
    scale = size / 512.0
    top, bottom = rgb(GROUND_TOP), rgb(GROUND_BOTTOM)

    gradient = Image.new("RGB", (1, size))
    for y in range(size):
        t = y / max(size - 1, 1)
        gradient.putpixel(
            (0, y), tuple(int(round(a + (b - a) * t)) for a, b in zip(top, bottom))
        )
    tile = gradient.resize((size, size))

    inset, radius = TILE_INSET * scale, TILE_RADIUS * scale
    box = (inset, inset, size - inset - 1, size - inset - 1)

    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(box, radius=radius, fill=255)

    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(tile, (0, 0), mask)

    rim = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    ImageDraw.Draw(rim).rounded_rectangle(
        box, radius=radius, outline=RIM, width=max(1, int(round(2 * scale)))
    )
    return Image.alpha_composite(out, rim)


def render(size: int, supersample: int = 4) -> Image.Image:
    """The full mark at `size` px, drawn large and shrunk for clean edges."""
    big = size * supersample
    tones, band_width = detail(size)
    image = ground(big)
    image.paste(foliation(big, tones, band_width), (0, 0), mark_mask(big))
    return image.resize((size, size), Image.LANCZOS)


# --- svg ---------------------------------------------------------------------

PATH_DATA = "".join(
    [f"M{S_PATH[0][0]:g} {S_PATH[0][1]:g}"]
    + [
        f"C{c1[0]:g} {c1[1]:g} {c2[0]:g} {c2[1]:g} {e[0]:g} {e[1]:g}"
        for c1, c2, e in S_PATH[1:]
    ]
)


def svg(mark_only: bool = False) -> str:
    """The same geometry as real curves, for the README and the web.

    The foliation is a rotated stripe pattern.  Rotating by BAND_ANGLE + 180
    about the centre makes the pattern's y axis run along the band normal
    `foliation()` uses, so a stripe lands exactly where the raster puts it.
    """
    stripes = "\n".join(
        f'      <rect x="0" y="{i * BAND_WIDTH:g}" width="512" '
        f'height="{BAND_WIDTH:g}" fill="{c}"/>'
        for i, c in enumerate(BANDS)
    )
    defs = f"""    <pattern id="foliation" patternUnits="userSpaceOnUse"
             x="0" y="{256 + BAND_PHASE - BAND_WIDTH / 2:g}"
             width="512" height="{BAND_WIDTH * len(BANDS):g}"
             patternTransform="rotate({BAND_ANGLE + 180:g} 256 256)">
{stripes}
    </pattern>"""
    mark = (
        f'  <path d="{PATH_DATA}" fill="none" stroke="url(#foliation)"\n'
        f'        stroke-width="{STROKE:g}" stroke-linecap="round" '
        f'stroke-linejoin="round"/>'
    )

    if mark_only:
        return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">
  <defs>
{defs}
  </defs>
{mark}
</svg>
"""

    edge = TILE_INSET
    span = 512 - 2 * edge
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">
  <defs>
{defs}
    <linearGradient id="ground" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{GROUND_TOP}"/>
      <stop offset="1" stop-color="{GROUND_BOTTOM}"/>
    </linearGradient>
  </defs>
  <rect x="{edge}" y="{edge}" width="{span}" height="{span}" \
rx="{TILE_RADIUS}" fill="url(#ground)"/>
{mark}
  <rect x="{edge + 1}" y="{edge + 1}" width="{span - 2}" height="{span - 2}" \
rx="{TILE_RADIUS - 1}" fill="none" stroke="#fff" stroke-opacity="0.11" \
stroke-width="2"/>
</svg>
"""


# --- outputs -----------------------------------------------------------------

ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)
PREVIEW_SIZES = (512, 256, 128, 64, 32, 16)

# The chunk types `iconutil` emits, as (OSType, pixel size).  Each pair is a
# logical size and its @2x twin.
ICNS_CHUNKS = (
    (b"icp4", 16),
    (b"ic11", 32),
    (b"icp5", 32),
    (b"ic12", 64),
    (b"ic07", 128),
    (b"ic13", 256),
    (b"ic08", 256),
    (b"ic14", 512),
    (b"ic09", 512),
    (b"ic10", 1024),
)


def write_icns(path: str) -> None:
    """Assemble an .icns by hand; iconutil only exists on macOS.

    The container is a magic word, a total byte count, then one length-prefixed
    chunk per icon -- and every size macOS asks for today takes a plain PNG.
    """
    chunks = b""
    for ostype, size in ICNS_CHUNKS:
        buf = io.BytesIO()
        render(size).save(buf, format="PNG")
        png = buf.getvalue()
        chunks += ostype + struct.pack(">I", len(png) + 8) + png
    with open(path, "wb") as f:
        f.write(b"icns" + struct.pack(">I", len(chunks) + 8) + chunks)


def main() -> None:
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

    def out(*parts: str) -> str:
        path = os.path.join(root, *parts)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        return path

    with open(out("assets", "logo", "schist.svg"), "w") as f:
        f.write(svg())
    with open(out("assets", "logo", "schist-mark.svg"), "w") as f:
        f.write(svg(mark_only=True))

    render(512).save(out("assets", "logo", "schist-512.png"))
    render(1024).save(out("assets", "logo", "schist-1024.png"))
    render(256).save(out("packaging", "linux", "schist.png"))

    # Every entry is drawn at its own size: left to resize a single master,
    # Pillow would put the 256px foliation into the 16px entry as speckle.
    # The base image has to be the largest: Pillow drops any requested size
    # bigger than it.
    icons = [render(s) for s in ICO_SIZES]
    icons[-1].save(
        out("packaging", "windows", "schist.ico"),
        sizes=[(s, s) for s in ICO_SIZES],
        append_images=icons[:-1],
    )
    write_icns(out("packaging", "macos", "schist.icns"))

    # A single strip of every size the icon actually gets shown at.
    gap, pad = 16, 16
    width = sum(PREVIEW_SIZES) + gap * (len(PREVIEW_SIZES) - 1) + pad * 2
    sheet = Image.new("RGBA", (width, max(PREVIEW_SIZES) + pad * 2), (40, 42, 46, 255))
    x = pad
    for size in PREVIEW_SIZES:
        icon = render(size)
        sheet.paste(icon, (x, pad), icon)
        x += size + gap
    sheet.save(out("assets", "logo", "preview.png"))

    print("wrote assets/logo, packaging/{macos,windows,linux} icons")


if __name__ == "__main__":
    main()
