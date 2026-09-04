//! Decoding a "DyBm" bitmap: channel formats, tile status grids, and
//! rebuilding each channel plane from its blocks.

use super::*;

/// Channel layout of a "DyBm" by its `Frmt` enum id.
pub(super) struct Format {
    bytes_per_sample: usize,
    channels: usize,
    kind: FormatKind,
}

pub(super) enum FormatKind {
    Rgba,
    Gray,
    Cmyk,
    Lab,
    /// One channel; decodes to (v, v, v, opaque).
    Mask,
}

pub(super) fn format(id: u16) -> Option<Format> {
    let (bytes_per_sample, channels, kind) = match id {
        0 => (1, 4, FormatKind::Rgba),
        1 => (2, 4, FormatKind::Rgba),
        2 => (1, 2, FormatKind::Gray),
        3 => (2, 2, FormatKind::Gray),
        4 => (1, 5, FormatKind::Cmyk),
        5 => (2, 4, FormatKind::Lab),
        // Single 8-bit channel: layer masks, and Photo 2's (usually
        // evicted) composite caches.
        6 => (1, 1, FormatKind::Mask),
        9 => (4, 4, FormatKind::Rgba),
        _ => return None,
    };
    Some(Format {
        bytes_per_sample,
        channels,
        kind,
    })
}

/// Every tile status of every channel, flattened; missing `Sta` fields
/// read as empty.
pub(super) fn all_statuses(bitm: &Node) -> Vec<u8> {
    let mut out = Vec::new();
    for sta in [b"Sta1", b"Sta2", b"Sta3", b"Sta4", b"Sta5"] {
        if let Some(Value::Array(items)) = bitm.field(sta) {
            out.extend(items.iter().filter_map(|v| match v {
                Value::U8(b) => Some(*b),
                _ => None,
            }));
        }
    }
    out
}

/// True when a bitmap stores or synthesizes any pixel at all. Photo 2
/// keeps evicted composite caches around (all statuses 0) — those are
/// absence, not content.
pub(super) fn bitmap_has_content(bitm: &Node) -> bool {
    all_statuses(bitm).iter().any(|&s| s > 1)
}

