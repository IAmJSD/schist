//! Position, capture time and city, from the EXIF, cached beside the
//! thumbnail as a three-line `.meta` file.

use crate::geo::nearest_city;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct PhotoMeta {
    pub gps: Option<(f64, f64)>,
    /// "YYYY-MM-DD HH:MM:SS": sortable as text, no calendar needed.
    pub taken: Option<String>,
    pub place: Option<String>,
}

/// One EXIF pass per photo, cached beside its thumbnail.
pub fn photo_meta(cache: &Option<PathBuf>, original: &Path) -> PhotoMeta {
    let meta_cache = cache.as_ref().map(|p| p.with_extension("meta"));
    if let Some(text) = meta_cache
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
    {
        let mut lines = text.lines();
        let gps = lines.next().and_then(|l| {
            let mut parts = l.split_whitespace().filter_map(|v| v.parse::<f64>().ok());
            match (parts.next(), parts.next()) {
                (Some(lat), Some(lon)) => Some((lat, lon)),
                _ => None,
            }
        });
        let field = |l: Option<&str>| {
            l.filter(|l| *l != "none" && !l.is_empty())
                .map(str::to_string)
        };
        return PhotoMeta {
            gps,
            taken: field(lines.next()),
            place: field(lines.next()),
        };
    }
    let data = exif_of(original);
    let gps = data.as_ref().and_then(gps_from);
    let taken = data.as_ref().and_then(datetime_from);
    let place = gps.and_then(|(lat, lon)| nearest_city(lat, lon));
    if let Some(path) = meta_cache {
        let line1 = match gps {
            Some((lat, lon)) => format!("{lat} {lon}"),
            None => "none".into(),
        };
        let text = format!(
            "{line1}\n{}\n{}",
            taken.as_deref().unwrap_or("none"),
            place.as_deref().unwrap_or("none")
        );
        let _ = std::fs::write(path, text);
    }
    PhotoMeta { gps, taken, place }
}

pub fn exif_of(path: &Path) -> Option<exif::Exif> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    exif::Reader::new().read_from_container(&mut reader).ok()
}

/// Latitude and longitude in degrees, from the GPS IFD.
pub fn gps_from(data: &exif::Exif) -> Option<(f64, f64)> {
    // Degrees/minutes/seconds as three rationals, hemisphere in the
    // companion Ref tag ("S"/"W" flip the sign).
    let axis = |tag: exif::Tag, ref_tag: exif::Tag, negative: char| -> Option<f64> {
        let field = data.get_field(tag, exif::In::PRIMARY)?;
        let exif::Value::Rational(parts) = &field.value else {
            return None;
        };
        if parts.is_empty() {
            return None;
        }
        let part = |i: usize| parts.get(i).map(|r| r.to_f64()).unwrap_or(0.0);
        let degrees = part(0) + part(1) / 60.0 + part(2) / 3600.0;
        let flip = data
            .get_field(ref_tag, exif::In::PRIMARY)
            .is_some_and(|f| f.display_value().to_string().contains(negative));
        Some(if flip { -degrees } else { degrees })
    };
    let lat = axis(exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef, 'S')?;
    let lon = axis(exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef, 'W')?;
    Some((lat, lon))
}

