//! Places: a boundary in degrees, and the gazetteer that turns "nyc"
//! into a point with a radius.

/// A boundary in degrees: what the import map's rectangle means, and
/// what the EXIF-position filter tests. Serde because a smart bucket's
/// area persists in `library.json`.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GeoBounds {
    pub south: f64,
    pub west: f64,
    pub north: f64,
    pub east: f64,
}

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
    pub fn from_corners(a: (f64, f64), b: (f64, f64)) -> GeoBounds {
        GeoBounds {
            south: a.0.min(b.0),
            west: a.1.min(b.1),
            north: a.0.max(b.0),
            east: a.1.max(b.1),
        }
    }
}

/// A place a query named: where it is, and how far out a photo still
/// counts as "there".
#[derive(Clone, Debug)]
pub struct GeoMatch {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub radius_km: f64,
}

/// GeoNames cities of 100k+ people plus a handful of aliases (nyc, sf,
/// vegas…), a quarter-megabyte in the binary. CC-BY 4.0, geonames.org.
struct City {
    /// Lowercase, what queries match against; aliases included.
    name: String,
    /// What people see: the canonical name, properly cased.
    display: String,
    lat: f64,
    lon: f64,
    pop: u64,
}

fn gazetteer() -> &'static Vec<City> {
    static GAZETTEER: std::sync::OnceLock<Vec<City>> = std::sync::OnceLock::new();
    GAZETTEER.get_or_init(|| {
        include_str!("../assets/gazetteer.tsv")
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| {
                let mut f = l.split('\t');
                Some(City {
                    name: f.next()?.to_string(),
                    display: f.next()?.to_string(),
                    lat: f.next()?.parse().ok()?,
                    lon: f.next()?.parse().ok()?,
                    pop: f.next()?.parse().ok()?,
                })
            })
            .collect()
    })
}

/// How far out of town a photo still counts as taken there: big cities
/// sprawl, small ones don't.
fn radius_for(pop: u64) -> f64 {
    if pop >= 5_000_000 {
        40.0
    } else if pop >= 1_000_000 {
        25.0
    } else if pop >= 250_000 {
        15.0
    } else {
        10.0
    }
}

/// Levenshtein distance, capped: the caller only cares about "close".
fn edit_distance(a: &str, b: &str, cap: usize) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > cap {
        return cap + 1;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            row.push((prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1));
        }
        if row.iter().min().copied().unwrap_or(0) > cap {
            return cap + 1;
        }
        prev = row;
    }
    prev[b.len()]
}

/// The best place the query names, if any: every one- to three-word
/// window of it, matched exactly, by prefix ("san fran"), or within a
/// typo or two ("new yrok"). Longer windows and better matches win;
/// population breaks ties, so "paris" is France's before Texas's.
pub fn find_place(query: &str) -> Option<GeoMatch> {
    let tokens: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    if tokens.is_empty() {
        return None;
    }
    // (window length, match quality, population) — all ascending.
    let mut best: Option<((usize, u8, u64), &City)> = None;
    for n in 1..=3.min(tokens.len()) {
        for window in tokens.windows(n) {
            let cand = window.join(" ");
            if cand.len() < 2 {
                continue;
            }
            let cap = match cand.chars().count() {
                0..=4 => 0,
                5..=7 => 1,
                _ => 2,
            };
            for city in gazetteer() {
                let quality = if city.name == cand {
                    3
                } else if cap > 0 && edit_distance(&cand, &city.name, cap) <= cap {
                    2
                } else if cand.len() >= 4 && city.name.starts_with(&cand) {
                    1
                } else {
                    continue;
                };
                let key = (n, quality, city.pop);
                if best.map(|(k, _)| key > k).unwrap_or(true) {
                    best = Some((key, city));
                }
            }
        }
    }
    let (_, city) = best?;
    Some(GeoMatch {
        name: city.display.clone(),
        lat: city.lat,
        lon: city.lon,
        radius_km: radius_for(city.pop),
    })
}

/// The city a photo groups under: the *largest* one within 60 km —
/// the gazetteer lists big neighbourhoods too, and a Manhattan photo
/// belongs to "New York City", not "Upper West Side" — else the
/// nearest within 300, else `None`: mid-ocean photos are nobody's.
pub fn nearest_city(lat: f64, lon: f64) -> Option<String> {
    let mut biggest: Option<(u64, &City)> = None;
    let mut nearest: Option<(f64, &City)> = None;
    for city in gazetteer() {
        let d = haversine_km((city.lat, city.lon), (lat, lon));
        if d <= 60.0 && biggest.map(|(pop, _)| city.pop > pop).unwrap_or(true) {
            biggest = Some((city.pop, city));
        }
        if nearest.map(|(b, _)| d < b).unwrap_or(true) {
            nearest = Some((d, city));
        }
    }
    if let Some((_, city)) = biggest {
        return Some(city.display.clone());
    }
    let (d, city) = nearest?;
    (d <= 300.0).then(|| city.display.clone())
}

/// Great-circle distance in kilometres.
pub fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lon1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lon2) = (b.0.to_radians(), b.1.to_radians());
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * 6371.0 * h.sqrt().asin()
}

/// How much a photo's position agrees with the named place: 1 inside
/// its radius, fading to nothing by three radii out.
pub fn geo_affinity(place: &GeoMatch, lat: f64, lon: f64) -> f32 {
    let d = haversine_km((place.lat, place.lon), (lat, lon));
    if d <= place.radius_km {
        1.0
    } else if d >= place.radius_km * 3.0 {
        0.0
    } else {
        (1.0 - (d - place.radius_km) / (place.radius_km * 2.0)) as f32
    }
}
