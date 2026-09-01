//! The import dialog's map: a navigable OpenStreetMap view — drag to
//! pan, scroll to zoom, draw a rectangle to set the boundary — plus the
//! geometry behind "was this photo taken inside it".
//!
//! The map is genuine OpenStreetMap: standard raster tiles fetched on
//! demand from tile.openstreetmap.org with an identifying User-Agent
//! (their tile policy's one requirement), cached on disk beside the
//! thumbnails, attributed in the dialog. It paints the way the document
//! canvas does — a gpui `canvas` element placing one quad per visible
//! tile — so panning is a repaint, not a re-render of a stitched image.

use super::library;
use super::*;
use std::io::Read as _;

// The boundary type itself lives in the workspace module so the modal
// enum can carry it on every target; everything that *does* geometry
// with it is here.
pub use super::GeoBounds;

impl GeoBounds {
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        lat >= self.south && lat <= self.north && lon >= self.west && lon <= self.east
    }

    pub fn center(&self) -> (f64, f64) {
        (
            (self.south + self.north) / 2.0,
            (self.west + self.east) / 2.0,
        )
    }

    /// Normalized from any two corners, so a drag in any direction
    /// makes a valid box.
    fn from_corners(a: (f64, f64), b: (f64, f64)) -> GeoBounds {
        GeoBounds {
            south: a.0.min(b.0),
            west: a.1.min(b.1),
            north: a.0.max(b.0),
            east: a.1.max(b.1),
        }
    }
}

/// A named place: a quick jump on the map, nothing more — the boundary
/// it sets is an ordinary [`GeoBounds`] the user is free to redraw.
pub struct Place {
    pub name: &'static str,
    pub bounds: GeoBounds,
}

macro_rules! place {
    ($name:literal, $south:literal, $west:literal, $north:literal, $east:literal) => {
        Place {
            name: $name,
            bounds: GeoBounds {
                south: $south,
                west: $west,
                north: $north,
                east: $east,
            },
        }
    };
}

pub const PLACES: &[Place] = &[
    place!("New York City", 40.49, -74.27, 40.92, -73.68),
    place!("San Francisco", 37.70, -122.53, 37.84, -122.34),
    place!("Los Angeles", 33.70, -118.67, 34.34, -118.15),
    place!("London", 51.28, -0.51, 51.69, 0.33),
    place!("Paris", 48.81, 2.22, 48.91, 2.47),
    place!("Berlin", 52.34, 13.09, 52.68, 13.76),
    place!("Tokyo", 35.52, 139.56, 35.82, 139.92),
    place!("Sydney", -34.12, 150.60, -33.57, 151.34),
];

/// One raster tile's edge, per the OSM standard.
const TILE: f64 = 256.0;
/// Web-Mercator's poles: beyond this latitude there are no tiles.
const MAX_LAT: f64 = 85.05;
const MIN_ZOOM: i32 = 2;
const MAX_ZOOM: i32 = 19;
/// Wheel travel per zoom step, so touchpads don't fly through levels.
const WHEEL_STEP: f32 = 40.0;
/// Tiles fetched per background batch.
const TILE_BATCH: usize = 6;
/// In-memory tiles kept before the cache is dumped (the disk cache
/// makes refetching cheap).
const TILE_KEEP: usize = 600;

/// Web-Mercator: latitude/longitude to fractional tile coordinates.
pub(super) fn tile_coords(lat: f64, lon: f64, zoom: i32) -> (f64, f64) {
    let n = 2f64.powi(zoom);
    let x = (lon + 180.0) / 360.0 * n;
    let rad = lat.clamp(-MAX_LAT, MAX_LAT).to_radians();
    let y = (1.0 - (rad.tan() + 1.0 / rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (x, y)
}

/// The inverse: fractional tile coordinates back to degrees.
pub(super) fn coords_to_lat_lon(x: f64, y: f64, zoom: i32) -> (f64, f64) {
    let n = 2f64.powi(zoom);
    let lon = (x / n * 360.0 - 180.0).clamp(-180.0, 180.0);
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y / n))
        .sinh()
        .atan()
        .to_degrees();
    (lat, lon)
}

