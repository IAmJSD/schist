//! Revision-bound downloads and safe names for provider-generated representations.
use crate::{
    auth,
    protocol::{map, parse, Capabilities, Value},
    Handle,
};
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
struct Ticket {
    url: String,
    revision: u64,
}
pub struct DownloadedAsset {
    pub bytes: Vec<u8>,
    pub revision: u64,
    pub content_type: Option<String>,
    pub content_disposition: Option<String>,
    pub format: Option<String>,
}
impl DownloadedAsset {
    pub fn suggested_name(&self, original: &str) -> String {
        if let Some(name) = self
            .content_disposition
            .as_deref()
            .and_then(disposition_filename)
        {
            return name;
        }
        let original = safe_filename(original).unwrap_or_else(|| "download".into());
        let mime = self
            .content_type
            .as_deref()
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let extension = match mime.as_str() {
            "image/vnd.adobe.photoshop" | "image/x-photoshop" => {
                Some(if self.bytes.get(4..6) == Some(&[0, 2]) {
                    "psb"
                } else {
                    "psd"
                })
            }
            "image/png" => Some("png"),
            "image/jpeg" => Some("jpg"),
            "image/webp" => Some("webp"),
            "image/tiff" => Some("tiff"),
            "image/heic" => Some("heic"),
            "image/heif" => Some("heif"),
            _ => self.format.as_deref().filter(|f| *f != "original"),
        };
        match extension {
            Some(ext) => std::path::Path::new(&original)
                .with_extension(ext)
                .to_string_lossy()
                .into_owned(),
            None => original,
        }
    }
}
pub fn valid_format(format: &str) -> bool {
    (1..=20).contains(&format.len())
        && format
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}
fn params(id: &str, format: Option<&str>, capabilities: Option<&Capabilities>) -> Result<Value> {
    let mut fields = vec![("id", id.into())];
    if let Some(format) = format {
        ensure!(valid_format(format), "Invalid download format");
        let capabilities = capabilities.context("Provider does not advertise export support")?;
        ensure!(
            if format == "original" {
                capabilities.original_download
            } else {
                capabilities.supports_export(format)
            },
            "Provider does not support the requested download format"
        );
        fields.push(("format", format.into()));
    }
    Ok(map(fields))
}
impl Handle {
    pub fn download_asset(
        &self,
        id: &str,
        format: Option<&str>,
        capabilities: Option<&Capabilities>,
    ) -> Result<DownloadedAsset> {
        let params = params(id, format, capabilities)?;
        download_with(format, || {
            parse(self.request("asset.download", params.clone())?)
        })
    }
}
fn download_with(
    format: Option<&str>,
    mut ticket: impl FnMut() -> Result<Ticket>,
) -> Result<DownloadedAsset> {
    for attempt in 0..3 {
        let ticket = ticket()?;
        match auth::download_response(&ticket.url, 512 * 1024 * 1024) {
            Ok(response) => {
                return Ok(DownloadedAsset {
                    bytes: response.bytes,
                    revision: ticket.revision,
                    content_type: response.content_type,
                    content_disposition: response.content_disposition,
                    format: format.map(str::to_owned),
                })
            }
            Err(error)
                if matches!(
                    error.downcast_ref::<ureq::Error>(),
                    Some(ureq::Error::StatusCode(409))
                ) =>
            {
                if attempt == 2 {
                    bail!("The cloud file kept changing during download; try again");
                }
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}
fn safe_filename(name: &str) -> Option<String> {
    let name = name.rsplit(['/', '\\']).next()?.trim();
    if name.is_empty() || name == "." || name == ".." || name.chars().any(char::is_control) {
        return None;
    }
    Some(name.replace([':', '*', '?', '"', '<', '>', '|'], "_"))
}
fn disposition_filename(header: &str) -> Option<String> {
    // Split parameters without treating semicolons inside a quoted filename as separators.
    let mut parts = Vec::new();
    let (mut quoted, mut escaped, mut part) = (false, false, String::new());
    for c in header.chars() {
        if escaped {
            part.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => parts.push(std::mem::take(&mut part)),
            _ => part.push(c),
        }
    }
    if quoted || escaped {
        return None;
    }
    parts.push(part);
    let mut plain = None;
    for part in parts.iter().skip(1) {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("filename*") {
            let Some((charset, rest)) = value.trim().split_once('\'') else {
                continue;
            };
            let Some((_, encoded)) = rest.split_once('\'') else {
                continue;
            };
            if !charset.eq_ignore_ascii_case("utf-8") {
                continue;
            }
            if let Some(decoded) = percent_decode(encoded).and_then(|s| safe_filename(&s)) {
                return Some(decoded);
            }
        } else if key.trim().eq_ignore_ascii_case("filename") {
            plain = safe_filename(value.trim());
        }
    }
    plain
}
fn percent_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut input = value.bytes();
    while let Some(c) = input.next() {
        bytes.push(if c == b'%' {
            let high = (input.next()? as char).to_digit(16)?;
            let low = (input.next()? as char).to_digit(16)?;
            (high * 16 + low) as u8
        } else {
            c
        });
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    fn http(statuses: Vec<u16>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            for (index, status) in statuses.into_iter().enumerate() {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with(&format!("GET /{} ", index + 1)));
                assert!(!request.to_ascii_lowercase().contains("authorization:"));
                assert!(!request.to_ascii_lowercase().contains("cookie:"));
                let headers = format!("HTTP/1.1 {status} Response\r\nContent-Length: 3\r\nContent-Type: image/vnd.adobe.photoshop\r\nContent-Disposition: attachment; filename*=UTF-8''edited%20image.psd\r\nConnection: close\r\n\r\n");
                socket.write_all(headers.as_bytes()).unwrap();
                socket.write_all(&[0, 128, 255]).unwrap();
            }
        });
        (base, worker)
    }
    #[test]
    fn download_refreshes_conflicting_ticket_and_keeps_metadata() {
        let (base, worker) = http(vec![409, 200]);
        let mut requests = 0;
        let result = download_with(None, || {
            requests += 1;
            Ok(Ticket {
                url: format!("{base}/{requests}"),
                revision: requests,
            })
        })
        .unwrap();
        worker.join().unwrap();
        assert_eq!(requests, 2);
        assert_eq!(result.revision, 2);
        assert_eq!(result.bytes, [0, 128, 255]);
        assert_eq!(
            result.content_type.as_deref(),
            Some("image/vnd.adobe.photoshop")
        );
        assert!(result.content_disposition.is_some());
        assert_eq!(result.suggested_name("original.heic"), "edited image.psd");
    }
    #[test]
    fn download_retry_is_bounded_and_does_not_retry_other_statuses() {
        for (statuses, count) in [(vec![409, 409, 409], 3), (vec![403], 1)] {
            let (base, worker) = http(statuses);
            let mut requests = 0;
            assert!(download_with(None, || {
                requests += 1;
                Ok(Ticket {
                    url: format!("{base}/{requests}"),
                    revision: requests,
                })
            })
            .is_err());
            worker.join().unwrap();
            assert_eq!(requests, count);
        }
    }
    #[test]
    fn explicit_formats_require_advertised_support_and_default_omits_format() {
        let caps: Capabilities = crate::protocol::parse(map([
            (
                "document_models",
                Value::Array(vec![crate::IMAGE_MODEL.into()]),
            ),
            (
                "formats",
                Value::Array(vec![map([
                    ("id", "codec.heif".into()),
                    ("name", "HEIC".into()),
                    ("extensions", Value::Array(vec!["heic".into()])),
                    ("can_export", false.into()),
                    ("runtime_requirement", Value::Nil),
                ])]),
            ),
            ("max_frame_bytes", (crate::MAX_FRAME as u64).into()),
            ("max_document_bytes", 128u64.into()),
            ("default_edited_export", "psd".into()),
            ("original_download", true.into()),
        ]))
        .unwrap();
        assert!(crate::protocol::field(&params("asset", None, None).unwrap(), "format").is_err());
        assert!(params("asset", Some("original"), Some(&caps)).is_ok());
        assert!(params("asset", Some("original"), None).is_err());
        assert!(params("asset", Some("heic"), Some(&caps)).is_err());
        for format in ["", ".png", "PNG", "../png", "a_b", "abcdefghijklmnopqrstu"] {
            assert!(!valid_format(format));
        }
    }
    #[test]
    fn filenames_use_disposition_then_content_type_without_directory_traversal() {
        assert_eq!(
            disposition_filename(
                "attachment; filename=plain.psd; filename*=UTF-8'en'%E2%98%83.psd"
            )
            .as_deref(),
            Some("☃.psd")
        );
        assert_eq!(
            disposition_filename("attachment; filename=\"a;b.psd\"").as_deref(),
            Some("a;b.psd")
        );
        assert_eq!(
            disposition_filename("attachment; filename*=UTF-8''..%2F..%2Fphoto.psd").as_deref(),
            Some("photo.psd")
        );
        assert!(disposition_filename("attachment; filename*=UTF-8''%00x.psd").is_none());
        let result = DownloadedAsset {
            bytes: vec![],
            revision: 1,
            content_type: Some("image/png; charset=binary".into()),
            content_disposition: None,
            format: None,
        };
        assert_eq!(result.suggested_name("original.heic"), "original.png");
    }
}
