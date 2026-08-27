//! Inspect and run Photoshop filter plug-ins from the command line.
//!
//! ```sh
//! cargo run -p schist-plugin-host-8bf --example 8bf -- inspect ~/Plug-Ins
//! cargo run -p schist-plugin-host-8bf --example 8bf -- apply Twirl.8bf in.ppm out.ppm
//! ```
//!
//! PPM is the image format here purely so the example needs no codec:
//! stage 1 is about the plug-in ABI, not about file formats.

use schist_plugin_host_8bf as bf;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("inspect") if args.len() == 2 => inspect(Path::new(&args[1])),
        Some("apply") if args.len() >= 4 => apply(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  \
                 8bf inspect <plug-in or directory>\n  \
                 8bf apply <plug-in> <in.ppm> <out.ppm> [--entry NAME] [--no-dialog]"
            );
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

type Res = Result<(), Box<dyn std::error::Error>>;

fn inspect(path: &Path) -> Res {
    let found = if path.is_dir() {
        bf::discover_dir(path)?
    } else {
        bf::inspect_file(path)?
    };
    if found.is_empty() {
        println!("no filter plug-ins found");
        return Ok(());
    }
    for f in &found {
        println!("{}", f.menu_name());
        println!("  file      {}", f.path.display());
        println!("  machine   {}", f.machine);
        if let Some((major, minor)) = f.pipl.version_pair() {
            println!("  interface {major}.{minor}");
        }
        println!("  code      {:?}", f.pipl.code_archs());
        if let Some(e) = &f.entry_point {
            println!("  entry     {e}");
        }
        if let Some(enbl) = f.pipl.enable_info() {
            println!("  enable    {enbl}");
        }
        match f.blocker() {
            Some(b) => println!("  BLOCKED   {b}"),
            None => println!("  runnable"),
        }
    }
    Ok(())
}

fn apply(args: &[String]) -> Res {
    let plugin = PathBuf::from(&args[0]);
    let input = PathBuf::from(&args[1]);
    let output = PathBuf::from(&args[2]);
    let mut entry_override = None;
    let mut show_dialog = true;
    let mut rest = args[3..].iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--entry" => entry_override = rest.next().cloned(),
            "--no-dialog" => show_dialog = false,
            other => return Err(format!("unknown option {other}").into()),
        }
    }

    let mut filter = match entry_override {
        // An explicit entry point skips the PiPL's code descriptor,
        // which is what rescues a plug-in whose PiPL is malformed — and
        // what lets this example drive a bare shared library.
        Some(entry) => {
            let pipl = bf::inspect_file(&plugin)
                .ok()
                .and_then(|f| f.into_iter().next())
                .map(|f| f.pipl)
                .unwrap_or_else(minimal_filter_pipl);
            bf::Filter::open(&plugin, pipl, &entry)?
        }
        None => {
            let found = bf::inspect_file(&plugin)?;
            let first = found.into_iter().next().ok_or("no filter in that file")?;
            if let Some(b) = first.blocker() {
                return Err(format!("{}: {b}", first.menu_name()).into());
            }
            first.load()?
        }
    };

    let mut image = read_ppm(&input)?;
    println!(
        "running {} over {}x{}",
        filter.name(),
        image.width,
        image.height
    );
    let opts = bf::RunOptions {
        show_dialog,
        progress: Some(Box::new(|done, total| {
            if total > 0 {
                eprint!("\r{:3}%", done * 100 / total);
            }
        })),
        ..Default::default()
    };
    filter.apply(&mut image, &opts)?;
    eprintln!("\rdone ");
    write_ppm(&output, &image)?;
    Ok(())
}

/// A property list claiming nothing but "this is a filter", for files
/// whose own PiPL could not be read.
fn minimal_filter_pipl() -> bf::Pipl {
    bf::Pipl {
        version: 0,
        endian: bf::Endian::Little,
        properties: vec![bf::pipl::Property {
            vendor: bf::abi::SIG_8BIM,
            key: bf::pipl::key::KIND,
            id: 0,
            data: bf::pipl::kind::FILTER.to_le_bytes().to_vec(),
        }],
    }
}

/// Binary PPM (P6), 8 bits per channel.
fn read_ppm(path: &Path) -> Result<bf::Image, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 && i < bytes.len() {
        if bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i].is_ascii_whitespace() {
            i += 1;
        } else {
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            fields.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
        }
    }
    if fields.first().map(String::as_str) != Some("P6") {
        return Err("only binary PPM (P6) input is supported".into());
    }
    let width: u32 = fields[1].parse()?;
    let height: u32 = fields[2].parse()?;
    if fields[3] != "255" {
        return Err("only 8-bit PPM is supported".into());
    }
    let pixels = &bytes[i + 1..];
    let want = width as usize * height as usize * 3;
    if pixels.len() < want {
        return Err("PPM is shorter than its header claims".into());
    }
    Ok(bf::Image {
        width,
        height,
        planes: 3,
        data: pixels[..want].to_vec(),
    })
}

fn write_ppm(path: &Path, image: &bf::Image) -> std::io::Result<()> {
    let mut out = format!("P6\n{} {}\n255\n", image.width, image.height).into_bytes();
    out.extend_from_slice(&image.data);
    std::fs::write(path, out)
}
