#!/usr/bin/env python3
"""Train the Content-Aware Fill network and export it to ONNX.

Photoshop fills a hole by inventing what was behind it. The classical
way to do that -- and the way this application did it until now -- is to
relax the boundary inwards, which is correct in the sense that it solves
Laplace's equation and useless in the sense that a wall of brick comes
back as a wall of beige.

This is a network that has been shown four hundred thousand holes and
what was behind them. It sees the picture with the hole punched out and
a mask saying where, and it predicts the whole picture back; only the
hole is kept. What it is good at is *structure* -- a horizon that
continues, a wall that stays a wall, a shadow that keeps its direction --
because that is what a convolutional decoder can learn from a corpus
this size on a CPU in an afternoon. What it is bad at is fine texture,
because an L1 objective always is, so the filter follows it with a
patch-synthesis pass that borrows real texture from the picture and uses
this only for the layout. The two together are what Content-Aware Fill
is.

The masks it trains on are the masks people draw: rectangles for
signage and lens flare, and free-form strokes for wires, tourists and
whatever else a lasso goes around.

Photographs come from `tools/train/photos.py` -- CC BY 2.0, Open Images.

Usage:  python3 tools/train/inpaint.py --data <dir of photographs> \\
            --out <model.onnx>
"""

import argparse
import concurrent.futures
import random
import time
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn
import torch.nn.functional as F
from PIL import Image

# Photographs are cached at this longest side: big enough that a crop is
# real texture rather than a downscale of one, small enough that the
# whole corpus fits in memory and no step waits on a JPEG decoder.
CACHE_SIDE = 448


def conv(cin: int, cout: int, stride: int = 1, dilation: int = 1) -> nn.Sequential:
    return nn.Sequential(
        nn.Conv2d(cin, cout, 3, stride=stride, padding=dilation, dilation=dilation, bias=False),
        nn.BatchNorm2d(cout),
        nn.ReLU(inplace=True),
    )


class InpaintNet(nn.Module):
    """A U-net over four channels: the picture with a hole, and the hole.

    The mask is a channel rather than a multiplication because the
    network has to be able to tell "black because it was painted black"
    from "black because it is missing", and those are the same pixels.

    The middle is dilated, which is the part that matters for a hole:
    the answer for a pixel in the centre of one is not in its
    neighbourhood -- there is nothing in its neighbourhood -- it is at
    the far edge, and a dilated stack at a quarter resolution is how
    that edge gets there.
    """

    def __init__(self, base: int = 28) -> None:
        super().__init__()
        c1, c2, c3, c4 = base, base * 2, base * 3, base * 4
        self.head = conv(4, c1)
        self.enc1 = conv(c1, c2, stride=2)
        self.enc2 = conv(c2, c3, stride=2)
        self.enc3 = conv(c3, c4, stride=2)
        self.mid1 = conv(c4, c4, dilation=2)
        self.mid2 = conv(c4, c4, dilation=4)
        self.mid3 = conv(c4, c4, dilation=8)
        self.dec1 = conv(c4 + c3, c3)
        self.dec2 = conv(c3 + c2, c2)
        self.dec3 = conv(c2 + c1, c1)
        self.out = nn.Conv2d(c1, 3, 3, padding=1)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        h1 = self.head(x)
        h2 = self.enc1(h1)
        h3 = self.enc2(h2)
        h4 = self.mid3(self.mid2(self.mid1(self.enc3(h3))))
        u = self.dec1(torch.cat([F.interpolate(h4, scale_factor=2, mode="nearest"), h3], 1))
        u = self.dec2(torch.cat([F.interpolate(u, scale_factor=2, mode="nearest"), h2], 1))
        u = self.dec3(torch.cat([F.interpolate(u, scale_factor=2, mode="nearest"), h1], 1))
        return torch.sigmoid(self.out(u))


