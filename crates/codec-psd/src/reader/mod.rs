//! Top-level PSD/PSB reader: stitches the five sections into a `Document`.

mod cursor;
mod header;
mod image_data;
mod layers;
mod pixels;
mod resources;
pub(crate) mod rle;

use crate::error::PsdError;
use cursor::Cursor;
use photoslop_color::ColorMode;
use photoslop_core::{Document, IntRect, Layer, PreservedResource, RasterLayer};

/// Sentinel `PreservedResource::id` under which the Color Mode Data section
/// is stashed (with name `b"colormodedata"`). Real Adobe image resource ids
/// never reach 0xFFFF, so the M6 writer can recognize and re-emit it as the
/// Color Mode Data section rather than as an image resource.
pub const COLOR_MODE_DATA_SENTINEL_ID: u16 = 0xFFFF;

/// Read a PSD (version 1) or PSB (version 2) file into a `Document`.
///
/// Never panics on malformed input — all structural problems surface as
/// [`PsdError`].
pub fn read_psd(bytes: &[u8]) -> Result<Document, PsdError> {
    let mut cur = Cursor::new(bytes);

    // 1. File header.
    let header = header::parse_header(&mut cur)?;

    // 2. Color Mode Data. Empty for RGB/Grayscale in practice (it holds the
    //    palette for Indexed and curves for Duotone), but preserved verbatim
    //    under a sentinel resource id when present.
    let cm_len = cur.u32()? as usize; // always u32, even in PSB
    let color_mode_data = cur.take(cm_len)?.to_vec();

    // 3. Image Resources.
    let res = resources::parse_image_resources(&mut cur)?;

    // 4. Layer & Mask Information.
    let parsed = layers::parse_layer_and_mask_info(&mut cur, &header)?;
    let mut tree_layers = parsed.layers;

    // 5. Merged image data. Only decoded when the file is flattened (zero
    //    layer records) — then it becomes a synthesized "Background" layer.
    //    Files with layers may still carry it; we tolerate and skip it.
    if tree_layers.is_empty() {
        match image_data::parse_image_data(&mut cur, &header)? {
            Some(composite) => {
                tree_layers.push(background_from_composite(
                    &header,
                    composite,
                    parsed.merged_alpha,
                ));
            }
            None => log::warn!("flattened PSD without merged image data; opening empty"),
        }
    }

    let mut doc = Document::new("", header.width, header.height, header.depth);
    doc.mode = header.mode;
    if let Some(dpi) = res.resolution_dpi {
        doc.resolution_dpi = dpi;
    }
    doc.icc_profile = res.icc_profile;
    doc.preserved_resources = res.preserved;
    if !color_mode_data.is_empty() {
        doc.preserved_resources.insert(
            0,
            PreservedResource {
                id: COLOR_MODE_DATA_SENTINEL_ID,
                name: b"colormodedata".to_vec(),
                data: color_mode_data,
            },
        );
    }
    doc.tree.layers = tree_layers;
    // Topmost layer starts active (children are stored bottom-to-top).
    doc.active_layer = doc.tree.layers.last().map(|l| l.id);
    doc.damage_all();
    doc.dirty = false;
    Ok(doc)
}

/// Build a "Background" raster layer from the merged composite of a
/// flattened file. The extra channel past the color channels is treated as
/// transparency only when the (negative) layer count said so; otherwise the
/// composite is opaque and extra channels are spot/alpha channels we ignore.
fn background_from_composite(
    header: &header::Header,
    composite: image_data::CompositePlanes,
    merged_alpha: bool,
) -> Layer {
    let base = header.base_channels() as usize;
    let to_f32 = |i: usize| {
        composite
            .planes
            .get(i)
            .map(|p| pixels::plane_to_f32(p, header.depth))
    };

    let mut planes = pixels::ColorPlanes::default();
    match header.mode {
        ColorMode::Rgb => {
            planes.r = to_f32(0);
            planes.g = to_f32(1);
            planes.b = to_f32(2);
        }
        ColorMode::Grayscale => {
            planes.r = to_f32(0);
            planes.g.clone_from(&planes.r);
            planes.b.clone_from(&planes.r);
        }
    }
    if merged_alpha && header.channels as usize > base {
        planes.a = to_f32(base);
    }

    let mut layer = Layer::new_raster("Background");
    let rect = IntRect::from_size(header.width, header.height);
    if let photoslop_core::LayerKind::Raster(RasterLayer { tiles }) = &mut layer.kind {
        pixels::fill_tiles(tiles, header.depth, rect, &planes);
    }
    layer
}
