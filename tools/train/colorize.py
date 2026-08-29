#!/usr/bin/env python3
"""Train the Colorize network and export it to ONNX.

Colourisation is the one filter in the set that cannot be signal
processing. Nothing in a greyscale photograph says that the grass is
green -- that is knowledge about the world, and the only way a filter
gets it is by having been shown a lot of grass.

The network is given luminance and predicts chroma, `R - Y` and `B - Y`.
That is the standard formulation and it is not an optimisation: the
luminance is already right, so a network asked to output a whole image
spends most of itself copying its own input, and any error it makes there
comes back as a photograph that got softer.

**It predicts a distribution over colours rather than a colour**, which is
the difference between this and a sepia filter. Asked for one number, a
network that cannot tell whether the car is red or blue answers "brown",
because the average of red and blue scores better than either guess --
and a colouriser trained that way makes everything brown. Asked instead
which of 249 binned colours it is, it can say "red or blue, not brown",
and the annealed mean of that distribution picks a side. The bins are
weighted so that rare, saturated colours count for more than the muddy
ones that dominate photographs, which is Zhang et al.'s class rebalancing
and is what stops the distribution collapsing towards grey anyway.

The architecture is an encoder-decoder with skips. The encoder's job is
to work out *what things are*, which needs to see far and does not need
resolution; the decoder's is to put the answer back where it belongs,
which is what the skips are for. Chroma comes out at a quarter of the
input's size, because chroma is low-frequency -- the app resamples it up
against the luminance it already had.

Trained on photographs from Open Images, every one CC BY 2.0; fetch them
with `tools/train/photos.py`.

Usage:  python3 tools/train/colorize.py --data <dir of jpegs> --out <model.onnx>
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

# Rec.601, matching `luma` in the filter set. The network has to predict
# chroma against the same luminance the app will recombine it with.
LUMA = (0.299, 0.587, 0.114)

# Images are stored at this size and cropped to `--patch` for training.
STORE = 288

# A photograph with less colour in it than this is a black-and-white or a
# sepia one, and it is *poison*: it teaches the network that the correct
# answer is no colour, on exactly the kind of picture this filter exists
# to be pointed at.
MIN_CHROMA = 0.02

# The colour bins: a grid over (R-Y, B-Y) at this spacing, out to this
# much chroma. Finer bins would be a better vocabulary and a worse
# classifier -- there are only so many examples of each.
BIN_STEP = 0.075
BIN_MAX = 0.6

# Temperature for the annealed mean at inference. At 1.0 the mean of the
# distribution is the same desaturated average regression would have
# given; at 0 it is the single most likely bin, which flickers between
# neighbours. Zhang et al. land on about a third and so does this.
TEMPERATURE = 0.4


def conv(cin: int, cout: int, stride: int = 1, dilation: int = 1) -> nn.Sequential:
    return nn.Sequential(
        nn.Conv2d(cin, cout, 3, stride=stride, padding=dilation, dilation=dilation, bias=False),
        nn.BatchNorm2d(cout),
        nn.ReLU(inplace=True),
    )


class ColorNet(nn.Module):
    """Encoder, dilated middle, decoder with skips, and a colour vocabulary.

    Sizes for the default 256-pixel input: the encoder takes it down to
    16x16, where a 3x3 convolution spans a sixth of the picture and the
    network can afford to be wide; the decoder brings it back to 64x64,
    which is where the chroma is emitted.

    The annealed mean is part of the graph rather than something the
    application does, so what the app loads is a model that takes
    luminance and returns colour, with nothing to get wrong at the seam.
    It is a softmax and a 1x1 convolution whose weights are the bin
    centres -- an expectation, written as a layer.
    """

    def __init__(self, bins: torch.Tensor, base: int = 24, temperature: float = TEMPERATURE):
        super().__init__()
        c1, c2, c3, c4 = base, base * 2, base * 3, base * 4
        self.head = conv(3, c1, stride=2)  # 128
        self.enc1 = conv(c1, c2, stride=2)  # 64
        self.enc2 = conv(c2, c3, stride=2)  # 32
        self.enc3 = conv(c3, c4, stride=2)  # 16
        # Dilated rather than another stride: the receptive field keeps
        # growing but 16x16 is already small enough to lose things in.
        self.mid1 = conv(c4, c4, dilation=2)
        self.mid2 = conv(c4, c4, dilation=2)
        self.dec1 = conv(c4 + c3, c3)  # 32
        self.dec2 = conv(c3 + c2, c2)  # 64
        self.refine = conv(c2, c2)
        self.logits = nn.Conv2d(c2, bins.shape[0], 1)
        # Start with no opinion: a uniform distribution, whose mean colour
        # is the middle of the bin grid, which is grey. The network then
        # learns colour as something it adds.
        nn.init.zeros_(self.logits.weight)
        nn.init.zeros_(self.logits.bias)
        self.mean = nn.Conv2d(bins.shape[0], 2, 1, bias=False)
        with torch.no_grad():
            self.mean.weight.copy_(bins.t().reshape(2, -1, 1, 1))
        self.mean.weight.requires_grad_(False)
        self.register_buffer("bins", bins)
        self.temperature = temperature

    def features(self, x: torch.Tensor) -> torch.Tensor:
        """The per-bin scores, which is what the loss is computed on."""
        h1 = self.head(x)
        h2 = self.enc1(h1)
        h3 = self.enc2(h2)
        h4 = self.mid2(self.mid1(self.enc3(h3)))
        # Nearest-neighbour upsampling followed by a convolution rather
        # than a transposed convolution: no checkerboard, and it is an
        # operator every runtime implements the same way.
        u = self.dec1(torch.cat([F.interpolate(h4, scale_factor=2, mode="nearest"), h3], 1))
        u = self.dec2(torch.cat([F.interpolate(u, scale_factor=2, mode="nearest"), h2], 1))
        return self.logits(self.refine(u))

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        """What the application runs: luminance in, chroma out."""
        p = F.softmax(self.features(x) / self.temperature, dim=1)
        return self.mean(p)


def make_bins() -> torch.Tensor:
    """The colour vocabulary: a grid, with the corners no colour reaches
    dropped."""
    vals = np.arange(-BIN_MAX, BIN_MAX + 1e-6, BIN_STEP, dtype=np.float32)
    grid = np.array([(a, b) for a in vals for b in vals], dtype=np.float32)
    keep = np.abs(grid[:, 0]) + np.abs(grid[:, 1]) <= BIN_MAX * 1.6
    return torch.from_numpy(grid[keep])


def split(rgb: torch.Tensor) -> tuple[torch.Tensor, torch.Tensor]:
    """An image to (luminance replicated to three channels, chroma)."""
    y = LUMA[0] * rgb[:, 0:1] + LUMA[1] * rgb[:, 1:2] + LUMA[2] * rgb[:, 2:3]
    chroma = torch.cat([rgb[:, 0:1] - y, rgb[:, 2:3] - y], 1)
    return y.repeat(1, 3, 1, 1), chroma


def load_images(folder: Path, limit: int | None) -> list[np.ndarray]:
    """Every photograph, square, small, and in colour."""
    kept, skipped = [], 0
    paths = sorted(p for p in folder.iterdir() if p.suffix.lower() in {".jpg", ".jpeg", ".png"})
    for path in paths[:limit] if limit else paths:
        try:
            img = Image.open(path).convert("RGB")
        except Exception:
            continue
        # Short side to STORE, then a centre crop: the subject of a
        # photograph is in the middle of it far more often than not.
        w, h = img.size
        s = STORE / min(w, h)
        img = img.resize((max(STORE, round(w * s)), max(STORE, round(h * s))), Image.BICUBIC)
        w, h = img.size
        left, top = (w - STORE) // 2, (h - STORE) // 2
        arr = np.asarray(img.crop((left, top, left + STORE, top + STORE)), dtype=np.uint8)
        f = arr.astype(np.float32) / 255.0
        y = f @ np.array(LUMA, dtype=np.float32)
        if float(np.abs(f - y[..., None]).mean()) < MIN_CHROMA:
            skipped += 1
            continue
        kept.append(arr)
    print(f"{len(kept)} photographs, {skipped} skipped for having no colour in them", flush=True)
    return kept


def batch(images: list[np.ndarray], size: int, count: int, rng: random.Random) -> torch.Tensor:
    out = np.empty((count, size, size, 3), dtype=np.float32)
    for i in range(count):
        img = images[rng.randrange(len(images))]
        y = rng.randrange(img.shape[0] - size + 1)
        x = rng.randrange(img.shape[1] - size + 1)
        p = img[y : y + size, x : x + size]
        if rng.random() < 0.5:
            p = p[:, ::-1]
        out[i] = p.astype(np.float32) / 255.0
    return torch.from_numpy(out).permute(0, 3, 1, 2).contiguous()


def soft_targets(chroma: torch.Tensor, bins: torch.Tensor, sigma: float = 0.06):
    """Each pixel's colour as a distribution over the nearest bins.

    Soft rather than one-hot because the bins are arbitrary: a colour a
    hair inside one bin is nearly the neighbouring one too, and training
    against a hard label would punish the network for a rounding
    decision it had no way to make.
    """
    n, _, h, w = chroma.shape
    flat = chroma.permute(0, 2, 3, 1).reshape(-1, 2)
    d2 = torch.cdist(flat, bins) ** 2
    val, idx = torch.topk(-d2, 5, dim=1)
    wgt = torch.exp(val / (2 * sigma * sigma))
    wgt = wgt / wgt.sum(1, keepdim=True)
    target = torch.zeros(flat.shape[0], bins.shape[0], device=chroma.device)
    target.scatter_(1, idx, wgt)
    return target.reshape(n, h, w, -1).permute(0, 3, 1, 2), idx[:, 0].reshape(n, h, w)


def bin_frequencies(images, bins: torch.Tensor, rng: random.Random, samples: int = 60):
    """How often each binned colour actually occurs in the corpus."""
    counts = torch.zeros(bins.shape[0])
    for _ in range(samples):
        _, chroma = split(batch(images, 128, 8, rng))
        small = F.interpolate(chroma, scale_factor=0.5, mode="area")
        _, nearest = soft_targets(small, bins)
        counts += torch.bincount(nearest.reshape(-1), minlength=bins.shape[0]).float()
    return counts / counts.sum()


def rebalance(p: torch.Tensor, lam: float = 0.5) -> torch.Tensor:
    """Class rebalancing: what a pixel of each colour is worth.

    Photographs are mostly desaturated, so an unweighted loss is mostly
    about being right on the muddy colours -- and the network obliges by
    never predicting anything else. Mixing the empirical distribution with
    a uniform one and inverting gives the rare, saturated bins the weight
    they need to be worth predicting.
    """
    q = 1.0 / (lam * p + (1 - lam) / p.shape[0])
    return q / (p * q).sum()


def evaluate(model: nn.Module, images, patch: int, rng: random.Random):
    """Mean chroma error, how much colour the network dares to use, and
    how much there was.

    The middle number is the one to watch, and the first one is a trap:
    predicting grey everywhere scores *well* on error, because the average
    photograph is not very colourful and being timid is never very wrong.
    A colouriser is doing its job when the colourfulness ratio is near
    one and the error is no worse than grey's -- which is the same thing
    every paper on this says in more words.
    """
    model.eval()
    errs, ours, theirs = [], [], []
    with torch.no_grad():
        for i in range(0, len(images), 16):
            rgb = batch(images[i : i + 16], patch, min(16, len(images) - i), rng)
            grey, chroma = split(rgb)
            pred = model(grey)
            small = F.interpolate(chroma, size=pred.shape[-2:], mode="area")
            errs.append((pred - small).abs().mean().item())
            ours.append(pred.abs().mean().item())
            theirs.append(small.abs().mean().item())
    model.train()
    return (
        sum(errs) / len(errs),
        sum(ours) / max(sum(theirs), 1e-6),
        sum(theirs) / len(theirs),
    )


def export(model: nn.Module, path: Path, patch: int) -> None:
    """Write the ONNX the app loads.

    Called at every checkpoint rather than only at the end: these runs
    take hours, and one that gets interrupted should still leave
    something behind that can be looked at.
    """
    was_training = model.training
    model.eval()
    path.parent.mkdir(parents=True, exist_ok=True)
    # The weights as well as the graph. The temperature is baked into the
    # export but not into the training, so this is what lets a finished
    # run be re-exported at another one without spending the hours again.
    torch.save(model.state_dict(), path.with_suffix(".pt"))
    torch.onnx.export(
        model,
        (torch.zeros(1, 3, patch, patch),),
        str(path),
        input_names=["input"],
        output_names=["chroma"],
        opset_version=11,
        dynamo=False,
    )
    if was_training:
        model.train()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--steps", type=int, default=12000)
    ap.add_argument("--batch", type=int, default=12)
    ap.add_argument("--patch", type=int, default=256)
    ap.add_argument("--base", type=int, default=24)
    # Baked into the exported graph, so changing it means
    # retraining or at least re-exporting: lower is more
    # colourful and more willing to be wrong.
    ap.add_argument("--temperature", type=float, default=TEMPERATURE)
    ap.add_argument("--lr", type=float, default=2e-3)
    ap.add_argument("--limit", type=int, default=None)
    ap.add_argument("--holdout", type=int, default=200)
    ap.add_argument("--seed", type=int, default=7)
    args = ap.parse_args()

    torch.manual_seed(args.seed)
    rng = random.Random(args.seed)

    images = load_images(args.data, args.limit)
    if len(images) < args.holdout + 10:
        raise SystemExit(f"only {len(images)} usable images in {args.data}")
    rng.shuffle(images)
    val_imgs, train_imgs = images[: args.holdout], images[args.holdout :]
    print(f"{len(train_imgs)} for training, {len(val_imgs)} held out", flush=True)

    bins = make_bins()
    print(f"{bins.shape[0]} colour bins", flush=True)
    weights = rebalance(bin_frequencies(train_imgs, bins, random.Random(3)))
    print(f"weights {weights.min():.2f}..{weights.max():.2f}", flush=True)

    model = ColorNet(bins, args.base, args.temperature)
    params = sum(p.numel() for p in model.parameters())
    print(f"{params} parameters ({params * 4 / 1024:.0f} KB as float32)", flush=True)

    opt = torch.optim.Adam(model.parameters(), lr=args.lr)
    sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, T_max=args.steps)

    started = time.time()
    for step in range(1, args.steps + 1):
        grey, chroma = split(batch(train_imgs, args.patch, args.batch, rng))
        logits = model.features(grey)
        # The target is the average colour of the patch each score
        # covers, which is what the network is in a position to know.
        small = F.interpolate(chroma, size=logits.shape[-2:], mode="area")
        target, nearest = soft_targets(small, bins)
        cross_entropy = -(target * F.log_softmax(logits, dim=1)).sum(1)
        loss = (cross_entropy * weights[nearest]).mean()
        opt.zero_grad(set_to_none=True)
        loss.backward()
        opt.step()
        sched.step()
        if step % 250 == 0 or step == 1:
            print(f"  step {step:5d}  loss {loss.item():.5f}  {time.time() - started:5.0f}s",
                  flush=True)
        if step % 1000 == 0:
            err, colour, real = evaluate(model, val_imgs, args.patch, random.Random(1))
            print(f"    held out: chroma error {err:.4f} against {real:.4f} of colour "
                  f"to get right, colourfulness {colour:.2f}x", flush=True)
            export(model, args.out, args.patch)

    err, colour, real = evaluate(model, val_imgs, args.patch, random.Random(1))
    print(f"held-out chroma error {err:.4f} against {real:.4f} of colour to get right, "
          f"colourfulness {colour:.2f} of the real thing")
    export(model, args.out, args.patch)
    print(f"wrote {args.out} ({args.out.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