/// The capture time as sortable text, from DateTimeOriginal (else
/// DateTime): "YYYY:MM:DD HH:MM:SS" with the date's colons swapped out.
pub fn datetime_from(data: &exif::Exif) -> Option<String> {
    let field = data
        .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
        .or_else(|| data.get_field(exif::Tag::DateTime, exif::In::PRIMARY))?;
    let raw = field.display_value().to_string();
    let raw = raw.trim();
    // "2026:08:14 17:03:22" — sanity before trusting it to sort.
    if raw.len() < 10 || !raw.as_bytes()[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut normalized: Vec<u8> = raw.bytes().collect();
    if normalized.get(4) == Some(&b':') {
        normalized[4] = b'-';
    }
    if normalized.get(7) == Some(&b':') {
        normalized[7] = b'-';
    }
    String::from_utf8(normalized).ok()
}

/// A unix time as the same sortable text, for photos whose EXIF says
/// nothing — the file's own clock is better than no clock.
pub fn taken_from_unix(secs: u64) -> String {
    let (y, m, d) = ymd_from_unix(secs);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// Civil date from days-since-epoch (Howard Hinnant's algorithm).
pub fn ymd_from_unix(secs: u64) -> (i64, u32, u32) {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bytes_parser_agrees_with_the_path_parser() {
        // A minimal JPEG carrying an APP1 EXIF segment with a Make tag.
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II\x2a\x00\x08\x00\x00\x00");
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x010fu16.to_le_bytes()); // Make
        tiff.extend_from_slice(&2u16.to_le_bytes()); // ASCII
        tiff.extend_from_slice(&6u32.to_le_bytes());
        tiff.extend_from_slice(&26u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(b"Apple\0");
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe1];
        jpeg.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&app1);
        jpeg.extend_from_slice(&[0xff, 0xd9]);
        let dir = std::env::temp_dir().join(format!("schist-exif-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("make.jpg");
        std::fs::write(&path, &jpeg).unwrap();
        let by_path = exif_summary(&path).expect("path parse");
        let by_bytes = exif_summary_bytes(&jpeg).expect("bytes parse");
        assert_eq!(by_path.make.as_deref(), Some("Apple"));
        assert_eq!(by_path.make, by_bytes.make);
        assert_eq!(by_path.gps, by_bytes.gps);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unix_times_become_civil_dates() {
        // 2026-09-01 00:00:00 UTC, checked against `date -u`.
        assert_eq!(ymd_from_unix(1_788_220_800), (2026, 9, 1));
        assert_eq!(taken_from_unix(1_788_220_800 + 3661), "2026-09-01 01:01:01");
        assert_eq!(ymd_from_unix(0), (1970, 1, 1));
    }
}

/// What a photo's EXIF says that a person would want to read: the
/// camera, the exposure, when and where. Every field optional — a PNG
/// from a screenshot has none of it, a phone photo has all of it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExifSummary {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub software: Option<String>,
    /// "1/250 s", "2 s".
    pub exposure: Option<String>,
    /// "f/1.8".
    pub aperture: Option<String>,
    pub iso: Option<u32>,
    /// "26 mm", with the 35 mm equivalent when the file gives one.
    pub focal_length: Option<String>,
    /// Sortable "YYYY-MM-DD HH:MM:SS".
    pub taken: Option<String>,
    pub gps: Option<(f64, f64)>,
    pub altitude_m: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub orientation: Option<u32>,
    pub flash: Option<bool>,
    pub white_balance: Option<String>,
    pub exposure_bias: Option<String>,
    pub metering: Option<String>,
}

impl ExifSummary {
    /// Whether there is anything at all worth a panel.
    pub fn is_empty(&self) -> bool {
        *self == ExifSummary::default()
    }

    /// "Apple iPhone 15 Pro" — make and model, without the make said
    /// twice when the model already starts with it.
    pub fn camera(&self) -> Option<String> {
        match (&self.make, &self.model) {
            (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => {
                Some(model.clone())
            }
            (Some(make), Some(model)) => Some(format!("{make} {model}")),
            (None, Some(model)) => Some(model.clone()),
            (Some(make), None) => Some(make.clone()),
            (None, None) => None,
        }
    }
}

/// Read the summary out of a file, `None` when it carries no EXIF.
pub fn exif_summary(path: &Path) -> Option<ExifSummary> {
    summarize(exif_of(path)?)
}

/// The same from the file's bytes, for where there is no file system
/// to read — the web build keeps its opened files in memory.
pub fn exif_summary_bytes(bytes: &[u8]) -> Option<ExifSummary> {
    let mut reader = std::io::Cursor::new(bytes);
    summarize(exif::Reader::new().read_from_container(&mut reader).ok()?)
}

fn summarize(data: exif::Exif) -> Option<ExifSummary> {
    let text = |tag: exif::Tag| -> Option<String> {
        let field = data.get_field(tag, exif::In::PRIMARY)?;
        let s = field.display_value().with_unit(&data).to_string();
        let s = s.trim().trim_matches('"').trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    let rational = |tag: exif::Tag| -> Option<f64> {
        let field = data.get_field(tag, exif::In::PRIMARY)?;
        match &field.value {
            exif::Value::Rational(v) => v.first().map(|r| r.to_f64()),
            exif::Value::SRational(v) => v.first().map(|r| r.to_f64()),
            _ => None,
        }
    };
    let uint = |tag: exif::Tag| -> Option<u32> {
        data.get_field(tag, exif::In::PRIMARY)?.value.get_uint(0)
    };
    let exposure = rational(exif::Tag::ExposureTime).map(|t| {
        if t > 0.0 && t < 1.0 {
            format!("1/{} s", (1.0 / t).round() as u64)
        } else {
            format!("{t} s")
        }
    });
    let aperture = rational(exif::Tag::FNumber).map(|f| format!("f/{f:.1}"));
    let focal_length =
        rational(exif::Tag::FocalLength).map(|mm| match uint(exif::Tag::FocalLengthIn35mmFilm) {
            Some(eq) if (eq as f64 - mm).abs() > 0.5 => format!("{mm:.0} mm ({eq} mm equiv.)"),
            _ => format!("{mm:.0} mm"),
        });
    let exposure_bias = rational(exif::Tag::ExposureBiasValue)
        .filter(|b| b.abs() > 0.01)
        .map(|b| format!("{b:+.1} EV"));
    let flash = uint(exif::Tag::Flash).map(|f| f & 1 == 1);
    let gps = gps_from(&data);
    let altitude_m = rational(exif::Tag::GPSAltitude).map(|alt| {
        // Ref 1 = below sea level.
        if uint(exif::Tag::GPSAltitudeRef) == Some(1) {
            -alt
        } else {
            alt
        }
    });
    let summary = ExifSummary {
        make: text(exif::Tag::Make),
        model: text(exif::Tag::Model),
        lens: text(exif::Tag::LensModel),
        software: text(exif::Tag::Software),
        exposure,
        aperture,
        iso: uint(exif::Tag::PhotographicSensitivity),
        focal_length,
        taken: datetime_from(&data),
        gps,
        altitude_m,
        width: uint(exif::Tag::PixelXDimension).or_else(|| uint(exif::Tag::ImageWidth)),
        height: uint(exif::Tag::PixelYDimension).or_else(|| uint(exif::Tag::ImageLength)),
        orientation: uint(exif::Tag::Orientation),
        flash,
        white_balance: text(exif::Tag::WhiteBalance),
        metering: text(exif::Tag::MeteringMode),
        exposure_bias,
    };
    (!summary.is_empty()).then_some(summary)
}