pub(super) fn decode_bitmap(
    archive: &Archive,
    graph: &Graph,
    bitm: &Node,
) -> Result<RgbaImage, AffinityError> {
    let frmt = enum_of(bitm, b"Frmt").ok_or_else(|| malformed("bitmap has no format"))?;
    let fmt = format(frmt).ok_or_else(|| malformed(format!("unknown pixel format {frmt}")))?;
    let width = i32_of(bitm, b"BmpW").unwrap_or(0);
    let height = i32_of(bitm, b"BmpH").unwrap_or(0);
    if width <= 0 || height <= 0 || width > 1 << 20 || height > 1 << 20 {
        return Err(malformed(format!("implausible bitmap {width}×{height}")));
    }
    check_pixel_count(width as u64, height as u64, "bitmap")?;
    let (width, height) = (width as usize, height as usize);

    let row_bytes = width * fmt.bytes_per_sample;
    let pitch = row_bytes.div_ceil(256) * 256;
    let rows = height.div_ceil(256) * 256;

    // Placed images don't duplicate their pixels: tiles with status 5
    // pull from the original file, carried in the Bckg entry. A fully
    // evicted bitmap (no status arrays at all) that still has its
    // source *is* the source, wholesale.
    let statuses = all_statuses(bitm);
    if statuses.is_empty() && bitm.field(b"Bckg").is_some() {
        return source_image(archive, bitm, width, height);
    }
    let source = if statuses.contains(&5) {
        Some(source_image(archive, bitm, width, height)?)
    } else {
        None
    };

    let sta_names: [&[u8; 4]; 5] = [b"Sta1", b"Sta2", b"Sta3", b"Sta4", b"Sta5"];
    let idx_names: [&[u8; 4]; 5] = [b"Idx1", b"Idx2", b"Idx3", b"Idx4", b"Idx5"];
    let twi_names: [&[u8; 4]; 5] = [b"TWi1", b"TWi2", b"TWi3", b"TWi4", b"TWi5"];
    let planes = (0..fmt.channels)
        .into_par_iter()
        .map(|channel| {
            load_plane(PlaneJob {
                archive,
                graph,
                bitm,
                sta: sta_names[channel],
                idx: idx_names[channel],
                // Affinity rounds the tile grid up past the pixels it
                // needs, so the status array's row stride is the declared
                // `TWi`, not `ceil(row_bytes / 256)`. Reading it as the
                // tight grid shears every row after the first.
                grid_width: i32_of(bitm, twi_names[channel])
                    .filter(|w| *w > 0)
                    .map_or(row_bytes.div_ceil(256), |w| w as usize),
                pitch,
                rows,
                height,
                bytes_per_sample: fmt.bytes_per_sample,
                source: source.as_ref().map(|s| (s, channel)),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Interleave planes into straight-alpha RGBA8. Higher depths are
    // reduced to 8 bits here; precision, not placement, is what's lost.
    let sample = |plane: &[u8], x: usize, y: usize| -> f32 {
        let at = y * pitch + x * fmt.bytes_per_sample;
        match fmt.bytes_per_sample {
            1 => plane[at] as f32 / 255.0,
            2 => u16::from_le_bytes([plane[at], plane[at + 1]]) as f32 / 65535.0,
            _ => f32::from_le_bytes(plane[at..at + 4].try_into().unwrap()).clamp(0.0, 1.0),
        }
    };

    let mut pixels = vec![0u8; width * height * 4];
    match (fmt.bytes_per_sample, &fmt.kind) {
        // 8-bit samples map to output bytes unchanged; interleave the
        // planes directly instead of round-tripping through f32.
        (1, FormatKind::Rgba) => {
            pixels
                .par_chunks_exact_mut(width * 4)
                .enumerate()
                .for_each(|(y, out_row)| {
                    let at = y * pitch;
                    let (r, g, b, a) = (
                        &planes[0][at..at + width],
                        &planes[1][at..at + width],
                        &planes[2][at..at + width],
                        &planes[3][at..at + width],
                    );
                    for (x, px) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        *px = [r[x], g[x], b[x], a[x]];
                    }
                });
            return Ok(RgbaImage {
                width: width as u32,
                height: height as u32,
                pixels,
            });
        }
        (1, FormatKind::Gray) => {
            pixels
                .par_chunks_exact_mut(width * 4)
                .enumerate()
                .for_each(|(y, out_row)| {
                    let at = y * pitch;
                    let (g, a) = (&planes[0][at..at + width], &planes[1][at..at + width]);
                    for (x, px) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        *px = [g[x], g[x], g[x], a[x]];
                    }
                });
            return Ok(RgbaImage {
                width: width as u32,
                height: height as u32,
                pixels,
            });
        }
        (1, FormatKind::Mask) => {
            pixels
                .par_chunks_exact_mut(width * 4)
                .enumerate()
                .for_each(|(y, out_row)| {
                    let at = y * pitch;
                    let v = &planes[0][at..at + width];
                    for (x, px) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        *px = [v[x], v[x], v[x], 0xFF];
                    }
                });
            return Ok(RgbaImage {
                width: width as u32,
                height: height as u32,
                pixels,
            });
        }
        _ => {}
    }
    pixels
        .par_chunks_exact_mut(width * 4)
        .enumerate()
        .for_each(|(y, out_row)| {
            for (x, out) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (r, g, b, a) = match fmt.kind {
                    FormatKind::Rgba => (
                        sample(&planes[0], x, y),
                        sample(&planes[1], x, y),
                        sample(&planes[2], x, y),
                        sample(&planes[3], x, y),
                    ),
                    FormatKind::Gray => {
                        let g = sample(&planes[0], x, y);
                        (g, g, g, sample(&planes[1], x, y))
                    }
                    FormatKind::Mask => {
                        let v = sample(&planes[0], x, y);
                        (v, v, v, 1.0)
                    }
                    FormatKind::Cmyk => {
                        let (c, m, yl, k) = (
                            sample(&planes[0], x, y),
                            sample(&planes[1], x, y),
                            sample(&planes[2], x, y),
                            sample(&planes[3], x, y),
                        );
                        (
                            (1.0 - c) * (1.0 - k),
                            (1.0 - m) * (1.0 - k),
                            (1.0 - yl) * (1.0 - k),
                            sample(&planes[4], x, y),
                        )
                    }
                    FormatKind::Lab => {
                        let l = sample(&planes[0], x, y) * 100.0;
                        let a_c = sample(&planes[1], x, y) * 255.0 - 128.0;
                        let b_c = sample(&planes[2], x, y) * 255.0 - 128.0;
                        let (r, g, b) = lab_to_srgb(l, a_c, b_c);
                        (r, g, b, sample(&planes[3], x, y))
                    }
                };
                out[0] = (r * 255.0 + 0.5) as u8;
                out[1] = (g * 255.0 + 0.5) as u8;
                out[2] = (b * 255.0 + 0.5) as u8;
                out[3] = (a * 255.0 + 0.5) as u8;
            }
        });
    Ok(RgbaImage {
        width: width as u32,
        height: height as u32,
        pixels,
    })
}