def hole_mask(size: int, rng: random.Random) -> np.ndarray:
    """One mask: 1 where the picture is missing.

    Half rectangles, half free-form strokes, and a few of each -- a
    selection is rarely one tidy shape, and a network trained on one
    tidy shape learns its shape rather than its emptiness.
    """
    mask = np.zeros((size, size), dtype=np.float32)
    if rng.random() < 0.5:
        for _ in range(rng.randint(1, 3)):
            w = rng.randint(size // 8, size // 2)
            h = rng.randint(size // 8, size // 2)
            x = rng.randint(0, size - w)
            y = rng.randint(0, size - h)
            mask[y : y + h, x : x + w] = 1.0
        return mask
    # A free-form stroke: a few joined segments, drawn thick. Points on
    # each segment are stamped as discs, which is what a round brush is.
    for _ in range(rng.randint(1, 3)):
        x, y = rng.uniform(0, size), rng.uniform(0, size)
        radius = rng.uniform(size * 0.03, size * 0.11)
        for _ in range(rng.randint(2, 6)):
            angle = rng.uniform(0, 2 * np.pi)
            length = rng.uniform(size * 0.1, size * 0.45)
            nx, ny = x + np.cos(angle) * length, y + np.sin(angle) * length
            steps = int(length) + 1
            for t in np.linspace(0, 1, steps):
                stamp(mask, x + (nx - x) * t, y + (ny - y) * t, radius)
            x, y = np.clip(nx, 0, size - 1), np.clip(ny, 0, size - 1)
    return mask


def stamp(mask: np.ndarray, cx: float, cy: float, radius: float) -> None:
    size = mask.shape[0]
    x0, x1 = max(0, int(cx - radius)), min(size, int(cx + radius) + 1)
    y0, y1 = max(0, int(cy - radius)), min(size, int(cy + radius) + 1)
    if x0 >= x1 or y0 >= y1:
        return
    ys = np.arange(y0, y1)[:, None] - cy
    xs = np.arange(x0, x1)[None, :] - cx
    mask[y0:y1, x0:x1][ys * ys + xs * xs <= radius * radius] = 1.0


def load_photos(folder: Path, limit: int | None) -> list[np.ndarray]:
    paths = sorted(p for p in folder.iterdir() if p.suffix.lower() in {".jpg", ".jpeg", ".png"})
    if limit:
        paths = paths[:limit]

    def read(path: Path) -> np.ndarray | None:
        try:
            img = Image.open(path).convert("RGB")
        except Exception:
            return None
        if min(img.size) < 96:
            return None
        if max(img.size) > CACHE_SIDE:
            s = CACHE_SIDE / max(img.size)
            img = img.resize((max(1, int(img.width * s)), max(1, int(img.height * s))), Image.BICUBIC)
        return np.asarray(img, dtype=np.uint8)

    with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
        photos = [p for p in pool.map(read, paths) if p is not None]
    mb = sum(p.nbytes for p in photos) / 1024 / 1024
    print(f"{len(photos)} photographs ({mb:.0f} MB cached)", flush=True)
    return photos


def batch(
    photos: list[np.ndarray], count: int, size: int, rng: random.Random
) -> tuple[torch.Tensor, torch.Tensor]:
    """(truth, mask) -- the crop as it was, and where the hole goes."""
    truth = np.empty((count, size, size, 3), dtype=np.float32)
    masks = np.empty((count, size, size), dtype=np.float32)
    for i in range(count):
        for _ in range(8):
            p = photos[rng.randrange(len(photos))]
            if p.shape[0] >= size and p.shape[1] >= size:
                break
        y = rng.randint(0, max(0, p.shape[0] - size))
        x = rng.randint(0, max(0, p.shape[1] - size))
        crop = p[y : y + size, x : x + size]
        if crop.shape[0] != size or crop.shape[1] != size:
            crop = np.asarray(
                Image.fromarray(crop).resize((size, size), Image.BICUBIC), dtype=np.uint8
            )
        if rng.random() < 0.5:
            crop = crop[:, ::-1]
        truth[i] = crop.astype(np.float32) / 255.0
        masks[i] = hole_mask(size, rng)
    return (
        torch.from_numpy(truth).permute(0, 3, 1, 2).contiguous(),
        torch.from_numpy(masks).unsqueeze(1).contiguous(),
    )


def edges(x: torch.Tensor) -> torch.Tensor:
    return torch.cat(
        [x[:, :, :, 1:] - x[:, :, :, :-1], (x[:, :, 1:, :] - x[:, :, :-1, :]).transpose(2, 3)],
        dim=1,
    )


def feed(truth: torch.Tensor, mask: torch.Tensor) -> torch.Tensor:
    """The picture with the hole punched out, and the hole beside it."""
    return torch.cat([truth * (1.0 - mask), mask], dim=1)


def evaluate(
    model: nn.Module, photos: list[np.ndarray], size: int, rng: random.Random
) -> tuple[float, float]:
    """(error in the hole, how much detail it dares put there).

    The second number is the one to watch. A network that has given up
    and is filling holes with the average of their edges scores a
    respectable error and a gradient of nearly nothing; the truth's own
    gradient is the number it should be approaching.
    """
    model.eval()
    errs, grads = [], []
    with torch.no_grad():
        for _ in range(8):
            truth, mask = batch(photos, 8, size, rng)
            got = model(feed(truth, mask))
            errs.append(((got - truth).abs() * mask).sum().item() / (mask.sum().item() * 3 + 1e-6))
            gx = (got[:, :, :-1, 1:] - got[:, :, :-1, :-1]).abs()
            gy = (got[:, :, 1:, :-1] - got[:, :, :-1, :-1]).abs()
            grad = (gx + gy).mean(1, keepdim=True)
            inner = mask[:, :, :-1, :-1]
            grads.append((grad * inner).sum().item() / (inner.sum().item() + 1e-6))
    model.train()
    return sum(errs) / len(errs), sum(grads) / len(grads)


def export(model: nn.Module, path: Path, size: int) -> None:
    was = model.training
    model.eval()
    path.parent.mkdir(parents=True, exist_ok=True)
    torch.save(model.state_dict(), path.with_suffix(".pt"))
    torch.onnx.export(
        model,
        (torch.zeros(1, 4, size, size),),
        str(path),
        input_names=["input"],
        output_names=["filled"],
        opset_version=11,
        dynamo=False,
    )
    if was:
        model.train()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--steps", type=int, default=14000)
    ap.add_argument("--batch", type=int, default=12)
    ap.add_argument("--size", type=int, default=160)
    ap.add_argument("--base", type=int, default=28)
    ap.add_argument("--lr", type=float, default=2e-3)
    ap.add_argument("--limit", type=int, default=None)
    ap.add_argument("--holdout", type=int, default=300)
    ap.add_argument("--seed", type=int, default=7)
    args = ap.parse_args()

    torch.manual_seed(args.seed)
    rng = random.Random(args.seed)
    photos = load_photos(args.data, args.limit)
    if len(photos) < args.holdout + 100:
        raise SystemExit(f"only {len(photos)} photographs in {args.data}")
    rng.shuffle(photos)
    val, train = photos[: args.holdout], photos[args.holdout :]
    print(f"{len(train)} for training, {len(val)} held out", flush=True)

    model = InpaintNet(args.base)
    params = sum(p.numel() for p in model.parameters())
    print(f"{params} parameters ({params * 4 / 1024:.0f} KB as float32)", flush=True)
    opt = torch.optim.Adam(model.parameters(), lr=args.lr)
    sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, T_max=args.steps)

    started = time.time()
    for step in range(1, args.steps + 1):
        truth, mask = batch(train, args.batch, args.size, rng)
        got = model(feed(truth, mask))
        # The hole is the job, so it carries the weight; the rest is
        # there at a fifth of it because a network allowed to ignore
        # what it was given stops using it.
        hole = ((got - truth).abs() * mask).sum() / (mask.sum() * 3 + 1e-6)
        kept = ((got - truth).abs() * (1.0 - mask)).sum() / ((1.0 - mask).sum() * 3 + 1e-6)
        # Differences, not values: this is the term that decides whether
        # a filled hole has anything in it at all.
        edge = F.l1_loss(edges(got), edges(truth))
        loss = hole + 0.2 * kept + 0.5 * edge
        opt.zero_grad(set_to_none=True)
        loss.backward()
        opt.step()
        sched.step()
        if step % 250 == 0 or step == 1:
            print(
                f"  step {step:5d}  loss {loss.item():.5f}  {time.time() - started:5.0f}s",
                flush=True,
            )
        if step % 1000 == 0:
            err, grad = evaluate(model, val, args.size, random.Random(1))
            print(f"    held out: hole error {err:.4f}, detail {grad:.4f}", flush=True)
            export(model, args.out, args.size)

    err, grad = evaluate(model, val, args.size, random.Random(1))
    print(f"held-out hole error {err:.4f}, detail {grad:.4f}")
    export(model, args.out, args.size)
    print(f"wrote {args.out} ({args.out.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