/// The deepest zoom that shows the whole box in roughly two tiles per
/// axis — how far a preset jump zooms in.
fn zoom_for(bounds: &GeoBounds) -> i32 {
    for zoom in (MIN_ZOOM..=12).rev() {
        let (x0, y0) = tile_coords(bounds.north, bounds.west, zoom);
        let (x1, y1) = tile_coords(bounds.south, bounds.east, zoom);
        if x1 - x0 <= 2.0 && y1 - y0 <= 2.0 {
            return zoom;
        }
    }
    MIN_ZOOM
}

// ----- tiles -----

/// Where fetched tiles are cached between runs. Roads change on the
/// timescale of construction; no expiry is fine.
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

enum MapTile {
    Pending,
    Ready(Arc<RenderImage>),
    Failed,
}

// ----- the navigable view -----

/// A drag on the map: moving it, or drawing the boundary on it.
pub enum MapDrag {
    Pan { last: (f32, f32) },
    Draw { anchor: (f64, f64) },
}

/// The map's whole state: where it is looking, what is drawn on it,
/// and the tiles it has. Lives on the library so the boundary survives
/// closing and reopening the dialog.
pub struct MapState {
    /// Latitude/longitude at the middle of the view.
    pub center: (f64, f64),
    pub zoom: i32,
    /// The drawn boundary, if any. `None` imports everything.
    pub selection: Option<GeoBounds>,
    /// The preset's name when the boundary came from a jump chip;
    /// cleared the moment the user draws their own.
    pub selection_name: Option<String>,
    /// Clicking draws instead of panning (Shift-drag always draws).
    pub draw_mode: bool,
    pub drag: Option<MapDrag>,
    /// Accumulated wheel travel toward the next zoom step.
    scroll_debt: f32,
    /// The widget's window-space rectangle, recorded each paint, so
    /// mouse positions can be turned into map positions.
    origin: (f32, f32),
    size: (f32, f32),
    tiles: FxHashMap<(i32, i64, i64), MapTile>,
    queue: Vec<(i32, i64, i64)>,
    ticker: bool,
}

impl Default for MapState {
    fn default() -> Self {
        MapState {
            // The Atlantic from far out: most of the inhabited world in
            // one glance, pick your continent and dive.
            center: (30.0, -20.0),
            zoom: MIN_ZOOM,
            selection: None,
            selection_name: None,
            draw_mode: false,
            drag: None,
            scroll_debt: 0.0,
            origin: (0.0, 0.0),
            size: (1.0, 1.0),
            tiles: FxHashMap::default(),
            queue: Vec::new(),
            ticker: false,
        }
    }
}

impl MapState {
    /// The view centre in global pixels at the current zoom.
    fn center_px(&self) -> (f64, f64) {
        let (x, y) = tile_coords(self.center.0, self.center.1, self.zoom);
        (x * TILE, y * TILE)
    }

    /// A window position as latitude/longitude under the map.
    pub fn geo_at(&self, window: (f32, f32)) -> (f64, f64) {
        let (cx, cy) = self.center_px();
        let gx = cx - self.size.0 as f64 / 2.0 + (window.0 - self.origin.0) as f64;
        let gy = cy - self.size.1 as f64 / 2.0 + (window.1 - self.origin.1) as f64;
        coords_to_lat_lon(gx / TILE, gy / TILE, self.zoom)
    }

    /// A latitude/longitude as a window position.
    fn window_at(&self, lat: f64, lon: f64) -> (f32, f32) {
        let (cx, cy) = self.center_px();
        let (x, y) = tile_coords(lat, lon, self.zoom);
        (
            (x * TILE - cx + self.size.0 as f64 / 2.0) as f32 + self.origin.0,
            (y * TILE - cy + self.size.1 as f64 / 2.0) as f32 + self.origin.1,
        )
    }

