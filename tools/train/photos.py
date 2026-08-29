#!/usr/bin/env python3
"""Fetch a photograph corpus for training the colourisation network.

A colouriser is only as good as the photographs it was shown, and the
photographs it was shown have to be ones anybody can fetch and nobody has
to ask about. Open Images is exactly that: every image in it is licensed
CC BY 2.0 -- the list is checked here rather than assumed -- and the
index carries the author and the link back to the original, which are
written to `credits.csv` beside the download.

The index is Google's; the pictures themselves come from the Flickr CDN
that has always served them. What is fetched is the ~640-pixel preview
each entry carries, not the original: the network sees 256x256, and a
20-megapixel original would be four hundred times the download for
something that gets resampled away.

The validation split is used rather than the training one because it is
41,620 images in one 15 MB index instead of nine million in a gigabyte
of them, and nothing here needs nine million.

Usage:  python3 tools/train/photos.py --out <dir> --count 20000
"""

import argparse
import csv
import io
import random
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

INDEX = (
    "https://storage.googleapis.com/openimages/2018_04/validation/"
    "validation-images-with-rotation.csv"
)
LICENCE = "https://creativecommons.org/licenses/by/2.0/"
AGENT = "schist-training/1.0 (https://github.com/IAmJSD/schist)"


def index(cache: Path) -> list[dict]:
    if not cache.exists():
        print(f"fetching the index into {cache}")
        req = urllib.request.Request(INDEX, headers={"User-Agent": AGENT})
        with urllib.request.urlopen(req, timeout=300) as r:
            cache.write_bytes(r.read())
    with cache.open(newline="") as f:
        rows = list(csv.DictReader(f))
    # Belt and braces: the whole point of this source is the licence, so
    # anything that is not the licence we expect is dropped rather than
    # trusted.
    return [r for r in rows if r["License"] == LICENCE and r["Thumbnail300KURL"]]


def fetch(row: dict, out: Path) -> dict | None:
    path = out / f"{row['ImageID']}.jpg"
    if path.exists() and path.stat().st_size > 0:
        return row
    try:
        req = urllib.request.Request(row["Thumbnail300KURL"], headers={"User-Agent": AGENT})
        with urllib.request.urlopen(req, timeout=60) as r:
            data = r.read()
    except Exception as e:
        print(f"  {row['ImageID']}: {e}")
        return None
    # A truncated JPEG is worse than a missing one: it decodes, so it
    # would train the network on half a photograph and a grey band.
    try:
        from PIL import Image

        Image.open(io.BytesIO(data)).convert("RGB").load()
    except Exception as e:
        print(f"  {row['ImageID']}: {e}")
        return None
    path.write_bytes(data)
    return row


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--count", type=int, default=20000)
    ap.add_argument("--jobs", type=int, default=12)
    ap.add_argument("--seed", type=int, default=7)
    args = ap.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)
    rows = index(args.out / "index.csv")
    print(f"{len(rows)} CC BY 2.0 photographs in the index")
    # Deterministic sample, so a re-run fetches the same corpus.
    rows.sort(key=lambda r: r["ImageID"])
    random.Random(args.seed).shuffle(rows)
    rows = rows[: args.count]

    got = []
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for i, row in enumerate(pool.map(lambda r: fetch(r, args.out), rows)):
            if row:
                got.append(row)
            if (i + 1) % 500 == 0:
                print(f"  {i + 1}/{len(rows)}", flush=True)

    with (args.out / "credits.csv").open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["file", "author", "licence", "source"])
        for r in got:
            w.writerow(
                [f"{r['ImageID']}.jpg", r["Author"], LICENCE, r["OriginalLandingURL"]]
            )
    print(f"{len(got)} images in {args.out}")


if __name__ == "__main__":
    main()
