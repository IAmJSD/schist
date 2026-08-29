#!/usr/bin/env python3
"""Train the Sketch to Portrait network and export it to ONNX.

Photoshop's Sketch to Portrait invents a photograph from a drawing, and
it can do that because it is a generative model that has been shown an
enormous number of faces. This is not that, and does not claim to be.

What it is: a network trained to *invert this build's own Photo to
Sketch*. Every training pair is a face crop and the sketch our filter
makes of it, so the network learns the one thing that operator threw
away -- the tone and colour between the lines -- and puts it back. On a
sketch that came out of this application it reconstructs a plausible
face. On a pencil drawing from a sketchbook it does something in the
same spirit and rather worse, because nobody's pencil is a
colour-dodge blend.

That framing is also what makes the job small enough to ship: filling
in a known operator is a much easier problem than inventing a face, and
it fits in 450k parameters.

Faces come from `tools/train/faces.py`, which cuts them out of the Open
Images photographs `tools/train/photos.py` fetches -- all CC BY 2.0.

Usage:  python3 tools/train/portrait.py --data <dir of face crops> \\
            --out <model.onnx>
"""

import argparse
import random
import time
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn
import torch.nn.functional as F
from PIL import Image

# Rec.601, matching `luma` in the filter set.
LUMA = (0.299, 0.587, 0.114)


def conv(cin: int, cout: int, stride: int = 1, dilation: int = 1) -> nn.Sequential:
    return nn.Sequential(
        nn.Conv2d(cin, cout, 3, stride=stride, padding=dilation, dilation=dilation, bias=False),
        nn.BatchNorm2d(cout),
        nn.ReLU(inplace=True),
    )


class PortraitNet(nn.Module):
    """A U-net: encoder, dilated middle, decoder with skips.

    The skips are the point. Filling in a sketch is mostly a local
    question -- what colour is this bit of cheek -- but *which* bit of
    cheek it is takes the whole face, so the middle sees far and the
    skips carry the drawing's own edges straight to the output. Without
    them the lines come back soft, which on a face reads immediately as
    wrong.
    """

    def __init__(self, base: int = 24) -> None:
        super().__init__()
        c1, c2, c3, c4 = base, base * 2, base * 3, base * 4
        self.head = conv(3, c1)
        self.enc1 = conv(c1, c2, stride=2)
        self.enc2 = conv(c2, c3, stride=2)
        self.enc3 = conv(c3, c4, stride=2)
        self.mid1 = conv(c4, c4, dilation=2)
        self.mid2 = conv(c4, c4, dilation=2)
        self.dec1 = conv(c4 + c3, c3)
        self.dec2 = conv(c3 + c2, c2)
        self.dec3 = conv(c2 + c1, c1)
        self.out = nn.Conv2d(c1, 3, 3, padding=1)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        h1 = self.head(x)
        h2 = self.enc1(h1)
        h3 = self.enc2(h2)
        h4 = self.mid2(self.mid1(self.enc3(h3)))
        u = self.dec1(torch.cat([F.interpolate(h4, scale_factor=2, mode="nearest"), h3], 1))
        u = self.dec2(torch.cat([F.interpolate(u, scale_factor=2, mode="nearest"), h2], 1))
        u = self.dec3(torch.cat([F.interpolate(u, scale_factor=2, mode="nearest"), h1], 1))
        # Bounded, because a photograph is: an unbounded head spends its
        # first epochs learning that pixels stop at white.
        return torch.sigmoid(self.out(u))


def blur(x: torch.Tensor, sigma: float) -> torch.Tensor:
    """Separable Gaussian, the same shape of blur the filter uses."""
    radius = max(1, int(sigma * 2.5))
    t = torch.arange(-radius, radius + 1, dtype=torch.float32, device=x.device)
    k = torch.exp(-(t**2) / (2 * sigma * sigma))
    k = (k / k.sum()).view(1, 1, 1, -1)
    c = x.shape[1]
    x = F.conv2d(F.pad(x, (radius, radius, 0, 0), mode="reflect"), k.expand(c, 1, 1, -1), groups=c)
    k = k.view(1, 1, -1, 1)
    return F.conv2d(F.pad(x, (0, 0, radius, radius), mode="reflect"), k.expand(c, 1, -1, 1), groups=c)


def sketch_of(rgb: torch.Tensor, detail: float, weight: float, shading: float) -> torch.Tensor:
    """Photo to Sketch, as the filter does it.

    Invert the picture, blur it, divide the original by it: where the two
    agree the quotient saturates to white and the paper is left blank,
    and where an edge makes them disagree a line appears. Kept in step
    with `plugins/filters-core/src/neural.rs` -- if that changes, the
    network has to be retrained against the new one.
    """
    plane = LUMA[0] * rgb[:, 0:1] + LUMA[1] * rgb[:, 1:2] + LUMA[2] * rgb[:, 2:3]
    soft = blur(1.0 - plane, 1.0 + (1.0 - detail) * 12.0)
    dodge = (plane / (1.0 - soft).clamp(min=1e-3)).clamp(max=1.0)
    line = 1.0 - ((1.0 - dodge) * (0.5 + weight * 3.0)).clamp(max=1.0)
    out = (line - (1.0 - plane) * shading * 0.6).clamp(0.0, 1.0)
    return out.repeat(1, 3, 1, 1)