    /// Start a drag: drawing when asked (Shift or draw mode), panning
    /// otherwise.
    pub fn begin_drag(&mut self, window: (f32, f32), draw: bool) {
        self.drag = Some(if draw {
            let anchor = self.geo_at(window);
            self.selection = Some(GeoBounds::from_corners(anchor, anchor));
            self.selection_name = None;
            MapDrag::Draw { anchor }
        } else {
            MapDrag::Pan { last: window }
        });
    }

    /// Continue whichever drag is running. Returns whether anything
    /// changed (so the caller knows to repaint).
    pub fn drag_to(&mut self, window: (f32, f32)) -> bool {
        match &mut self.drag {
            Some(MapDrag::Pan { last }) => {
                let (dx, dy) = (window.0 - last.0, window.1 - last.1);
                *last = window;
                let (cx, cy) = self.center_px();
                self.center =
                    coords_to_lat_lon((cx - dx as f64) / TILE, (cy - dy as f64) / TILE, self.zoom);
                self.center.0 = self.center.0.clamp(-MAX_LAT, MAX_LAT);
                true
            }
            Some(MapDrag::Draw { anchor }) => {
                let anchor = *anchor;
                let here = self.geo_at(window);
                self.selection = Some(GeoBounds::from_corners(anchor, here));
                true
            }
            None => false,
        }
    }

    pub fn end_drag(&mut self) {
        // A boundary needs area; a stray click's zero-size box means
        // "no boundary", which reads as clearing it — keep that.
        if let (Some(MapDrag::Draw { .. }), Some(sel)) = (&self.drag, &self.selection) {
            if (sel.north - sel.south) < 1e-6 || (sel.east - sel.west) < 1e-6 {
                self.selection = None;
            }
        }
        self.drag = None;
    }

    /// Wheel travel: a step of zoom once enough has accumulated, about
    /// the point under the pointer, the way every web map does it.
    pub fn wheel(&mut self, dy: f32, window: (f32, f32)) -> bool {
        self.scroll_debt += dy;
        let step = if self.scroll_debt >= WHEEL_STEP {
            1
        } else if self.scroll_debt <= -WHEEL_STEP {
            -1
        } else {
            return false;
        };
        self.scroll_debt = 0.0;
        self.zoom_step(step, window);
        true
    }

    /// One zoom step keeping the point under `window` fixed.
    pub fn zoom_step(&mut self, step: i32, window: (f32, f32)) {
        let anchor = self.geo_at(window);
        let zoom = (self.zoom + step).clamp(MIN_ZOOM, MAX_ZOOM);
        if zoom == self.zoom {
            return;
        }
        self.zoom = zoom;
        // Put the anchor back under the pointer: solve for the centre
        // that places it at the same window position.
        let (ax, ay) = tile_coords(anchor.0, anchor.1, zoom);
        let local = (
            (window.0 - self.origin.0) as f64,
            (window.1 - self.origin.1) as f64,
        );
        let cx = ax * TILE - local.0 + self.size.0 as f64 / 2.0;
        let cy = ay * TILE - local.1 + self.size.1 as f64 / 2.0;
        self.center = coords_to_lat_lon(cx / TILE, cy / TILE, zoom);
        self.center.0 = self.center.0.clamp(-MAX_LAT, MAX_LAT);
    }

    /// One zoom step about the middle of the view (the ± buttons).
    pub fn zoom_center(&mut self, step: i32) {
        let at = (
            self.origin.0 + self.size.0 / 2.0,
            self.origin.1 + self.size.1 / 2.0,
        );
        self.zoom_step(step, at);
    }

