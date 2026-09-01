//! Places, and the OpenStreetMap preview the import dialog draws for
//! one: "taken in New York" as a bounding box, shown on a real map.
//!
//! The map is genuine OpenStreetMap — standard raster tiles fetched from
//! tile.openstreetmap.org with an identifying User-Agent (their tile
//! policy's one requirement), cached on disk beside the thumbnails, and
//! attributed in the dialog. The place's box is drawn over the tiles: a
//! faint fill and a solid outline, so the dialog answers "which photos
//! will this import take" with geography rather than numbers.

use super::*;
use std::io::Read as _;

/// A named place: a bounding box in degrees.
pub struct Place {
    pub name: &'static str,
    pub south: f64,
    pub west: f64,
    pub north: f64,
    pub east: f64,
}

/// The places the import filter offers. Bounding boxes rather than
/// polygons: "taken in New York" is a photo-sorting question, not a
/// surveying one, and a box errs by a suburb, not a continent.
pub const PLACES: &[Place] = &[
    Place {
        name: "New York City",
        south: 40.49,
        west: -74.27,
        north: 40.92,
        east: -73.68,
    },
    Place {
        name: "San Francisco",
        south: 37.70,
        west: -122.53,
        north: 37.84,
        east: -122.34,
    },
    Place {
        name: "Los Angeles",
        south: 33.70,
        west: -118.67,
        north: 34.34,
        east: -118.15,
    },
    Place {
        name: "London",
        south: 51.28,
        west: -0.51,
        north: 51.69,
        east: 0.33,
    },
    Place {
        name: "Paris",
        south: 48.81,
        west: 2.22,
        north: 48.91,
        east: 2.47,
    },
    Place {
        name: "Berlin",
        south: 52.34,
        west: 13.09,
        north: 52.68,
        east: 13.76,
    },
    Place {
        name: "Tokyo",
        south: 35.52,
        west: 139.56,
        north: 35.82,
        east: 139.92,
    },
    Place {
        name: "Sydney",
        south: -34.12,
        west: 150.60,
        north: -33.57,
        east: 151.34,
    },
];

pub fn place_contains(place: &Place, lat: f64, lon: f64) -> bool {
    lat >= place.south && lat <= place.north && lon >= place.west && lon <= place.east
}

