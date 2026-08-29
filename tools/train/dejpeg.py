#!/usr/bin/env python3
"""Train the JPEG Artifact Removal network and export it to ONNX.

JPEG throws away high frequencies inside 8x8 blocks, so what comes back
is blocking -- steps at the block edges that were not in the picture --
and ringing, the overshoot around every hard edge. Both are a *known*
degradation with a known structure, which is why a network this small can
undo a useful amount of it: it is not inventing detail, it is recognising
a compressor's signature.

Same shape of problem as `detail.py`, so the same shape of answer: a
residual CNN that predicts the difference between what it was given and
what the photograph was, starting from the identity so the first epochs
are not spent undoing random noise.

Two details matter more than the architecture.

*Phase.* Real blocking is aligned to the image's own 8x8 grid, but a
filter runs on a selection that can start anywhere, so patches are cut
from the compressed image at unaligned offsets. The network has to find
the grid in the picture rather than assume it is at a multiple of eight.

*Quality.* Trained across quality 10 to 60 at once rather than at one
setting, because the filter is not told what quality the file was, and a
network fitted to quality 20 does very little at quality 50.

Trained on the Kodak True Color Image Suite, which Kodak released for
unrestricted use, with four images held out.

Usage:  python3 tools/train/dejpeg.py --data <dir of pngs> --out <model.onnx>
"""

import argparse
import io
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

# The qualities the network is fitted over. Below 10 the image is gone
# and no filter will bring it back; above 60 there is little left to
# remove and the risk is a network that smooths a clean photograph.
QUALITIES = (10, 15, 20, 25, 30, 40, 50, 60)


class DeblockNet(nn.Module):
    """A residual CNN, wider and one layer deeper than the Super Zoom one.

    Blocking is a longer-range pattern than the missing high frequencies
    of an enlargement -- an 8x8 grid has to be *seen* as a grid before it
    can be told apart from real edges eight pixels apart -- so this needs
    the receptive field the extra layers buy. It is still 66k parameters,
    which is 264 KB and ships inside the binary.

    Normalised and Kaiming-initialised, both of which turned out to
    matter more than the size. With PyTorch's default convolution
    initialisation the activations shrink by about a third a layer, so
    what reaches the zero-initialised tail is small enough that the
    network trains for thousands of steps and stays at the identity --
    scoring exactly as well as doing nothing, which is a very quiet way
    to fail. Kaiming initialisation alone fixes it; the normalisation is
    worth another two thirds on top.
    """

    def __init__(self, channels: int = 48, layers: int = 5) -> None:
        super().__init__()
        assert layers >= 2
        body = [nn.Conv2d(3, channels, 3, padding=1)]
        body += [nn.Conv2d(channels, channels, 3, padding=1) for _ in range(layers - 2)]
        self.body = nn.ModuleList(body)
        self.norm = nn.ModuleList([nn.BatchNorm2d(channels) for _ in body])
        self.tail = nn.Conv2d(channels, 3, 3, padding=1)
        for conv in self.body:
            nn.init.kaiming_normal_(conv.weight, nonlinearity="relu")
            nn.init.zeros_(conv.bias)
        # Start at the identity: the network's first answer is "the
        # photograph is already right", which is most of the answer.
        nn.init.zeros_(self.tail.weight)
        nn.init.zeros_(self.tail.bias)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        h = x
        for conv, norm in zip(self.body, self.norm):
            h = F.relu(norm(conv(h)))
        return x + self.tail(h)


def compress(img: np.ndarray, quality: int) -> np.ndarray:
    """Round-trip an image through JPEG at `quality`."""
    buf = io.BytesIO()
    Image.fromarray((img * 255.0 + 0.5).astype(np.uint8)).save(
        buf, format="JPEG", quality=quality
    )
    buf.seek(0)
    return np.asarray(Image.open(buf).convert("RGB"), dtype=np.float32) / 255.0


def load_images(folder: Path) -> tuple[list, list]:
    """Every image, with its compressed versions at every quality.

    Compressing up front rather than per patch is what makes the training
    loop fast: a JPEG round trip of a 768x512 image costs more than the
    gradient step it would be feeding.
    """
    train, val = [], []
    for path in sorted(folder.glob("*.png")):
        clean = np.asarray(Image.open(path).convert("RGB"), dtype=np.float32) / 255.0
        versions = [compress(clean, q) for q in QUALITIES]
        (val if path.stem in VALIDATION else train).append((clean, versions))
    return train, val


