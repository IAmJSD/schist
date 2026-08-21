//! Example third-party codec: binary PGM (Netpbm P5), a grayscale format
//! Schist has no built-in support for.
//!
//! Build: `cargo build --release --target wasm32-unknown-unknown -p schist-example-pgm`

use schist_plugin_sdk::*;

schist_codec! {
    id: "com.example.pgm",
    name: "Netpbm PGM",
    extensions: ["pgm"],
    probe: |bytes: &[u8]| bytes.starts_with(b"P5"),
    decode: |bytes: &[u8]| -> Option<(u32, u32, Vec<u8>)> {
        if !bytes.starts_with(b"P5") {
            return None;
        }
        // Header: "P5", width, height, maxval — whitespace separated, with
        // '#' comments allowed between fields.
        let mut fields = Vec::new();
        let mut pos = 2usize;
        while fields.len() < 3 && pos < bytes.len() {
            match bytes[pos] {
                b'#' => {
                    while pos < bytes.len() && bytes[pos] != b'\n' {
                        pos += 1;
                    }
                }
                c if c.is_ascii_whitespace() => pos += 1,
                _ => {
                    let start = pos;
                    while pos < bytes.len() && !bytes[pos].is_ascii_whitespace() {
                        pos += 1;
                    }
                    let text = std::str::from_utf8(&bytes[start..pos]).ok()?;
                    fields.push(text.parse::<u32>().ok()?);
                }
            }
        }
        if fields.len() < 3 {
            return None;
        }
        // Exactly one whitespace byte separates the header from the data.
        pos += 1;
        let (width, height, maxval) = (fields[0], fields[1], fields[2].max(1));
        let count = (width as usize).checked_mul(height as usize)?;
        if width == 0 || height == 0 || bytes.len() < pos + count {
            return None;
        }
        log(&format!("decoding {width}x{height} PGM"));
        let mut rgba = Vec::with_capacity(count * 4);
        for &v in &bytes[pos..pos + count] {
            let scaled = ((v as u32 * 255) / maxval).min(255) as u8;
            rgba.extend_from_slice(&[scaled, scaled, scaled, 255]);
        }
        Some((width, height, rgba))
    }
}
