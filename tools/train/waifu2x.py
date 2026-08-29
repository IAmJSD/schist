#!/usr/bin/env python3
"""Convert waifu2x's upconv_7 2x upscalers to the ONNX the app embeds.

Nothing is trained here. nagadomi released the waifu2x models under the
MIT licence with the weights as JSON dumps of the Torch7 graphs; this
script fetches a pinned revision of two of them -- the "art" model the
project is named for and the "photo" one trained on photographs --
rebuilds the seven-layer graph in PyTorch, loads the weights verbatim and
exports ONNX.

One deliberate change to the graph: upconv_7 is all valid (unpadded)
convolutions, so it eats a 7-pixel border and returns 2N-28 pixels for N
in. Seven pixels of replicate padding are baked in front of the first
convolution so a tile in is exactly a doubled tile out, which is the
contract `schist_neural::run_scaled` tiles against. Replication is the
right pad for the job for the same reason the tile driver mirrors at
image edges: a network fed a black border draws one.

Usage:  python3 tools/train/waifu2x.py --out-dir crates/neural/models
        python3 tools/train/waifu2x.py --check photo.png   # also score it
"""

import argparse
import hashlib
import json
import urllib.request
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn
import torch.nn.functional as F

# The waifu2x revision the weights come from, so a re-run converts the
# same bytes it did the first time. The JSON hashes make sure of it.
REVISION = "cc385f97a9debfe611316aabfd5d8bb30ba2dbeb"
VARIANTS = {
    "art": "8e5e66b0826607d29c038d5b8399b71fe9cc23950eb5ec57be5ab1f865f1ca31",
    "photo": "deb2c288f635bed4a0ffc0be9ca8f6a735233b530ad5b2c392c5b2e6c5fe30ad",
}
URL = (
    "https://raw.githubusercontent.com/nagadomi/waifu2x/{rev}"
    "/models/upconv_7/{variant}/scale2.0x_model.json"
)

# Valid convolutions lose 14 pixels of each side's output; 7 of input
# padding puts them back (model_config says offset=14, at output scale).
PAD = 7


class Upconv7(nn.Module):
    """upconv_7 as waifu2x defines it, with the padding baked in.

    Six unpadded 3x3 convolutions with LeakyReLU(0.1) between, then a
    4x4 stride-2 transposed convolution up to twice the size. The
    channel widths come from the weights themselves.
    """

    def __init__(self, layers: list[dict]) -> None:
        super().__init__()
        body: list[nn.Module] = []
        for spec in layers:
            cin, cout = spec["nInputPlane"], spec["nOutputPlane"]
            w = torch.tensor(np.asarray(spec["weight"], dtype=np.float32))
            b = torch.tensor(np.asarray(spec["bias"], dtype=np.float32))
            if spec["class_name"] == "nn.SpatialConvolutionMM":
                conv = nn.Conv2d(cin, cout, (spec["kH"], spec["kW"]))
                body += [conv, nn.LeakyReLU(0.1)]
            elif spec["class_name"] == "nn.SpatialFullConvolution":
                # Torch's full convolution and PyTorch's ConvTranspose2d
                # share the (in, out, kH, kW) weight layout.
                conv = nn.ConvTranspose2d(
                    cin, cout, (spec["kH"], spec["kW"]),
                    stride=(spec["dH"], spec["dW"]),
                    padding=(spec["padH"], spec["padW"]),
                )
                body.append(conv)  # nothing after the last layer
            else:
                raise SystemExit(f"unexpected layer {spec['class_name']}")
            with torch.no_grad():
                conv.weight.copy_(w.reshape(conv.weight.shape))
                conv.bias.copy_(b)
        self.body = nn.Sequential(*body)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        return self.body(F.pad(x, (PAD,) * 4, mode="replicate"))


def fetch(variant: str, cache: Path) -> list[dict]:
    path = cache / f"waifu2x-{variant}-{REVISION[:12]}.json"
    if not path.exists():
        url = URL.format(rev=REVISION, variant=variant)
        print(f"fetching {url}")
        path.parent.mkdir(parents=True, exist_ok=True)
        with urllib.request.urlopen(url) as r:
            path.write_bytes(r.read())
    data = path.read_bytes()
    got = hashlib.sha256(data).hexdigest()
    if got != VARIANTS[variant]:
        raise SystemExit(f"{path}: sha256 {got}, wanted {VARIANTS[variant]}")
    return json.loads(data)


def psnr(a: torch.Tensor, b: torch.Tensor) -> float:
    return -10.0 * torch.log10(F.mse_loss(a.clamp(0, 1), b.clamp(0, 1))).item()


def check(model: nn.Module, image: Path) -> None:
    """Halve an image, double it back, and compare against bicubic."""
    from PIL import Image

    img = Image.open(image).convert("RGB")
    img = img.crop((0, 0, img.width & ~3, img.height & ~3))
    full = torch.tensor(np.asarray(img, dtype=np.float32) / 255.0)
    full = full.permute(2, 0, 1).unsqueeze(0)
    small = F.avg_pool2d(full, 2)
    with torch.no_grad():
        ours = model(small)
    bicubic = F.interpolate(small, scale_factor=2, mode="bicubic")
    print(f"  {image.name}: bicubic {psnr(bicubic, full):.2f} dB, "
          f"waifu2x {psnr(ours, full):.2f} dB")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out-dir", type=Path, default=Path("crates/neural/models"))
    ap.add_argument("--cache", type=Path, default=Path("/tmp/waifu2x-weights"))
    ap.add_argument("--check", type=Path, help="image to score the models on")
    args = ap.parse_args()

    for variant in VARIANTS:
        layers = fetch(variant, args.cache)
        config = layers[0]["model_config"]
        assert config["arch_name"] == "upconv_7", config
        assert config["offset"] == 2 * PAD, config
        model = Upconv7(layers).eval()

        # The whole point of the padding: a tile comes back exactly doubled.
        with torch.no_grad():
            out = model(torch.rand(1, 3, 40, 56))
        assert out.shape == (1, 3, 80, 112), out.shape

        if args.check:
            check(model, args.check)

        out_path = args.out_dir / f"waifu2x-{variant}.onnx"
        out_path.parent.mkdir(parents=True, exist_ok=True)
        torch.onnx.export(
            model,
            (torch.zeros(1, 3, 128, 128),),
            str(out_path),
            input_names=["input"],
            output_names=["output"],
            dynamic_axes={"input": {2: "h", 3: "w"}, "output": {2: "h", 3: "w"}},
            opset_version=11,
            dynamo=False,
        )
        data = out_path.read_bytes()
        print(f"wrote {out_path} ({len(data)} bytes, "
              f"sha256 {hashlib.sha256(data).hexdigest()})")


if __name__ == "__main__":
    main()
