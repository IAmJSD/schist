#!/usr/bin/env python3
"""Read a cube-card render back as the filter's displacement field.

`cube.png` from probecards.py gives every pixel of a 512x512 document a
distinct colour — red is x within its 64-pixel tile, green is y, blue is
which tile — so a filter that moves pixels about rather than recolouring
them writes its whole inverse map into the render: the colour that lands
at a destination pixel says which source pixel it came from, to a
fraction of a pixel wherever the resample interpolated.

    cargo run -p schist-codec-affinity --example afthumb -- probe.af r.png
    python3 tools/uvdecode.py r.png              # radius and angle profile
    python3 tools/uvdecode.py r.png --field      # the raw source field

Sample away from the tile boundaries: there the blue channel is a blend
of two tile indices and the decode means nothing, which is what `good`
below drops.
"""

import argparse

import numpy as np
from PIL import Image


def decode(path):
    """Source (x, y) for every destination pixel, and a validity mask."""
    a = np.asarray(Image.open(path).convert("RGB")).astype(np.float64)
    tile = np.clip(np.round(a[..., 2] * 63 / 255).astype(int), 0, 63)
    sx = (tile % 8) * 64 + a[..., 0] * 63 / 255
    sy = (tile // 8) * 64 + a[..., 1] * 63 / 255
    h, w = sx.shape
    ys, xs = np.mgrid[0:h, 0:w].astype(float)
    good = ((xs % 64) > 4) & ((xs % 64) < 59) & ((ys % 64) > 4) & ((ys % 64) < 59)
    return sx, sy, good


def profile(path, cx, cy, step):
    sx, sy, good = decode(path)
    h, w = sx.shape
    ys, xs = np.mgrid[0:h, 0:w].astype(float)
    dx, dy = xs + 0.5 - cx, ys + 0.5 - cy
    sdx, sdy = sx + 0.5 - cx, sy + 0.5 - cy
    r = np.hypot(dx, dy)
    sr = np.hypot(sdx, sdy)
    dth = (np.arctan2(sdy, sdx) - np.arctan2(dy, dx) + np.pi) % (2 * np.pi) - np.pi
    print("    r    source r   sr/r    turn (deg)      n")
    for q in np.arange(step, r[good].max(), step):
        sel = good & (np.abs(r - q) < step / 2)
        if sel.sum() < 30:
            continue
        print(f"{q:7.1f} {np.median(sr[sel]):10.4f} {np.median(sr[sel] / r[sel]):8.4f}"
              f" {np.degrees(np.median(dth[sel])):11.4f} {sel.sum():7d}")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("render", help="the probe's thumbnail, from afthumb")
    ap.add_argument("--centre", type=float, nargs=2, default=(256.0, 256.0))
    ap.add_argument("--step", type=float, default=8.0)
    ap.add_argument("--field", action="store_true", help="dump the raw field")
    args = ap.parse_args()
    if args.field:
        sx, sy, good = decode(args.render)
        np.save("uv.npy", np.stack([sx, sy, good]))
        print("wrote uv.npy: source x, source y, validity")
    else:
        profile(args.render, args.centre[0], args.centre[1], args.step)


if __name__ == "__main__":
    main()