pub(super) struct PlaneJob<'a> {
    archive: &'a Archive<'a>,
    graph: &'a Graph,
    bitm: &'a Node,
    sta: &'a [u8; 4],
    idx: &'a [u8; 4],
    /// Tiles per row of the status array, as the file declares it.
    grid_width: usize,
    pitch: usize,
    rows: usize,
    height: usize,
    bytes_per_sample: usize,
    /// The bitmap's original file and which of its channels this plane
    /// is, when any tile is source-backed (status 5).
    source: Option<(&'a RgbaImage, usize)>,
}

/// Rebuild one channel plane from its tile status list and blocks.
pub(super) fn load_plane(job: PlaneJob) -> Result<Vec<u8>, AffinityError> {
    let statuses: Vec<u8> = match job.bitm.field(job.sta) {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::U8(b) => Ok(*b),
                _ => Err(malformed("tile status is not a byte")),
            })
            .collect::<Result<_, _>>()?,
        // Fully evicted bitmaps drop their status arrays entirely.
        _ => Vec::new(),
    };
    let blocks = job.graph.children(job.bitm, job.idx);
    let mut next_block = blocks.iter();

    let mut plane = vec![0u8; job.pitch * job.rows];
    let places = tile_offsets(job.grid_width, job.height);
    // Pair each stored tile with its destination first; decompressing
    // and CRC-checking the payloads is the bulk of the work and every
    // tile is independent, so that part fans out across cores.
    let mut stored: Vec<(usize, usize, [i32; 4], &str)> = Vec::new();
    for (&status, (x, y)) in statuses.iter().zip(places) {
        match status {
            0 | 1 => {}
            2 => fill_tile(&mut plane, job.pitch, x, y, &[0xFF]),
            3 => fill_tile(&mut plane, job.pitch, x, y, &0x3F80_0000u32.to_le_bytes()),
            4 => {
                let block = next_block
                    .next()
                    .ok_or_else(|| malformed("more stored tiles than blocks"))?;
                // Photo 2 omits the rect on full tiles; the copy below
                // clips to the plane, so the full default is safe.
                let rect: [i32; 4] = match block.field(b"Rect").or_else(|| block.field(b"IRct")) {
                    Some(Value::VecI(v)) if v.len() == 4 => [v[0], v[1], v[2], v[3]],
                    _ => [0, 0, 256, 256],
                };
                let name = match block.field(b"Data") {
                    Some(Value::Embedded { name, .. }) => name,
                    _ => return Err(malformed("block has no data reference")),
                };
                stored.push((x, y, rect, name));
            }
            // Source-backed: the pixels live in the bitmap's original
            // file (Bckg), not in tile entries.
            5 => {
                let Some((source, channel)) = job.source else {
                    return Err(malformed("source-backed tile without a source image"));
                };
                copy_source_tile(&mut plane, &job, source, channel, x, y)?;
            }
            other => return Err(malformed(format!("unknown tile status {other}"))),
        }
    }
    let tiles = stored
        .par_iter()
        .map(|&(_, _, _, name)| {
            let entry = job
                .archive
                .head(name)
                .ok_or_else(|| malformed(format!("missing tile entry {name:?}")))?;
            tile_payload(job.archive.extract(entry)?)
                .ok_or_else(|| malformed(format!("tile {name:?} has no 64 KiB payload")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for ((x, y, rect, _), tile) in stored.iter().zip(&tiles) {
        let (x0, y0) = (
            rect[0].clamp(0, 256) as usize,
            rect[1].clamp(0, 256) as usize,
        );
        let (x1, y1) = (
            rect[2].clamp(0, 256) as usize,
            rect[3].clamp(0, 256) as usize,
        );
        for ty in y0..y1 {
            if y + ty >= job.rows {
                break;
            }
            let dst = (y + ty) * job.pitch + x + x0;
            let src = ty * 256 + x0;
            let n = x1.saturating_sub(x0).min(job.pitch - (x + x0));
            plane[dst..dst + n].copy_from_slice(&tile[src..src + n]);
        }
    }
    Ok(plane)
}

/// Where each entry of a `Sta` array lands in its plane: `x` is a byte
/// offset, `y` a row. The grid is `grid_width` tiles across — which is
/// not always the tight `ceil(row_bytes / 256)`, because Affinity
/// rounds the allocation up (a 5848-byte row can sit in a 32-tile
/// grid). Walking it at the tight width shears every row but the first.
pub(super) fn tile_offsets(
    grid_width: usize,
    height: usize,
) -> impl Iterator<Item = (usize, usize)> {
    let width = grid_width.max(1);
    (0..)
        .map(move |i| ((i % width) * 256, (i / width) * 256))
        .take_while(move |&(_, y)| y < height)
}

/// Fill one tile of a channel plane from the decoded source image.
/// `x` is a byte offset into the plane; the source is always RGBA8, so
/// wider formats spread each 8-bit sample across the sample width.
pub(super) fn copy_source_tile(
    plane: &mut [u8],
    job: &PlaneJob,
    source: &RgbaImage,
    channel: usize,
    x: usize,
    y: usize,
) -> Result<(), AffinityError> {
    if channel >= 4 {
        return Err(malformed("source-backed tile in a >4 channel format"));
    }
    let px0 = x / job.bytes_per_sample;
    let per_tile = 256 / job.bytes_per_sample;
    let sw = source.width as usize;
    for ty in 0..256usize {
        let sy = y + ty;
        if sy >= source.height as usize || sy >= job.rows {
            break;
        }
        for tx in 0..per_tile {
            let sx = px0 + tx;
            if sx >= sw {
                break;
            }
            let v = source.pixels[(sy * sw + sx) * 4 + channel];
            let at = sy * job.pitch + x + tx * job.bytes_per_sample;
            match job.bytes_per_sample {
                1 => plane[at] = v,
                2 => plane[at..at + 2].copy_from_slice(&(v as u16 * 257).to_le_bytes()),
                _ => plane[at..at + 4].copy_from_slice(&(v as f32 / 255.0).to_le_bytes()),
            }
        }
    }
    Ok(())
}

/// Decode a bitmap's original file: the Bckg entry is a tiny "Blck"
/// graph document whose Data blob is the file bytes (PNG, JPEG…).
pub(super) fn source_image(
    archive: &Archive,
    bitm: &Node,
    width: usize,
    height: usize,
) -> Result<RgbaImage, AffinityError> {
    let name = match bitm.field(b"Bckg") {
        Some(Value::Embedded { name, .. }) => name,
        _ => return Err(malformed("source-backed bitmap has no Bckg entry")),
    };
    let entry = archive
        .head(name)
        .ok_or_else(|| malformed(format!("missing source entry {name:?}")))?;
    let data = archive.extract(entry)?;
    let graph = graph::parse(&data)?;
    let file = graph
        .node(graph::ROOT)
        .field(b"Data")
        .and_then(|v| match v {
            Value::Blob(b) => Some(b),
            _ => None,
        })
        .ok_or_else(|| malformed("source entry has no file data"))?;
    let img = image::load_from_memory(file)
        .map_err(|e| malformed(format!("decoding source image: {e}")))?
        .to_rgba8();
    if (img.width() as usize, img.height() as usize) != (width, height) {
        return Err(malformed(format!(
            "source image is {}×{}, bitmap says {width}×{height}",
            img.width(),
            img.height()
        )));
    }
    Ok(RgbaImage {
        width: img.width(),
        height: img.height(),
        pixels: img.into_raw(),
    })
}
/// A tile entry is either the bare 64 KiB plane, or (older files) a tiny
/// graph document of type "Data" whose one blob field holds the plane.
pub(super) fn tile_payload(data: Vec<u8>) -> Option<Vec<u8>> {
    if data.len() == 0x10000 {
        return Some(data);
    }
    let graph = graph::parse(&data).ok()?;
    graph
        .node(graph::ROOT)
        .fields
        .iter()
        .find_map(|(_, v)| match v {
            Value::Blob(b) if b.len() == 0x10000 => Some(b.clone()),
            _ => None,
        })
}

pub(super) fn fill_tile(plane: &mut [u8], pitch: usize, x: usize, y: usize, pattern: &[u8]) {
    for row in 0..256 {
        let base = (y + row) * pitch + x;
        if base + 256 > plane.len() {
            break;
        }
        for (i, byte) in plane[base..base + 256].iter_mut().enumerate() {
            *byte = pattern[i % pattern.len()];
        }
    }
}