    /// Jump to a preset: frame its box and make it the boundary.
    pub fn jump_to(&mut self, name: &str, bounds: GeoBounds) {
        self.center = bounds.center();
        self.zoom = zoom_for(&bounds);
        self.selection = Some(bounds);
        self.selection_name = Some(name.to_string());
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.selection_name = None;
    }
}

/// Everything one frame of the map paints.
pub struct MapPaint {
    pub tiles: Vec<(Bounds<Pixels>, Arc<RenderImage>)>,
    /// Tiles not here yet: painted as flat sea-grey.
    pub missing: Vec<Bounds<Pixels>>,
    /// The boundary in window space, if one is set.
    pub selection: Option<Bounds<Pixels>>,
}

impl Workspace {
    /// Lay the visible tiles out for painting, queueing fetches for the
    /// ones that are not here yet. Runs in the canvas prepaint, exactly
    /// as the document viewport does.
    pub(super) fn prepare_map_paint(&mut self, bounds: Bounds<Pixels>) -> MapPaint {
        let map = &mut self.library.map;
        map.origin = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        map.size = (
            f32::from(bounds.size.width).max(1.0),
            f32::from(bounds.size.height).max(1.0),
        );
        let (cx, cy) = map.center_px();
        let (w, h) = (map.size.0 as f64, map.size.1 as f64);
        let n = 1i64 << map.zoom;
        let left = ((cx - w / 2.0) / TILE).floor() as i64;
        let right = ((cx + w / 2.0) / TILE).floor() as i64;
        let top = ((cy - h / 2.0) / TILE).floor() as i64;
        let bottom = ((cy + h / 2.0) / TILE).floor() as i64;
        let mut paint = MapPaint {
            tiles: Vec::new(),
            missing: Vec::new(),
            selection: None,
        };
        let zoom = map.zoom;
        for ty in top.max(0)..=bottom.min(n - 1) {
            for tx in left.max(0)..=right.min(n - 1) {
                let rect = Bounds {
                    origin: point(
                        px((tx as f64 * TILE - cx + w / 2.0) as f32 + map.origin.0),
                        px((ty as f64 * TILE - cy + h / 2.0) as f32 + map.origin.1),
                    ),
                    size: size(px(TILE as f32), px(TILE as f32)),
                };
                match map.tiles.get(&(zoom, tx, ty)) {
                    Some(MapTile::Ready(img)) => paint.tiles.push((rect, img.clone())),
                    Some(_) => paint.missing.push(rect),
                    None => {
                        map.tiles.insert((zoom, tx, ty), MapTile::Pending);
                        map.queue.push((zoom, tx, ty));
                        paint.missing.push(rect);
                    }
                }
            }
        }
        if let Some(sel) = map.selection {
            let (x0, y0) = map.window_at(sel.north, sel.west);
            let (x1, y1) = map.window_at(sel.south, sel.east);
            paint.selection = Some(Bounds {
                origin: point(px(x0), px(y0)),
                size: size(px((x1 - x0).max(1.0)), px((y1 - y0).max(1.0))),
            });
        }
        paint
    }

