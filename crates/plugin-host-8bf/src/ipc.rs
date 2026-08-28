//! The wire between Schist and a plug-in helper process.
//!
//! Deliberately small and hand-rolled. The helper is cross-compiled to
//! four targets and runs under Wine and Rosetta, so every dependency it
//! carries is a dependency that has to build and work in all of those;
//! a length-prefixed frame with a tag byte needs none.
//!
//! Pixels do **not** travel over this. They live in a file both
//! processes map, so the image is written once and read once however
//! large it is. This carries the request and the progress reports.
//!
//! The control channel is a loopback TCP socket rather than a Unix
//! socket or a pipe, because it has to work identically for a native
//! helper, a Windows helper under Wine, and an Intel helper under
//! Rosetta. The host listens and the helper connects back, so the helper
//! needs no address of its own. A random token, sent first, keeps
//! another local process from talking to a socket it happened to find.

use std::io::{self, Read, Write};

/// Frames larger than this are a protocol error rather than an
/// allocation: the largest legitimate message is a PiPL, which is
/// kilobytes.
const MAX_FRAME: u32 = 4 * 1024 * 1024;

pub const TAG_HELLO: u8 = 1;
pub const TAG_RUN: u8 = 2;
pub const TAG_PROGRESS: u8 = 3;
pub const TAG_FINISHED: u8 = 4;
pub const TAG_LOG: u8 = 5;

/// What the host asks the helper to do. One per run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    /// The plug-in, as a path the *helper* can open — which under Wine
    /// is a Windows-form path.
    pub plugin: String,
    pub entry: String,
    /// The raw PiPL resource, so the helper parses the same metadata the
    /// host discovered rather than being told a summary of it.
    pub pipl: Vec<u8>,
    /// The shared pixel file, again as the helper sees it.
    pub pixels: String,
    pub width: u32,
    pub height: u32,
    pub planes: u16,
    pub show_dialog: bool,
    pub foreground: [u8; 4],
    pub background: [u8; 4],
    /// Empty for "the document has no name".
    pub title: String,
}

/// What the helper says back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    Hello {
        token: String,
    },
    Progress {
        done: i32,
        total: i32,
    },
    /// `code` is zero on success; `message` carries the plug-in's own
    /// words where it gave any.
    Finished {
        code: i32,
        message: String,
    },
    Log {
        text: String,
    },
}

// --- framing -------------------------------------------------------------

pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes is beyond anything this protocol sends"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// --- encoding ------------------------------------------------------------

struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.u32(v as u32)
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        self
    }
    fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
}

struct Dec<'a>(&'a [u8]);

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "message ended mid-field",
            ));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(self.u32()? as i32)
    }
    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> io::Result<String> {
        let b = self.bytes()?;
        String::from_utf8(b)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "string was not valid UTF-8"))
    }
    fn arr4(&mut self) -> io::Result<[u8; 4]> {
        Ok(self.take(4)?.try_into().unwrap())
    }
}

impl RunRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc(Vec::new());
        e.u8(TAG_RUN)
            .str(&self.plugin)
            .str(&self.entry)
            .bytes(&self.pipl)
            .str(&self.pixels)
            .u32(self.width)
            .u32(self.height)
            .u32(self.planes as u32)
            .u8(u8::from(self.show_dialog));
        e.0.extend_from_slice(&self.foreground);
        e.0.extend_from_slice(&self.background);
        e.str(&self.title);
        e.0
    }

    pub fn decode(payload: &[u8]) -> io::Result<RunRequest> {
        let mut d = Dec(payload);
        if d.u8()? != TAG_RUN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a run request",
            ));
        }
        Ok(RunRequest {
            plugin: d.str()?,
            entry: d.str()?,
            pipl: d.bytes()?,
            pixels: d.str()?,
            width: d.u32()?,
            height: d.u32()?,
            planes: d.u32()? as u16,
            show_dialog: d.u8()? != 0,
            foreground: d.arr4()?,
            background: d.arr4()?,
            title: d.str()?,
        })
    }
}

impl Report {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc(Vec::new());
        match self {
            Report::Hello { token } => {
                e.u8(TAG_HELLO).str(token);
            }
            Report::Progress { done, total } => {
                e.u8(TAG_PROGRESS).i32(*done).i32(*total);
            }
            Report::Finished { code, message } => {
                e.u8(TAG_FINISHED).i32(*code).str(message);
            }
            Report::Log { text } => {
                e.u8(TAG_LOG).str(text);
            }
        }
        e.0
    }

    pub fn decode(payload: &[u8]) -> io::Result<Report> {
        let mut d = Dec(payload);
        Ok(match d.u8()? {
            TAG_HELLO => Report::Hello { token: d.str()? },
            TAG_PROGRESS => Report::Progress {
                done: d.i32()?,
                total: d.i32()?,
            },
            TAG_FINISHED => Report::Finished {
                code: d.i32()?,
                message: d.str()?,
            },
            TAG_LOG => Report::Log { text: d.str()? },
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown message tag {other}"),
                ))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RunRequest {
        RunRequest {
            plugin: r"Z:\tmp\Twirl.8bf".into(),
            entry: "PluginMain".into(),
            pipl: vec![1, 0, 0, 0, 9, 9],
            pixels: r"Z:\tmp\pix.bin".into(),
            width: 1920,
            height: 1080,
            planes: 3,
            show_dialog: true,
            foreground: [1, 2, 3, 4],
            background: [250, 251, 252, 253],
            title: "seaside.psd".into(),
        }
    }

    #[test]
    fn a_run_request_survives_the_wire() {
        let r = sample();
        assert_eq!(RunRequest::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn every_report_survives_the_wire() {
        for r in [
            Report::Hello {
                token: "cafef00d".into(),
            },
            Report::Progress {
                done: 7,
                total: 100,
            },
            Report::Finished {
                code: -30101,
                message: "cannot filter this mode".into(),
            },
            Report::Log {
                text: "handle.new(8)".into(),
            },
        ] {
            assert_eq!(Report::decode(&r.encode()).unwrap(), r);
        }
    }

    #[test]
    fn frames_round_trip_and_refuse_absurd_lengths() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        assert_eq!(read_frame(&mut buf.as_slice()).unwrap(), b"hello");

        let mut absurd = (MAX_FRAME + 1).to_le_bytes().to_vec();
        absurd.extend_from_slice(b"...");
        let err = read_frame(&mut absurd.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_truncated_message_is_an_error_not_a_panic() {
        let full = sample().encode();
        for cut in [1, 5, 20, full.len() - 1] {
            assert!(RunRequest::decode(&full[..cut]).is_err(), "cut at {cut}");
        }
        assert!(Report::decode(&[]).is_err());
        assert!(Report::decode(&[99]).is_err());
    }
}
