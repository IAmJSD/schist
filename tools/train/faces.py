#!/usr/bin/env python3
"""Cut face crops out of a photograph corpus, for the portrait networks.

Sketch to Portrait has to be trained on faces, and a corpus of general
photographs is mostly not faces. This finds them with the same detector
the application downloads -- UltraFace, from Filter ▸ Neural Filters ▸
Manage Models, or straight from the ONNX Model Zoo -- so the crops the
network learns from are framed exactly the way the filter will frame
them at run time.

The box is grown before cropping: a detector's box stops at the jaw and
the hairline, and a portrait is not a portrait without either.

Needs OpenCV -- `pip install opencv-python` -- which is used for its
ONNX runner and its image I/O; the training scripts themselves need only
PyTorch and Pillow.

Usage:
  python3 tools/train/faces.py --photos <dir> --out <dir> \\
      --model ~/.local/share/schist/models/face.onnx
"""

import argparse
from pathlib import Path

import cv2
import numpy as np

# What the detector wants: 320x240, mean 127, scale 1/128.
NET_W, NET_H = 320, 240
CONFIDENCE = 0.75
# Detector boxes are tight; a portrait wants this much more around them.
GROW = 1.9
# Below this the face is too small in the original to be worth training
# on -- upscaling it would teach the network to reproduce mush.
MIN_FACE = 110


def detect(net, image):
    """Face boxes in image pixels, as (x0, y0, x1, y1, score)."""
    h, w = image.shape[:2]
    blob = cv2.dnn.blobFromImage(
        image, 1 / 128.0, (NET_W, NET_H), (127, 127, 127), swapRB=True
    )
    net.setInput(blob)
    scores, boxes = net.forward(["scores", "boxes"])
    scores, boxes = scores[0], boxes[0]
    keep = []
    for i in range(scores.shape[0]):
        s = float(scores[i][1])
        if s < CONFIDENCE:
            continue
        x0, y0, x1, y1 = boxes[i]
        keep.append((x0 * w, y0 * h, x1 * w, y1 * h, s))
    # Non-maximum suppression, best first: a single-shot detector fires
    # several times on one face.
    keep.sort(key=lambda b: -b[4])
    out = []
    for b in keep:
        if all(iou(b, k) < 0.4 for k in out):
            out.append(b)
    return out


def iou(a, b):
    x = min(a[2], b[2]) - max(a[0], b[0])
    y = min(a[3], b[3]) - max(a[1], b[1])
    if x <= 0 or y <= 0:
        return 0.0
    inter = x * y
    area = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1])
    return inter / max(area - inter, 1e-6)


def crop_of(image, box, size):
    """A square crop around a face, or None if it runs off the picture."""
    h, w = image.shape[:2]
    x0, y0, x1, y1, _ = box
    side = max(x1 - x0, y1 - y0) * GROW
    if side < MIN_FACE:
        return None
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    # A little above centre: a face's box is the face, and the head goes
    # further up than it goes down.
    cy -= side * 0.06
    left, top = int(cx - side / 2), int(cy - side / 2)
    right, bottom = int(left + side), int(top + side)
    if left < 0 or top < 0 or right > w or bottom > h:
        return None
    return cv2.resize(
        image[top:bottom, left:right], (size, size), interpolation=cv2.INTER_AREA
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--photos", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--model", type=Path, required=True)
    ap.add_argument("--size", type=int, default=256)
    ap.add_argument("--limit", type=int, default=None)
    args = ap.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)
    net = cv2.dnn.readNetFromONNX(str(args.model))
    paths = sorted(p for p in args.photos.iterdir() if p.suffix.lower() in {".jpg", ".jpeg", ".png"})
    if args.limit:
        paths = paths[: args.limit]

    kept = 0
    for i, path in enumerate(paths):
        image = cv2.imread(str(path))
        if image is None:
            continue
        try:
            boxes = detect(net, image)
        except Exception as e:
            print(f"  {path.name}: {e}", flush=True)
            continue
        for j, box in enumerate(boxes):
            face = crop_of(image, box, args.size)
            if face is None:
                continue
            # Skip the ones with no colour in them: a colouring network
            # trained on black-and-white photographs learns to make
            # black-and-white photographs.
            f = face.astype(np.float32) / 255.0
            grey = f.mean(axis=2, keepdims=True)
            if float(np.abs(f - grey).mean()) < 0.02:
                continue
            cv2.imwrite(str(args.out / f"{path.stem}-{j}.png"), face)
            kept += 1
        if (i + 1) % 500 == 0:
            print(f"  {i + 1}/{len(paths)}, {kept} faces", flush=True)
    print(f"{kept} faces in {args.out}")


if __name__ == "__main__":
    main()
