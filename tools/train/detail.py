#!/usr/bin/env python3
"""Train the Super Zoom detail-restoration network and export it to ONNX.

The filter cannot change its buffer's size, so the job is the *other* half
of an upscale: given an image that has already been enlarged and has lost
its high frequencies, put them back. That is the classic SRCNN/VDSR
formulation -- the network sees a bicubic-upscaled image and predicts the
residual between it and the original.

Residual learning matters here. Predicting the *difference* rather than
the image means the network starts at "output = input", which is already
most of the answer, so a network this small converges in minutes rather
than not at all.

Trained on the Kodak True Color Image Suite, which Kodak released for
unrestricted use and which has been the standard low-level-vision
benchmark for thirty years. Four of the twenty-four images are held out
so the reported score is not the training score.

Usage:  python3 tools/train/detail.py --data <dir of pngs> --out <model.onnx>
"""

import argparse
import math
import random
import time
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn
import torch.nn.functional as F
from PIL import Image

# Held out from training so the score at the end means something.
VALIDATION = {"kodim05", "kodim13", "kodim19", "kodim23"}


class DetailNet(nn.Module):
    """A small residual CNN: four 3x3 convolutions and a skip.

    Kept deliberately tiny -- about 21k parameters, 84 KB as float32 --
    because it ships inside the application and runs on the CPU over
    however many megapixels the user points it at. Everything about the
    shape is a cost/quality trade rather than a copy of a paper.
    """

    def __init__(self, channels: int = 32, layers: int = 4) -> None:
        super().__init__()
        assert layers >= 2
        body = [nn.Conv2d(3, channels, 3, padding=1)]
        body += [nn.Conv2d(channels, channels, 3, padding=1) for _ in range(layers - 2)]
        self.body = nn.ModuleList(body)
        self.tail = nn.Conv2d(channels, 3, 3, padding=1)
        # Start the tail at zero so the untrained network is exactly the
        # identity. Without this the first epochs are spent undoing random
        # noise the network added to the image.
        nn.init.zeros_(self.tail.weight)
        nn.init.zeros_(self.tail.bias)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        h = x
        for conv in self.body:
            h = F.relu(conv(h))
        return x + self.tail(h)


def degrade(batch: torch.Tensor) -> torch.Tensor:
    """Halve and restore an image, which is what an enlargement costs it."""
    small = F.interpolate(batch, scale_factor=0.5, mode="bicubic", align_corners=False)
    back = F.interpolate(small, size=batch.shape[-2:], mode="bicubic", align_corners=False)
    return back.clamp(0.0, 1.0)


def load_images(folder: Path) -> tuple[list[np.ndarray], list[np.ndarray]]:
    train, val = [], []
    for path in sorted(folder.glob("*.png")):
        arr = np.asarray(Image.open(path).convert("RGB"), dtype=np.float32) / 255.0
        (val if path.stem in VALIDATION else train).append(arr)
    return train, val


def patches(images: list[np.ndarray], size: int, count: int, rng: random.Random):
    """Random crops, with the flips and rotations that quadruple the data."""
    out = np.empty((count, size, size, 3), dtype=np.float32)
    for i in range(count):
        img = images[rng.randrange(len(images))]
        y = rng.randrange(img.shape[0] - size)
        x = rng.randrange(img.shape[1] - size)
        p = img[y : y + size, x : x + size]
        if rng.random() < 0.5:
            p = p[:, ::-1]
        if rng.random() < 0.5:
            p = p[::-1, :]
        if rng.random() < 0.5:
            p = p.transpose(1, 0, 2)
        out[i] = p
    return torch.from_numpy(out).permute(0, 3, 1, 2).contiguous()


def psnr(a: torch.Tensor, b: torch.Tensor) -> float:
    mse = F.mse_loss(a.clamp(0, 1), b).item()
    return 99.0 if mse <= 1e-12 else 10.0 * math.log10(1.0 / mse)


def evaluate(model: nn.Module, images: list[np.ndarray]) -> tuple[float, float]:
    """Return (bicubic PSNR, model PSNR) over the held-out images."""
    model.eval()
    base, ours = [], []
    with torch.no_grad():
        for img in images:
            t = torch.from_numpy(img).permute(2, 0, 1).unsqueeze(0)
            # Crop to a multiple of two so the halving is exact.
            h, w = (t.shape[-2] // 2) * 2, (t.shape[-1] // 2) * 2
            t = t[..., :h, :w]
            d = degrade(t)
            base.append(psnr(d, t))
            ours.append(psnr(model(d), t))
    model.train()
    return sum(base) / len(base), sum(ours) / len(ours)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--steps", type=int, default=4000)
    ap.add_argument("--batch", type=int, default=16)
    ap.add_argument("--patch", type=int, default=64)
    ap.add_argument("--channels", type=int, default=32)
    ap.add_argument("--layers", type=int, default=4)
    ap.add_argument("--lr", type=float, default=1e-3)
    ap.add_argument("--seed", type=int, default=7)
    args = ap.parse_args()

    torch.manual_seed(args.seed)
    rng = random.Random(args.seed)

    train_imgs, val_imgs = load_images(args.data)
    if not train_imgs:
        raise SystemExit(f"no training images in {args.data}")
    print(f"{len(train_imgs)} training images, {len(val_imgs)} held out")

    model = DetailNet(args.channels, args.layers)
    params = sum(p.numel() for p in model.parameters())
    print(f"{params} parameters ({params * 4 / 1024:.0f} KB as float32)")

    opt = torch.optim.Adam(model.parameters(), lr=args.lr)
    sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, T_max=args.steps)

    started = time.time()
    for step in range(1, args.steps + 1):
        target = patches(train_imgs, args.patch, args.batch, rng)
        got = model(degrade(target))
        # L1 rather than L2: it is what super-resolution work settled on,
        # because squared error prefers a blurry average over a sharp
        # guess that might be slightly wrong.
        loss = F.l1_loss(got, target)
        opt.zero_grad(set_to_none=True)
        loss.backward()
        opt.step()
        sched.step()
        if step % 500 == 0 or step == 1:
            print(f"  step {step:5d}  loss {loss.item():.5f}  {time.time() - started:5.0f}s")

    if val_imgs:
        base, ours = evaluate(model, val_imgs)
        print(f"held-out PSNR: bicubic {base:.2f} dB -> model {ours:.2f} dB "
              f"({ours - base:+.2f})")

    # Export with dynamic height and width: the app runs this over tiles
    # whose size it chooses, not a fixed input.
    model.eval()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    torch.onnx.export(
        model,
        (torch.zeros(1, 3, args.patch, args.patch),),
        str(args.out),
        input_names=["input"],
        output_names=["output"],
        dynamic_axes={"input": {2: "h", 3: "w"}, "output": {2: "h", 3: "w"}},
        opset_version=11,
        dynamo=False,
    )
    print(f"wrote {args.out} ({args.out.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