def load_faces(folder: Path, size: int, limit: int | None) -> list[np.ndarray]:
    faces = []
    paths = sorted(p for p in folder.iterdir() if p.suffix.lower() in {".png", ".jpg"})
    for path in paths[:limit] if limit else paths:
        try:
            img = Image.open(path).convert("RGB")
        except Exception:
            continue
        if min(img.size) < size:
            continue
        faces.append(np.asarray(img.resize((size, size), Image.BICUBIC), dtype=np.uint8))
    print(f"{len(faces)} faces", flush=True)
    return faces


def batch(faces: list[np.ndarray], count: int, rng: random.Random) -> torch.Tensor:
    out = np.empty((count, faces[0].shape[0], faces[0].shape[1], 3), dtype=np.float32)
    for i in range(count):
        f = faces[rng.randrange(len(faces))]
        if rng.random() < 0.5:
            f = f[:, ::-1]
        out[i] = f.astype(np.float32) / 255.0
    return torch.from_numpy(out).permute(0, 3, 1, 2).contiguous()


def edges(x: torch.Tensor) -> torch.Tensor:
    """Horizontal and vertical differences, for the edge loss."""
    return torch.cat(
        [x[:, :, :, 1:] - x[:, :, :, :-1], (x[:, :, 1:, :] - x[:, :, :-1, :]).transpose(2, 3)],
        dim=1,
    )


def evaluate(model: nn.Module, faces: list[np.ndarray], rng: random.Random) -> tuple[float, float]:
    """(mean absolute error, how much colour it dares to use)."""
    model.eval()
    errs, chroma = [], []
    with torch.no_grad():
        for i in range(0, min(len(faces), 128), 16):
            rgb = batch(faces[i : i + 16], min(16, len(faces) - i), rng)
            got = model(sketch_of(rgb, 0.5, 0.5, 0.4))
            errs.append((got - rgb).abs().mean().item())
            grey = (got * torch.tensor(LUMA).view(1, 3, 1, 1)).sum(1, keepdim=True)
            chroma.append((got - grey).abs().mean().item())
    model.train()
    return sum(errs) / len(errs), sum(chroma) / len(chroma)


def export(model: nn.Module, path: Path, size: int) -> None:
    was = model.training
    model.eval()
    path.parent.mkdir(parents=True, exist_ok=True)
    torch.save(model.state_dict(), path.with_suffix(".pt"))
    torch.onnx.export(
        model,
        (torch.zeros(1, 3, size, size),),
        str(path),
        input_names=["input"],
        output_names=["portrait"],
        opset_version=11,
        dynamo=False,
    )
    if was:
        model.train()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--steps", type=int, default=12000)
    ap.add_argument("--batch", type=int, default=16)
    ap.add_argument("--size", type=int, default=128)
    ap.add_argument("--base", type=int, default=24)
    ap.add_argument("--lr", type=float, default=2e-3)
    ap.add_argument("--limit", type=int, default=None)
    ap.add_argument("--holdout", type=int, default=200)
    ap.add_argument("--seed", type=int, default=7)
    args = ap.parse_args()

    torch.manual_seed(args.seed)
    rng = random.Random(args.seed)
    faces = load_faces(args.data, args.size, args.limit)
    if len(faces) < args.holdout + 50:
        raise SystemExit(f"only {len(faces)} faces in {args.data}")
    rng.shuffle(faces)
    val, train = faces[: args.holdout], faces[args.holdout :]
    print(f"{len(train)} for training, {len(val)} held out", flush=True)

    model = PortraitNet(args.base)
    params = sum(p.numel() for p in model.parameters())
    print(f"{params} parameters ({params * 4 / 1024:.0f} KB as float32)", flush=True)
    opt = torch.optim.Adam(model.parameters(), lr=args.lr)
    sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, T_max=args.steps)

    started = time.time()
    for step in range(1, args.steps + 1):
        rgb = batch(train, args.batch, rng)
        # The sketch settings vary per batch, so the network learns to
        # fill in *a* drawing rather than one particular rendering.
        sketch = sketch_of(
            rgb,
            rng.uniform(0.3, 0.8),
            rng.uniform(0.25, 0.8),
            rng.uniform(0.1, 0.7),
        )
        got = model(sketch)
        # L1 for the tone, plus L1 on the differences: without the
        # second term a network fitted to absolute error alone learns
        # that the safest face is a blurry one.
        loss = F.l1_loss(got, rgb) + 0.5 * F.l1_loss(edges(got), edges(rgb))
        opt.zero_grad(set_to_none=True)
        loss.backward()
        opt.step()
        sched.step()
        if step % 250 == 0 or step == 1:
            print(f"  step {step:5d}  loss {loss.item():.5f}  {time.time() - started:5.0f}s",
                  flush=True)
        if step % 1000 == 0:
            err, chroma = evaluate(model, val, random.Random(1))
            print(f"    held out: error {err:.4f}, colour {chroma:.4f}", flush=True)
            export(model, args.out, args.size)

    err, chroma = evaluate(model, val, random.Random(1))
    print(f"held-out error {err:.4f}, colour {chroma:.4f}")
    export(model, args.out, args.size)
    print(f"wrote {args.out} ({args.out.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