    /// Start the tile loader if fetches are queued and none is running.
    pub(super) fn kick_map_tiles(&mut self, cx: &mut Context<Self>) {
        let map = &mut self.library.map;
        // The disk cache makes refetching cheap; dumping the lot beats
        // bookkeeping an LRU for a dialog.
        if map.tiles.len() > TILE_KEEP && map.drag.is_none() {
            map.tiles.clear();
        }
        if map.queue.is_empty() || map.ticker {
            return;
        }
        map.ticker = true;
        cx.spawn(async move |this, cx| loop {
            let batch: Vec<(i32, i64, i64)> = match this.update(cx, |ws, _| {
                let queue = &mut ws.library.map.queue;
                let n = queue.len().min(TILE_BATCH);
                queue.drain(..n).collect()
            }) {
                Ok(batch) => batch,
                Err(_) => return,
            };
            if batch.is_empty() {
                this.update(cx, |ws, _| ws.library.map.ticker = false).ok();
                return;
            }
            let results = cx
                .background_executor()
                .spawn(async move {
                    batch
                        .into_iter()
                        .map(|(z, x, y)| {
                            let img = fetch_tile(z, x, y).and_then(|img| {
                                library::rgba_to_render_image(
                                    img.width(),
                                    img.height(),
                                    img.into_raw(),
                                )
                            });
                            ((z, x, y), img)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let keep = this.update(cx, |ws, cx| {
                for (key, img) in results {
                    let state = match img {
                        Some(img) => MapTile::Ready(img),
                        None => MapTile::Failed,
                    };
                    ws.library.map.tiles.insert(key, state);
                }
                cx.notify();
            });
            if keep.is_err() {
                return;
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mercator_puts_null_island_in_the_middle() {
        let (x, y) = tile_coords(0.0, 0.0, 1);
        assert!((x - 1.0).abs() < 1e-9 && (y - 1.0).abs() < 1e-9);
        let (x, y) = tile_coords(0.0, 0.0, 4);
        assert!((x - 8.0).abs() < 1e-9 && (y - 8.0).abs() < 1e-9);
    }

    #[test]
    fn mercator_round_trips() {
        // Panning and zoom-at-cursor both go degrees → pixels → degrees;
        // a lossy pair would make the map creep under the pointer.
        for &(lat, lon) in &[(40.758, -73.985), (-33.86, 151.21), (64.15, -21.94)] {
            for zoom in [2, 8, 15] {
                let (x, y) = tile_coords(lat, lon, zoom);
                let (lat2, lon2) = coords_to_lat_lon(x, y, zoom);
                assert!((lat - lat2).abs() < 1e-6, "{lat} came back {lat2}");
                assert!((lon - lon2).abs() < 1e-6, "{lon} came back {lon2}");
            }
        }
    }

    #[test]
    fn manhattan_is_in_new_york_and_boston_is_not() {
        let nyc = &PLACES[0];
        assert_eq!(nyc.name, "New York City");
        assert!(nyc.bounds.contains(40.758, -73.985)); // Times Square
        assert!(!nyc.bounds.contains(42.355, -71.065)); // Boston Common
    }

    #[test]
    fn a_drawn_box_is_valid_whatever_corner_the_drag_started_from() {
        let a = GeoBounds::from_corners((40.9, -74.2), (40.5, -73.7));
        let b = GeoBounds::from_corners((40.5, -73.7), (40.9, -74.2));
        assert_eq!(a, b);
        assert!(a.south < a.north && a.west < a.east);
    }

    #[test]
    fn zoom_at_cursor_keeps_the_anchor_under_the_pointer() {
        let mut map = MapState {
            size: (520.0, 300.0),
            ..MapState::default()
        };
        map.center = (48.85, 2.35); // Paris
        map.zoom = 6;
        let cursor = (140.0, 90.0);
        let before = map.geo_at(cursor);
        map.zoom_step(1, cursor);
        let after = map.geo_at(cursor);
        assert!((before.0 - after.0).abs() < 1e-6, "latitude drifted");
        assert!((before.1 - after.1).abs() < 1e-6, "longitude drifted");
    }

    #[test]
    fn preset_jumps_frame_their_box() {
        let mut map = MapState::default();
        for place in PLACES {
            map.jump_to(place.name, place.bounds);
            assert!((MIN_ZOOM..=12).contains(&map.zoom), "{}", place.name);
            assert_eq!(map.selection, Some(place.bounds));
            let (x0, y0) = tile_coords(place.bounds.north, place.bounds.west, map.zoom);
            let (x1, y1) = tile_coords(place.bounds.south, place.bounds.east, map.zoom);
            assert!(x1 - x0 <= 2.0 && y1 - y0 <= 2.0, "{}", place.name);
        }
    }
}