/// Web-Mercator: latitude/longitude to fractional tile coordinates.
fn tile_coords(lat: f64, lon: f64, zoom: i32) -> (f64, f64) {
    let n = 2f64.powi(zoom);
    let x = (lon + 180.0) / 360.0 * n;
    let rad = lat.to_radians();
    let y = (1.0 - (rad.tan() + 1.0 / rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (x, y)
}

/// One raster tile's edge, per the OSM standard.
const TILE: usize = 256;

/// The deepest zoom at which the box still spans at most two tiles per
/// axis, so a preview is never more than 3×3 tiles (nine small fetches,
/// most of them cached after the first look).
fn pick_zoom(place: &Place) -> i32 {
    for zoom in (1..=12).rev() {
        let (x0, y0) = tile_coords(place.north, place.west, zoom);
        let (x1, y1) = tile_coords(place.south, place.east, zoom);
        if x1 - x0 <= 2.0 && y1 - y0 <= 2.0 {
            return zoom;
        }
    }
    1
}

/// Where fetched tiles are cached between runs. Tiles are content that
/// changes on the timescale of road-building; no expiry is fine.
fn tile_cache_path(zoom: i32, x: i64, y: i64) -> Option<PathBuf> {
    Some(
        crate::crash::state_dir()?
            .join("schist/tiles")
            .join(format!("{zoom}-{x}-{y}.png")),
    )
}

/// One tile, from the disk cache or the network. Blocking.
fn fetch_tile(zoom: i32, x: i64, y: i64) -> Option<image::RgbaImage> {
    let n = 1i64 << zoom;
    if x < 0 || y < 0 || x >= n || y >= n {
        return None;
    }
    let cache = tile_cache_path(zoom, x, y);
    if let Some(bytes) = cache.as_ref().and_then(|p| std::fs::read(p).ok()) {
        if let Ok(img) = image::load_from_memory(&bytes) {
            return Some(img.into_rgba8());
        }
    }
    let url = format!("https://tile.openstreetmap.org/{zoom}/{x}/{y}.png");
    let mut response = ureq::get(&url)
        // OSM's tile usage policy asks for a User-Agent that identifies
        // the application, not a browser masquerade.
        .header(
            "User-Agent",
            "schist-gallery (+https://github.com/Infrawrench/schist)",
        )
        .call()
        .ok()?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(4 << 20)
        .read_to_end(&mut bytes)
        .ok()?;
    let img = image::load_from_memory(&bytes).ok()?.into_rgba8();
    if let Some(path) = cache {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, &bytes);
    }
    Some(img)
}

/// Assemble the tiles covering a place and draw its box over them.
/// Blocking (network); returns straight RGBA, or `None` when not one
/// tile could be had (offline, most likely).
fn render_map(place: &'static Place) -> Option<(u32, u32, Vec<u8>)> {
    let zoom = pick_zoom(place);
    let (x0, y0) = tile_coords(place.north, place.west, zoom);
    let (x1, y1) = tile_coords(place.south, place.east, zoom);
    let (tx0, ty0) = (x0.floor() as i64, y0.floor() as i64);
    let (tx1, ty1) = (x1.floor() as i64, y1.floor() as i64);
    let (cols, rows) = ((tx1 - tx0 + 1) as usize, (ty1 - ty0 + 1) as usize);
    let (w, h) = (cols * TILE, rows * TILE);
    // Sea-grey underneath, so a tile that failed to arrive reads as
    // missing rather than as black land.
    let mut rgba = vec![0xD8u8; w * h * 4];
    let mut any = false;
    for row in 0..rows {
        for col in 0..cols {
            let Some(tile) = fetch_tile(zoom, tx0 + col as i64, ty0 + row as i64) else {
                continue;
            };
            if tile.width() as usize != TILE || tile.height() as usize != TILE {
                continue;
            }
            any = true;
            let raw = tile.into_raw();
            for line in 0..TILE {
                let dst = ((row * TILE + line) * w + col * TILE) * 4;
                let src = line * TILE * 4;
                rgba[dst..dst + TILE * 4].copy_from_slice(&raw[src..src + TILE * 4]);
            }
        }
    }
    if !any {
        return None;
    }
    // The bounding box: solid two-pixel outline, faint fill.
    let to_px = |v: f64, origin: i64, max: usize| {
        (((v - origin as f64) * TILE as f64).round() as i64).clamp(0, max as i64 - 1)
    };
    let (bx0, by0) = (to_px(x0, tx0, w), to_px(y0, ty0, h));
    let (bx1, by1) = (to_px(x1, tx0, w), to_px(y1, ty0, h));
    const BLUE: [u8; 3] = [0x4A, 0x90, 0xD9];
    for y in by0..=by1 {
        for x in bx0..=bx1 {
            let i = (y as usize * w + x as usize) * 4;
            let edge = x - bx0 < 2 || bx1 - x < 2 || y - by0 < 2 || by1 - y < 2;
            if edge {
                rgba[i..i + 3].copy_from_slice(&BLUE);
            } else {
                for (c, &tint) in BLUE.iter().enumerate() {
                    let base = rgba[i + c] as u32;
                    rgba[i + c] = ((base * 5 + tint as u32) / 6) as u8;
                }
            }
            rgba[i + 3] = 0xFF;
        }
    }
    Some((w as u32, h as u32, rgba))
}

/// A place preview's spot in the pipeline, keyed by place index.
pub(super) enum MapPreview {
    Pending,
    Ready(Arc<RenderImage>),
    /// Not one tile arrived; the filter still works without the picture.
    Failed,
}

impl Workspace {
    /// Start fetching a place's map preview if it has not been asked for
    /// yet. The dialog calls this for whichever place is selected.
    pub(super) fn ensure_map_preview(&mut self, place: usize, cx: &mut Context<Self>) {
        if self.library.map_previews.contains_key(&place) {
            return;
        }
        let Some(spec) = PLACES.get(place) else {
            return;
        };
        self.library.map_previews.insert(place, MapPreview::Pending);
        cx.spawn(async move |this, cx| {
            let map = cx
                .background_executor()
                .spawn(async move { render_map(spec) })
                .await;
            this.update(cx, |ws, cx| {
                let state = map
                    .and_then(|(w, h, rgba)| super::library::rgba_to_render_image(w, h, rgba))
                    .map(MapPreview::Ready)
                    .unwrap_or(MapPreview::Failed);
                ws.library.map_previews.insert(place, state);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mercator_puts_the_null_island_in_the_middle() {
        // (0°, 0°) is the centre of the world map at every zoom.
        let (x, y) = tile_coords(0.0, 0.0, 1);
        assert!((x - 1.0).abs() < 1e-9 && (y - 1.0).abs() < 1e-9);
        let (x, y) = tile_coords(0.0, 0.0, 4);
        assert!((x - 8.0).abs() < 1e-9 && (y - 8.0).abs() < 1e-9);
    }

    #[test]
    fn manhattan_is_in_new_york_and_boston_is_not() {
        let nyc = &PLACES[0];
        assert_eq!(nyc.name, "New York City");
        // Times Square.
        assert!(place_contains(nyc, 40.758, -73.985));
        // Boston Common.
        assert!(!place_contains(nyc, 42.355, -71.065));
    }

    #[test]
    fn every_place_previews_in_at_most_a_three_by_three() {
        for place in PLACES {
            let zoom = pick_zoom(place);
            assert!((1..=12).contains(&zoom), "{}", place.name);
            let (x0, y0) = tile_coords(place.north, place.west, zoom);
            let (x1, y1) = tile_coords(place.south, place.east, zoom);
            assert!(
                x1 - x0 <= 2.0 && y1 - y0 <= 2.0,
                "{} spans {}x{} tiles at z{zoom}",
                place.name,
                x1 - x0,
                y1 - y0
            );
            // And the box is not degenerate on the map.
            assert!(x1 > x0 && y1 > y0, "{}", place.name);
        }
    }
}
