#!/usr/bin/env python3
"""Write the probe cards the Affinity harness feeds to the real app.

Open one of these in Affinity, add the adjustment or effect being
probed, and Save As into fixtures/affinity-probe/. For a 512x512
document the file's embedded thumbnail is Affinity's own render, byte
for byte, so the saved file *is* the ground truth — see the "Probe
fixtures" section of docs/affinity-format.md.

    python3 tools/probecards.py <outdir>

  cube.png      the whole 64^3 RGB cube, 64 tiles of 64x64 (tile index
                = blue, x = red, y = green). One probe hands back an
                adjustment's complete transfer function.
  greycard.png  a full 0-255 ramp in grey and in each primary, two
                pixels per level. For anything per-channel, or where
                8-bit precision along one axis beats coverage.
  blurcard.png  an opaque square on transparency, for fitting the
                radius convention of a blur from its alpha edge.
"""

import sys

import numpy as np
from PIL import Image


def cube() -> np.ndarray:
    n = 64
    lut = np.round(np.arange(n) * 255 / (n - 1)).astype(np.uint8)
    img = np.zeros((512, 512, 3), np.uint8)
    for b in range(n):
        ty, tx = divmod(b, 8)
        tile = np.zeros((n, n, 3), np.uint8)
        tile[..., 0] = lut[None, :]
        tile[..., 1] = lut[:, None]
        tile[..., 2] = lut[b]
        img[ty * n : (ty + 1) * n, tx * n : (tx + 1) * n] = tile
    return img


def greycard() -> np.ndarray:
    img = np.zeros((512, 512, 3), np.uint8)
    lv = np.repeat(np.arange(256, dtype=np.uint8), 2)
    img[0:128] = lv[None, :, None]
    img[128:256, :, 0] = lv[None, :]
    img[256:384, :, 1] = lv[None, :]
    img[384:512, :, 2] = lv[None, :]
    return img


def blurcard() -> np.ndarray:
    img = np.zeros((512, 512, 4), np.uint8)
    img[128:384, 128:384] = 255
    return img


def main() -> None:
    out = sys.argv[1] if len(sys.argv) > 1 else "."
    Image.fromarray(cube()).save(f"{out}/cube.png")
    Image.fromarray(greycard()).save(f"{out}/greycard.png")
    Image.fromarray(blurcard(), "RGBA").save(f"{out}/blurcard.png")
    print(f"wrote cube.png, greycard.png, blurcard.png to {out}")


if __name__ == "__main__":
    main()
