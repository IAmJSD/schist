//! One bounded request per process. stdin/stdout are binary; diagnostics use stderr.
//! `import <filename>` reads original file bytes and emits a MessagePack map with
//! `state` (Yjs bin). `export <extension>` reads Yjs bytes and emits `bytes` (bin).
//! `validate` reconstructs a Yjs image; `formats` lists built-in codec capabilities.
use anyhow::{bail, ensure, Context, Result};
use schist_document::{export, formats, import, materialize, registry, SharedDocument, MODEL};
use serde::Serialize;
use std::io::{Read, Write};
const MAX_INPUT: u64 = 256 * 1024 * 1024;
#[derive(Serialize)]
struct Reply {
    model: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<serde_bytes::ByteBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<serde_bytes::ByteBuf>,
    width: u32,
    height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    extension: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<&'static str>,
}
fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let registry = registry();
    if args.first().map(String::as_str) == Some("formats") {
        std::io::stdout().write_all(&rmp_serde::to_vec_named(&formats(&registry))?)?;
        return Ok(());
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_INPUT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_INPUT,
        "Worker input exceeds 256 MiB"
    );
    let operation = args
        .first()
        .context("Expected import, export, validate, or formats")?;
    let document = if operation == "import" {
        import(
            &registry,
            &bytes,
            args.get(1).context("Import needs a file name")?,
        )?
    } else {
        materialize(&bytes)?
    };
    let mut reply = Reply {
        model: MODEL,
        state: None,
        bytes: None,
        width: document.width,
        height: document.height,
        extension: None,
        mime_type: None,
    };
    match operation.as_str() {
        "import" => reply.state = Some(SharedDocument::new(&document)?.full_state().into()),
        "export" => {
            let result = export(
                &registry,
                &document,
                args.get(1).map(String::as_str).unwrap_or("psd"),
            )?;
            reply.bytes = Some(result.bytes.into());
            reply.extension = Some(result.extension);
            reply.mime_type = Some(result.mime_type);
        }
        "validate" => {}
        _ => bail!("Unknown operation"),
    }
    let output = rmp_serde::to_vec_named(&reply)?;
    ensure!(
        output.len() as u64 <= MAX_INPUT + 1024,
        "Worker output exceeds 256 MiB"
    );
    std::io::stdout().write_all(&output)?;
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