def patches(images: list, size: int, count: int, rng: random.Random):
    """Random crops of (compressed, clean), with flips and rotations.

    The crop offset is deliberately *not* a multiple of eight: at run time
    the filter has no idea where the block grid is.
    """
    src = np.empty((count, size, size, 3), dtype=np.float32)
    dst = np.empty((count, size, size, 3), dtype=np.float32)
    for i in range(count):
        clean, versions = images[rng.randrange(len(images))]
        bad = versions[rng.randrange(len(versions))]
        y = rng.randrange(clean.shape[0] - size)
        x = rng.randrange(clean.shape[1] - size)
        a = bad[y : y + size, x : x + size]
        b = clean[y : y + size, x : x + size]
        if rng.random() < 0.5:
            a, b = a[:, ::-1], b[:, ::-1]
        if rng.random() < 0.5:
            a, b = a[::-1, :], b[::-1, :]
        if rng.random() < 0.5:
            a, b = a.transpose(1, 0, 2), b.transpose(1, 0, 2)
        src[i], dst[i] = a, b
    to_t = lambda a: torch.from_numpy(a).permute(0, 3, 1, 2).contiguous()
    return to_t(src), to_t(dst)


def psnr(a: torch.Tensor, b: torch.Tensor) -> float:
    mse = F.mse_loss(a.clamp(0, 1), b).item()
    return 99.0 if mse <= 1e-12 else 10.0 * math.log10(1.0 / mse)


def evaluate(model: nn.Module, images: list) -> list[tuple[int, float, float]]:
    """(quality, JPEG PSNR, model PSNR) over the held-out images."""
    model.eval()
    rows = []
    with torch.no_grad():
        for qi, q in enumerate(QUALITIES):
            base, ours = [], []
            for clean, versions in images:
                t = torch.from_numpy(clean).permute(2, 0, 1).unsqueeze(0)
                d = torch.from_numpy(versions[qi]).permute(2, 0, 1).unsqueeze(0)
                base.append(psnr(d, t))
                ours.append(psnr(model(d), t))
            rows.append((q, sum(base) / len(base), sum(ours) / len(ours)))
    model.train()
    return rows


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--steps", type=int, default=12000)
    ap.add_argument("--batch", type=int, default=16)
    ap.add_argument("--patch", type=int, default=64)
    ap.add_argument("--channels", type=int, default=48)
    ap.add_argument("--layers", type=int, default=5)
    ap.add_argument("--lr", type=float, default=1e-3)
    ap.add_argument("--seed", type=int, default=7)
    args = ap.parse_args()

    torch.manual_seed(args.seed)
    rng = random.Random(args.seed)

    train_imgs, val_imgs = load_images(args.data)
    if not train_imgs:
        raise SystemExit(f"no training images in {args.data}")
    print(f"{len(train_imgs)} training images, {len(val_imgs)} held out")

    model = DeblockNet(args.channels, args.layers)
    params = sum(p.numel() for p in model.parameters())
    print(f"{params} parameters ({params * 4 / 1024:.0f} KB as float32)")

    opt = torch.optim.Adam(model.parameters(), lr=args.lr)
    sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, T_max=args.steps)

    started = time.time()
    for step in range(1, args.steps + 1):
        bad, good = patches(train_imgs, args.patch, args.batch, rng)
        # L1 for the same reason super-resolution work uses it: squared
        # error would rather blur the whole patch than be wrong anywhere.
        loss = F.l1_loss(model(bad), good)
        opt.zero_grad(set_to_none=True)
        loss.backward()
        opt.step()
        sched.step()
        if step % 500 == 0 or step == 1:
            print(f"  step {step:5d}  loss {loss.item():.5f}  {time.time() - started:5.0f}s",
                  flush=True)

    if val_imgs:
        print("held-out PSNR by quality:")
        for q, base, ours in evaluate(model, val_imgs):
            print(f"  q{q:<3d} JPEG {base:.2f} dB -> model {ours:.2f} dB ({ours - base:+.2f})")

    # Dynamic height and width: the app picks the tile size, not this.
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
