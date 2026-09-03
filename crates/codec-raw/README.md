# schist-codec-raw

Camera raw files, decoded and developed in pure Rust, written clean-room
from the public DNG, TIFF/EP, EXIF and ITU T.81 specifications, published
format descriptions, and observation of real files against LibRaw's
`unprocessed_raw` output. Nothing in it derives from dcraw, LibRaw,
rawspeed, rawler or any other copyleft decoder: where a codec has no
public description (Canon's CRX and compressed CRW, Fuji's compressed
RAF, Samsung's NX1 scheme, Phase One's IIQ S, Panasonic's RawFormat 8),
it was implemented from a written functional specification produced by
a separate party who read the reference — the standard clean-room
arrangement — and verified against the reference's output only.

`decode` reads a file into a `RawImage` (the sensor frame, its filter
array, levels, white balance, colour matrix, crop, orientation and the
camera's embedded JPEG); `develop` turns that into linear sRGB. `probe`
names the container without decoding, `preview` finds the JPEG cheaply,
`orientation` reads the tag cheaply.

## Coverage

"Exact" means every sample of the decoded frame matched LibRaw's unpacked
frame on the files listed, drawn from the raw.pixls.us sample set.

| container | status |
| --- | --- |
| DNG (incl. ProRAW, Pixel, Leica, Pentax, Ricoh, Sigma, DJI, Hasselblad) | exact on 26; uncompressed, lossless JPEG, deflate, lossy JPEG, float; GoPro VC-5 and JPEG XL unsupported |
| Sony ARW / SR2 / SRF | exact on 24, every generation incl. ARW 1.0 and ARW 4 lossless |
| Nikon NEF / NRW | exact on 36 (20 bodies); Z 8/9 High Efficiency unsupported |
| Canon CR2 | exact on 14; sRAW/mRAW unsupported |
| Canon CRW | exact on 4 (compressed and uncompressed) |
| Canon CR3 | exact on 10: CRX lossless and lossy (cRAW), EOS R through R8, dual-pixel included |
| Fujifilm RAF | exact on 13: uncompressed, lossless and lossy compressed, X-Trans and Bayer; SuperCCD unsupported |
| Olympus / OM ORF | exact on 12, four sensor layouts |
| Panasonic RW2 / Leica RWL | exact on 31, RawFormat 4 through 8 |
| Pentax PEF | exact on 10 |
| Samsung SRW | exact on 17, every compression |
| Minolta MRW, Kodak DCR/KDC, Epson ERF, Mamiya MEF | exact on 15 bodies; Kodak DC50 unsupported; Epson's as-shot balance is not found in the file |
| Hasselblad 3FR / FFF | exact on 7 |
| Phase One IIQ | exact on 11, raw, "IIQ L" and both "IIQ S" formats |
| Leaf MOS | exact on 3 |
| Sigma X3F | metadata and preview only |

Colour needs a matrix. DNG carries its own; the other formats look one
up in `cameras.rs` (189 bodies, each entry naming its source). A camera
with no entry develops in camera RGB, and the Schist plugin prefers
LibRaw for those.

## Verifying

`SCHIST_RAW_CORPUS=<dir> cargo test --release -p schist-codec-raw` runs
every module's corpus test over a directory of raws with LibRaw oracle
sidecars beside them (`<file>.tiff` from `unprocessed_raw -T`,
`<file>.identify.txt` from `raw-identify -v -w`, `<file>.json` from
`exiftool -G -a -u -j`). `cargo run --release -p schist-codec-raw
--example rawinfo -- <file>` prints what `decode` makes of a file.
