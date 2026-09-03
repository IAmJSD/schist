//! Lossless JPEG (ITU T.81 process 14, SOF3), the compression of DNG,
//! Canon CR2, Pentax, Kodak, Hasselblad and Leaf raws.
//!
//! Handles 2–16-bit precision, 1–4 components, all seven predictors,
//! point transforms, restart intervals, and the two habits of
//! camera-written streams: Canon's SOF3 that declares two or four
//! components over a width that is a slice of the sensor, and DNG
//! tiles whose components are simply interleaved pixels.
//!
//! Both habits need no special case here. A lossless JPEG scan is a
//! plain raster of `width * components` samples a row, and every
//! camera that plays games with the component count is only choosing
//! how to group the sensor's samples into that raster: Canon splits a
//! row into two or four interleaved slices, Adobe's CFA tiles
//! interleave two adjacent sensor columns. Handing the caller the
//! samples in stream order is therefore the only useful thing to do —
//! un-slicing needs the container's tags, which this module does not
//! see.

use crate::bits::{BitPumpJpeg, HuffTable};
use crate::{Error, Result};

/// A decoded frame: `width * height * components` samples, row-major,
/// components interleaved, exactly as the stream orders them.
#[derive(Debug, Clone, PartialEq)]
pub struct LjpegImage {
    pub width: usize,
    pub height: usize,
    pub components: usize,
    pub precision: u32,
    pub data: Vec<u16>,
}

/// A ceiling on what a frame header may claim, so a corrupt or hostile
/// two-byte dimension cannot ask for a terabyte. 256 M samples is far
/// past the largest sensor anyone has shipped (a 100 MP back's CFA
/// frame is 0.1 G) while still being a half-gigabyte allocation at
/// worst.
const MAX_SAMPLES: usize = 1 << 28;

/// Everything the markers before the entropy-coded data say.
struct Header {
    precision: u32,
    width: usize,
    height: usize,
    components: usize,
    /// SOS's `Ss`: which of T.81's seven prediction rules the scan
    /// uses away from the first row and column.
    predictor: u8,
    /// SOS's `Al`: the encoder right-shifted every sample by this
    /// much, so decoded samples are shifted back on the way out.
    point_transform: u32,
    /// DRI's interval in MCUs; 0 when the stream has no restarts.
    restart_interval: usize,
    /// Up to four DHT tables, by table id.
    tables: [Option<HuffTable>; 4],
    /// Which table each scan component reads its differences with.
    component_tables: [usize; 4],
    /// Offset of the first entropy-coded byte.
    scan_start: usize,
}

/// Decode a complete lossless JPEG stream (SOI through EOI; trailing
/// bytes ignored).
pub fn decode(bytes: &[u8]) -> Result<LjpegImage> {
    let header = parse(bytes, false)?;
    let samples = header
        .width
        .checked_mul(header.height)
        .and_then(|n| n.checked_mul(header.components))
        .filter(|n| *n <= MAX_SAMPLES)
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "lossless JPEG frame of {}x{}x{} is larger than this decoder allows",
                header.width, header.height, header.components
            ))
        })?;

    // The shortest a symbol can be is one bit, so a scan that holds
    // fewer bits than the frame has samples is truncated (or lying)
    // and there is no point allocating for it.
    let scan = &bytes[header.scan_start..];
    if (scan.len() as u64).saturating_mul(8) < samples as u64 {
        return Err(Error::Corrupt(format!(
            "lossless JPEG scan holds {} bytes for {samples} samples",
            scan.len()
        )));
    }

    let tables = scan_tables(&header)?;
    let mut data = vec![0u16; samples];
    // The predictor is fixed for the whole scan, so it is chosen once
    // here and compiled into the sample loop rather than tested per
    // sample.
    match header.predictor {
        1 => decode_scan::<1>(scan, &header, &tables, &mut data)?,
        2 => decode_scan::<2>(scan, &header, &tables, &mut data)?,
        3 => decode_scan::<3>(scan, &header, &tables, &mut data)?,
        4 => decode_scan::<4>(scan, &header, &tables, &mut data)?,
        5 => decode_scan::<5>(scan, &header, &tables, &mut data)?,
        6 => decode_scan::<6>(scan, &header, &tables, &mut data)?,
        7 => decode_scan::<7>(scan, &header, &tables, &mut data)?,
        p => {
            return Err(Error::Unsupported(format!(
                "lossless JPEG predictor {p} (0 is the differential/hierarchical mode)"
            )))
        }
    }

    // T.81's point transform: the encoder divided every sample by
    // 2^Al, and prediction runs on those divided values, so the shift
    // back happens only once the whole scan is decoded. Cameras all
    // use Al = 0.
    if header.point_transform > 0 {
        for sample in &mut data {
            *sample <<= header.point_transform;
        }
    }

    Ok(LjpegImage {
        width: header.width,
        height: header.height,
        components: header.components,
        precision: header.precision,
        data,
    })
}

/// The frame header alone (dimensions, components, precision), for
/// callers that size their output before decoding. `data` is empty.
pub fn header(bytes: &[u8]) -> Result<LjpegImage> {
    let header = parse(bytes, true)?;
    Ok(LjpegImage {
        width: header.width,
        height: header.height,
        components: header.components,
        precision: header.precision,
        data: Vec::new(),
    })
}

/// The scan's Huffman tables, one borrow per component.
fn scan_tables(header: &Header) -> Result<Vec<&HuffTable>> {
    (0..header.components)
        .map(|c| {
            let id = header.component_tables[c];
            header.tables[id].as_ref().ok_or_else(|| {
                Error::Corrupt(format!(
                    "scan component {c} uses Huffman table {id}, which the file never defines"
                ))
            })
        })
        .collect()
}

/// Big-endian `u16` at `at`, the only integer width JPEG headers use.
fn be16(bytes: &[u8], at: usize) -> Result<usize> {
    match bytes.get(at..at + 2) {
        Some(b) => Ok(((b[0] as usize) << 8) | b[1] as usize),
        None => Err(Error::Corrupt("JPEG header ends mid-value".into())),
    }
}

/// Walk the marker segments up to (and including) SOS. With
/// `header_only` the tables are still read — they cost little and a
/// caller asking only for the size does not pay for the scan.
fn parse(bytes: &[u8], header_only: bool) -> Result<Header> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return Err(Error::Corrupt("not a JPEG stream: no SOI".into()));
    }
    let mut precision = 0u32;
    let mut width = 0usize;
    let mut height = 0usize;
    let mut components = 0usize;
    let mut ids = [0u8; 4];
    let mut restart_interval = 0usize;
    let mut tables: [Option<HuffTable>; 4] = [None, None, None, None];

    let mut at = 2usize;
    loop {
        // Segments are separated by a 0xFF; any number of extra 0xFF
        // fill bytes may precede the marker itself.
        let Some(&lead) = bytes.get(at) else {
            return Err(Error::Corrupt("JPEG stream ends before its scan".into()));
        };
        if lead != 0xFF {
            return Err(Error::Corrupt(format!(
                "expected a JPEG marker at {at}, found {lead:#04x}"
            )));
        }
        at += 1;
        while bytes.get(at) == Some(&0xFF) {
            at += 1;
        }
        let Some(&marker) = bytes.get(at) else {
            return Err(Error::Corrupt("JPEG stream ends on a marker".into()));
        };
        at += 1;

        match marker {
            // Standalone markers: no payload.
            0xD8 | 0x01 | 0xD0..=0xD7 => continue,
            0xD9 => {
                return Err(Error::Corrupt(
                    "JPEG stream ends (EOI) before its scan".into(),
                ))
            }
            _ => {}
        }
        let length = be16(bytes, at)?;
        if length < 2 {
            return Err(Error::Corrupt(format!(
                "JPEG segment {marker:#04x} has length {length}"
            )));
        }
        let payload = bytes.get(at + 2..at + length).ok_or_else(|| {
            Error::Corrupt(format!(
                "JPEG segment {marker:#04x} runs past the end of the file"
            ))
        })?;
        let next = at + length;

        match marker {
            // SOF3: the lossless frame header.
            0xC3 => {
                if payload.len() < 6 {
                    return Err(Error::Corrupt("SOF3 segment is too short".into()));
                }
                precision = payload[0] as u32;
                height = ((payload[1] as usize) << 8) | payload[2] as usize;
                width = ((payload[3] as usize) << 8) | payload[4] as usize;
                components = payload[5] as usize;
                if !(2..=16).contains(&precision) {
                    return Err(Error::Unsupported(format!(
                        "lossless JPEG precision {precision}"
                    )));
                }
                if height == 0 {
                    // A zero height means the real one arrives in a
                    // DNL marker after the scan. No camera writes
                    // that, and honouring it means decoding blind.
                    return Err(Error::Unsupported(
                        "lossless JPEG with the height deferred to DNL".into(),
                    ));
                }
                if width == 0 {
                    return Err(Error::Corrupt("SOF3 declares a zero width".into()));
                }
                if !(1..=4).contains(&components) {
                    return Err(Error::Unsupported(format!(
                        "lossless JPEG with {components} components"
                    )));
                }
                if payload.len() < 6 + components * 3 {
                    return Err(Error::Corrupt(
                        "SOF3 segment is shorter than its component list".into(),
                    ));
                }
                for c in 0..components {
                    let spec = &payload[6 + c * 3..9 + c * 3];
                    ids[c] = spec[0];
                    // Subsampling has no meaning for a lossless frame
                    // and nothing writes it, but a file that claims it
                    // would decode to nonsense if it were ignored.
                    if spec[1] != 0x11 {
                        // Canon's sRAW and mRAW are the only raw
                        // streams that do this: a subsampled YCbCr
                        // lossless frame. Decoding it needs the MCU
                        // interleave rules and a chroma upsampler,
                        // neither of which belongs here yet.
                        return Err(Error::Unsupported(format!(
                            "lossless JPEG component {c} with sampling factors {:#04x} (Canon sRAW/mRAW)",
                            spec[1]
                        )));
                    }
                }
            }
            // Any other SOFn is a different JPEG process entirely.
            0xC0..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => {
                return Err(Error::Unsupported(format!(
                    "JPEG process SOF{} ({marker:#04x}); only lossless SOF3 belongs in a raw file",
                    marker & 0x0F
                )));
            }
            // DHT: one or more tables in the one segment.
            0xC4 => {
                let mut p = 0usize;
                while p < payload.len() {
                    let class_and_id = payload[p];
                    let id = (class_and_id & 0x0F) as usize;
                    if class_and_id >> 4 != 0 {
                        return Err(Error::Unsupported(
                            "an AC Huffman table in a lossless JPEG (only DC tables are used)"
                                .into(),
                        ));
                    }
                    if id >= 4 {
                        return Err(Error::Corrupt(format!(
                            "Huffman table id {id} is outside 0..=3"
                        )));
                    }
                    let counts = payload.get(p + 1..p + 17).ok_or_else(|| {
                        Error::Corrupt("DHT segment ends inside its code lengths".into())
                    })?;
                    let total: usize = counts.iter().map(|c| *c as usize).sum();
                    let symbols = payload.get(p + 17..p + 17 + total).ok_or_else(|| {
                        Error::Corrupt("DHT segment ends inside its symbols".into())
                    })?;
                    // A caller after the frame's shape alone does
                    // not need the codes built.
                    if !header_only {
                        tables[id] = Some(HuffTable::new(counts, symbols)?);
                    }
                    p += 17 + total;
                }
            }
            // DRI: how many MCUs between restart markers.
            0xDD => {
                restart_interval = be16(payload, 0)?;
            }
            // SOS: the scan header, and then the entropy-coded data.
            0xDA => {
                if width == 0 {
                    return Err(Error::Corrupt("SOS before SOF3".into()));
                }
                let ns = *payload
                    .first()
                    .ok_or_else(|| Error::Corrupt("empty SOS segment".into()))?
                    as usize;
                if ns != components {
                    // Non-interleaved scans (one component per scan)
                    // are legal but nothing writes them for raw.
                    return Err(Error::Unsupported(format!(
                        "a lossless JPEG scan over {ns} of the frame's {components} components"
                    )));
                }
                if payload.len() < 1 + ns * 2 + 3 {
                    return Err(Error::Corrupt("SOS segment is too short".into()));
                }
                let mut component_tables = [0usize; 4];
                for c in 0..ns {
                    let selector = payload[1 + c * 2];
                    let table = (payload[2 + c * 2] >> 4) as usize;
                    if table >= 4 {
                        return Err(Error::Corrupt(format!(
                            "scan component {c} names Huffman table {table}"
                        )));
                    }
                    // The scan lists components by the ids SOF3 gave
                    // them; they are normally in frame order, but a
                    // permuted scan is legal.
                    let index = ids[..components]
                        .iter()
                        .position(|id| *id == selector)
                        .ok_or_else(|| {
                            Error::Corrupt(format!(
                                "scan names component {selector}, which the frame does not have"
                            ))
                        })?;
                    component_tables[index] = table;
                }
                let tail = &payload[1 + ns * 2..];
                let predictor = tail[0];
                let point_transform = (tail[2] & 0x0F) as u32;
                if point_transform >= precision {
                    return Err(Error::Corrupt(format!(
                        "point transform {point_transform} with {precision}-bit samples"
                    )));
                }
                return Ok(Header {
                    precision,
                    width,
                    height,
                    components,
                    predictor,
                    point_transform,
                    restart_interval,
                    tables,
                    component_tables,
                    scan_start: next,
                });
            }
            // APPn, COM, DQT, DNL and the rest: not our business.
            _ => {}
        }
        at = next;
    }
}

/// T.81's seven prediction rules, over the sample to the left (`a`),
/// the one above (`b`) and the one above-left (`c`). The halvings are
/// arithmetic shifts, as the specification defines them.
#[inline(always)]
fn predict<const P: u8>(a: i32, b: i32, c: i32) -> i32 {
    match P {
        1 => a,
        2 => b,
        3 => c,
        4 => a + b - c,
        5 => a + ((b - c) >> 1),
        6 => b + ((a - c) >> 1),
        _ => (a + b) >> 1,
    }
}

/// Decode the whole scan, restart interval by restart interval.
fn decode_scan<const P: u8>(
    scan: &[u8],
    header: &Header,
    tables: &[&HuffTable],
    out: &mut [u16],
) -> Result<()> {
    let mcus = header.width * header.height;
    let interval = if header.restart_interval == 0 {
        mcus
    } else {
        header.restart_interval
    };
    // The prediction every restart interval (and the frame) starts
    // from: half of full scale, so the first difference is centred.
    let initial = 1i32 << (header.precision - header.point_transform - 1);

    let mut pump = BitPumpJpeg::new(scan);
    // Where `pump`'s slice starts inside `scan`, so the search for the
    // next RSTn can be expressed in `scan`'s offsets.
    let mut base = 0usize;
    let mut mcu = 0usize;
    // RSTn markers count 0..=7 and wrap; a stream that lost an
    // interval would otherwise resync on the next marker and come out
    // shifted by a row with no error.
    let mut restart_index = 0u8;
    while mcu < mcus {
        let end = (mcu + interval).min(mcus);
        decode_interval::<P>(&mut pump, header, tables, out, mcu, end, initial);
        mcu = end;
        if mcu < mcus {
            // The encoder padded to a byte boundary and wrote RSTn;
            // the bits after it start a fresh accumulator, and the
            // predictors reset.
            base = find_restart(scan, base + pump.byte_pos(), restart_index)?;
            restart_index = (restart_index + 1) % 8;
            pump = BitPumpJpeg::new(&scan[base..]);
        }
    }
    Ok(())
}

/// One restart interval: MCUs `start..end` in raster order. An MCU of
/// a lossless scan is one sample of each component, so the MCU index
/// is the pixel index.
#[allow(clippy::too_many_arguments)]
fn decode_interval<const P: u8>(
    pump: &mut BitPumpJpeg<'_>,
    header: &Header,
    tables: &[&HuffTable],
    out: &mut [u16],
    start: usize,
    end: usize,
    initial: i32,
) {
    let width = header.width;
    let ncomp = header.components;
    let stride = width * ncomp;

    let mut mcu = start;
    // The first sample of an interval has no usable neighbours: T.81
    // predicts it from half scale, wherever in the raster it falls.
    let mut first = true;
    while mcu < end {
        let y = mcu / width;
        let row_end = end.min((y + 1) * width);
        let mut idx = mcu * ncomp;

        if first || mcu.is_multiple_of(width) {
            for table in tables.iter().take(ncomp) {
                let diff = table.decode_diff(pump);
                // Left of the first column there is nothing, so the
                // rule is "predict from above" (T.81's predictor 2).
                let prediction = if first {
                    initial
                } else {
                    out[idx - stride] as i32
                };
                out[idx] = (prediction + diff) as u16;
                idx += 1;
            }
            first = false;
            mcu += 1;
            if mcu >= row_end {
                continue;
            }
        }

        let count = row_end - mcu;
        if y == 0 {
            // The top row has nothing above it: predictor 1, always.
            for _ in 0..count {
                for table in tables.iter().take(ncomp) {
                    let diff = table.decode_diff(pump);
                    out[idx] = (out[idx - ncomp] as i32 + diff) as u16;
                    idx += 1;
                }
            }
        } else {
            for _ in 0..count {
                for table in tables.iter().take(ncomp) {
                    let diff = table.decode_diff(pump);
                    let a = out[idx - ncomp] as i32;
                    let b = out[idx - stride] as i32;
                    let c = out[idx - stride - ncomp] as i32;
                    // Wrapping to sixteen bits is what T.81 means by
                    // "modulo 65536": the difference was taken that
                    // way, and the sample fits in `precision` bits
                    // again once it wraps back.
                    out[idx] = (predict::<P>(a, b, c) + diff) as u16;
                    idx += 1;
                }
            }
        }
        mcu = row_end;
    }
}

/// The offset just past the next `RSTn` marker at or after `from`.
/// Entropy-coded `0xFF` bytes are always stuffed with a `0x00`, so any
/// other `FF xx` in the scan is a real marker and finding one that is
/// not a restart means the stream ended early.
fn find_restart(scan: &[u8], from: usize, expected: u8) -> Result<usize> {
    let mut at = from;
    while at + 1 < scan.len() {
        if scan[at] == 0xFF {
            match scan[at + 1] {
                // Stuffing, or fill before a marker.
                0x00 => at += 2,
                0xFF => at += 1,
                marker @ 0xD0..=0xD7 if marker == 0xD0 + expected => return Ok(at + 2),
                marker @ 0xD0..=0xD7 => {
                    return Err(Error::Corrupt(format!(
                        "restart marker RST{} where RST{expected} was due",
                        marker - 0xD0
                    )))
                }
                marker => {
                    return Err(Error::Corrupt(format!(
                        "marker FF{marker:02X} in the scan where a restart marker was due"
                    )))
                }
            }
        } else {
            at += 1;
        }
    }
    Err(Error::Corrupt(
        "the scan ends before its next restart marker".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // A lossless JPEG encoder, so the decoder can be tested against
    // something other than itself. It is deliberately naive: a fixed
    // canonical Huffman table given as code-length counts, every
    // predictor, any precision, one to four components, optional
    // restart intervals.
    // ---------------------------------------------------------------

    /// How to encode a frame.
    #[derive(Clone)]
    struct Encode {
        width: usize,
        height: usize,
        components: usize,
        precision: u32,
        predictor: u8,
        point_transform: u32,
        restart_interval: usize,
        /// Sixteen counts of codes of each length, and the symbols in
        /// canonical order (all seventeen difference categories).
        counts: [u8; 16],
        symbols: Vec<u8>,
        /// Give each component its own copy of the table under its own
        /// id, to exercise multi-table DHT segments.
        table_per_component: bool,
    }

    impl Encode {
        fn new(width: usize, height: usize, components: usize, precision: u32) -> Encode {
            Encode {
                width,
                height,
                components,
                precision,
                predictor: 1,
                point_transform: 0,
                restart_interval: 0,
                // 1 + 2 + 3 + 4 + 5 + 2 = 17 symbols, Kraft sum 0.906.
                counts: [0, 1, 2, 3, 4, 5, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                symbols: (0..17u8).collect(),
                table_per_component: false,
            }
        }
        /// A complete code with one symbol at every length up to 16,
        /// so the decoder's slow path for long codes gets used.
        fn long_codes(mut self) -> Encode {
            self.counts = [1; 16];
            self.counts[15] = 2;
            self
        }
    }

    /// MSB-first bit writer that stuffs a zero after every 0xFF, as
    /// JPEG's entropy coder must.
    struct BitWriter {
        out: Vec<u8>,
        acc: u32,
        nbits: u32,
    }

    impl BitWriter {
        fn new() -> BitWriter {
            BitWriter {
                out: Vec::new(),
                acc: 0,
                nbits: 0,
            }
        }
        fn put(&mut self, code: u32, len: u32) {
            assert!(len <= 16);
            self.acc = (self.acc << len) | (code & ((1u32 << len) - 1));
            self.nbits += len;
            while self.nbits >= 8 {
                let byte = (self.acc >> (self.nbits - 8)) as u8;
                self.out.push(byte);
                if byte == 0xFF {
                    self.out.push(0x00);
                }
                self.nbits -= 8;
            }
            self.acc &= (1u32 << self.nbits) - 1;
        }
        /// Pad to a byte with 1 bits, which is what JPEG requires
        /// before a marker (a run of 1s cannot be mistaken for data
        /// because the all-ones code is never assigned).
        fn pad(&mut self) {
            if self.nbits > 0 {
                let n = 8 - self.nbits;
                self.put((1 << n) - 1, n);
            }
        }
        fn marker(&mut self, marker: u8) {
            self.pad();
            self.out.push(0xFF);
            self.out.push(marker);
        }
    }

    /// Canonical code assignment: symbols in order, shortest codes
    /// first, exactly as a JPEG decoder reconstructs them.
    fn canonical(counts: &[u8; 16], symbols: &[u8]) -> Vec<(u32, u32)> {
        let mut table = vec![(0u32, 0u32); 256];
        let mut code = 0u32;
        let mut index = 0usize;
        for len in 1..=16u32 {
            for _ in 0..counts[len as usize - 1] {
                table[symbols[index] as usize] = (code, len);
                code += 1;
                index += 1;
            }
            code <<= 1;
        }
        assert_eq!(index, symbols.len(), "counts and symbols disagree");
        table
    }

    /// The difference between `sample` and its prediction, as a
    /// category and the bits that follow it.
    fn encode_diff(w: &mut BitWriter, codes: &[(u32, u32)], sample: u16, prediction: i32) {
        // Modulo 65536, mapped into -32768..=32767: exactly what the
        // decoder undoes by wrapping.
        let diff = (sample as i32 - prediction) as i16 as i32;
        let category = if diff == 0 {
            0
        } else {
            32 - (diff.unsigned_abs()).leading_zeros()
        };
        assert!(category <= 16);
        let (code, len) = codes[category as usize];
        assert!(len > 0, "symbol {category} has no code");
        w.put(code, len);
        if category == 0 || category == 16 {
            // Category 16 is the single value -32768; T.81 writes no
            // extra bits for it.
            assert!(category != 16 || diff == -32768);
            return;
        }
        let bits = if diff > 0 {
            diff as u32
        } else {
            (diff + (1 << category) - 1) as u32
        };
        w.put(bits, category);
    }

    /// Encode `samples` (`width * height * components`, interleaved).
    fn encode(spec: &Encode, samples: &[u16]) -> Vec<u8> {
        assert_eq!(samples.len(), spec.width * spec.height * spec.components);
        let codes = canonical(&spec.counts, &spec.symbols);
        let mut out: Vec<u8> = vec![0xFF, 0xD8];

        // DHT, all tables in the one segment.
        let ntables = if spec.table_per_component {
            spec.components
        } else {
            1
        };
        let mut dht = Vec::new();
        for id in 0..ntables {
            dht.push(id as u8);
            dht.extend_from_slice(&spec.counts);
            dht.extend_from_slice(&spec.symbols);
        }
        out.extend_from_slice(&[0xFF, 0xC4]);
        out.extend_from_slice(&((dht.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&dht);

        if spec.restart_interval > 0 {
            out.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04]);
            out.extend_from_slice(&(spec.restart_interval as u16).to_be_bytes());
        }

        // SOF3.
        out.extend_from_slice(&[0xFF, 0xC3]);
        out.extend_from_slice(&((8 + spec.components * 3) as u16).to_be_bytes());
        out.push(spec.precision as u8);
        out.extend_from_slice(&(spec.height as u16).to_be_bytes());
        out.extend_from_slice(&(spec.width as u16).to_be_bytes());
        out.push(spec.components as u8);
        for c in 0..spec.components {
            out.extend_from_slice(&[c as u8 + 1, 0x11, 0x00]);
        }

        // SOS.
        out.extend_from_slice(&[0xFF, 0xDA]);
        out.extend_from_slice(&((6 + spec.components * 2) as u16).to_be_bytes());
        out.push(spec.components as u8);
        for c in 0..spec.components {
            let table = if spec.table_per_component { c as u8 } else { 0 };
            out.extend_from_slice(&[c as u8 + 1, table << 4]);
        }
        out.extend_from_slice(&[spec.predictor, 0x00, spec.point_transform as u8]);

        // The scan itself, mirroring the decoder's prediction rules.
        let ncomp = spec.components;
        let stride = spec.width * ncomp;
        let initial = 1i32 << (spec.precision - spec.point_transform - 1);
        let shifted: Vec<u16> = samples.iter().map(|s| s >> spec.point_transform).collect();
        let interval = if spec.restart_interval == 0 {
            spec.width * spec.height
        } else {
            spec.restart_interval
        };
        let mut w = BitWriter::new();
        let mut restarts = 0u8;
        for mcu in 0..spec.width * spec.height {
            if mcu > 0 && mcu % interval == 0 {
                w.marker(0xD0 + restarts % 8);
                restarts += 1;
            }
            let first_of_interval = mcu % interval == 0;
            let x = mcu % spec.width;
            let y = mcu / spec.width;
            for c in 0..ncomp {
                let idx = mcu * ncomp + c;
                let prediction = if first_of_interval {
                    initial
                } else if x == 0 {
                    shifted[idx - stride] as i32
                } else if y == 0 {
                    shifted[idx - ncomp] as i32
                } else {
                    let a = shifted[idx - ncomp] as i32;
                    let b = shifted[idx - stride] as i32;
                    let cc = shifted[idx - stride - ncomp] as i32;
                    match spec.predictor {
                        1 => a,
                        2 => b,
                        3 => cc,
                        4 => a + b - cc,
                        5 => a + ((b - cc) >> 1),
                        6 => b + ((a - cc) >> 1),
                        _ => (a + b) >> 1,
                    }
                };
                encode_diff(&mut w, &codes, shifted[idx], prediction);
            }
        }
        w.pad();
        out.extend_from_slice(&w.out);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    /// xorshift64*, so the tests are random but reproducible without a
    /// dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_image(rng: &mut Rng, len: usize, precision: u32) -> Vec<u16> {
        let max = 1u64 << precision;
        (0..len)
            .map(|i| {
                // A mix of noise and smooth gradients: pure noise
                // never exercises the small difference categories,
                // and pure gradients never exercise the large ones.
                if i % 3 == 0 {
                    rng.below(max) as u16
                } else {
                    ((i as u64 * 7 + rng.below(8)) % max) as u16
                }
            })
            .collect()
    }

    #[test]
    fn round_trip_every_predictor() {
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        for predictor in 1..=7u8 {
            for components in 1..=4usize {
                let mut spec = Encode::new(19, 7, components, 12);
                spec.predictor = predictor;
                let samples = random_image(&mut rng, 19 * 7 * components, 12);
                let stream = encode(&spec, &samples);
                let got = decode(&stream).expect("decode");
                assert_eq!(got.width, 19);
                assert_eq!(got.height, 7);
                assert_eq!(got.components, components);
                assert_eq!(got.precision, 12);
                assert_eq!(
                    got.data, samples,
                    "predictor {predictor}, {components} components"
                );
            }
        }
    }

    #[test]
    fn round_trip_every_precision() {
        let mut rng = Rng(0xDEAD_BEEF_0BAD_F00D);
        for precision in 2..=16u32 {
            let mut spec = Encode::new(23, 5, 2, precision);
            spec.predictor = 4;
            let samples = random_image(&mut rng, 23 * 5 * 2, precision);
            let stream = encode(&spec, &samples);
            let got = decode(&stream).expect("decode");
            assert_eq!(got.data, samples, "precision {precision}");
        }
    }

    #[test]
    fn round_trip_sixteen_bit_wraps() {
        // At sixteen bits the differences are taken modulo 65536 and
        // the widest of them (category 16) carries no value bits, so
        // this is the case where the wrap-around rules matter.
        let mut rng = Rng(0x0F0F_1234_5678_0001);
        let mut spec = Encode::new(64, 16, 1, 16);
        spec.predictor = 1;
        let mut samples = random_image(&mut rng, 64 * 16, 16);
        // Force a category-16 difference: 0 followed by 32768.
        samples[10] = 0;
        samples[11] = 32768;
        let stream = encode(&spec, &samples);
        assert_eq!(decode(&stream).unwrap().data, samples);
    }

    #[test]
    fn round_trip_with_stuffed_ff_bytes() {
        // Sixteen-bit noise makes long codes and 0xFF bytes in the
        // entropy data; the decoder must unstuff them.
        let mut rng = Rng(0x5555_AAAA_1111_2222);
        let spec = Encode::new(37, 11, 3, 16).long_codes();
        let samples = random_image(&mut rng, 37 * 11 * 3, 16);
        let stream = encode(&spec, &samples);
        assert!(
            stream.windows(2).any(|w| w == [0xFF, 0x00]),
            "the test encoder never emitted a stuffed byte, so this proves nothing"
        );
        assert_eq!(decode(&stream).unwrap().data, samples);
    }

    #[test]
    fn round_trip_with_long_codes() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut spec = Encode::new(31, 9, 1, 14).long_codes();
        spec.predictor = 6;
        let samples = random_image(&mut rng, 31 * 9, 14);
        let stream = encode(&spec, &samples);
        assert_eq!(decode(&stream).unwrap().data, samples);
    }

    #[test]
    fn round_trip_with_restart_intervals() {
        let mut rng = Rng(0xABCD_0123_4567_89AB);
        for interval in [1usize, 5, 17, 40, 1000] {
            for predictor in [1u8, 4, 7] {
                let mut spec = Encode::new(17, 6, 2, 12);
                spec.predictor = predictor;
                spec.restart_interval = interval;
                let samples = random_image(&mut rng, 17 * 6 * 2, 12);
                let stream = encode(&spec, &samples);
                let got = decode(&stream).expect("decode with restarts");
                assert_eq!(
                    got.data, samples,
                    "interval {interval}, predictor {predictor}"
                );
            }
        }
    }

    #[test]
    fn round_trip_with_a_table_per_component() {
        let mut rng = Rng(0x2222_3333_4444_5555);
        let mut spec = Encode::new(13, 4, 4, 12);
        spec.table_per_component = true;
        spec.predictor = 5;
        let samples = random_image(&mut rng, 13 * 4 * 4, 12);
        let stream = encode(&spec, &samples);
        assert_eq!(decode(&stream).unwrap().data, samples);
    }

    #[test]
    fn round_trip_with_a_point_transform() {
        let mut rng = Rng(0x7777_8888_9999_AAAA);
        let mut spec = Encode::new(21, 5, 1, 14);
        spec.point_transform = 3;
        spec.predictor = 4;
        let samples = random_image(&mut rng, 21 * 5, 14);
        let stream = encode(&spec, &samples);
        let got = decode(&stream).unwrap();
        // The encoder threw the low bits away, so that is what comes
        // back: the decoder undoes only the shift.
        let want: Vec<u16> = samples.iter().map(|s| (s >> 3) << 3).collect();
        assert_eq!(got.data, want);
    }

    #[test]
    fn round_trip_odd_shapes() {
        let mut rng = Rng(0xFEED_FACE_CAFE_BEEF);
        for (w, h) in [(1usize, 1usize), (1, 9), (9, 1), (2, 2), (255, 3)] {
            for components in [1usize, 2] {
                let spec = Encode::new(w, h, components, 12);
                let samples = random_image(&mut rng, w * h * components, 12);
                let stream = encode(&spec, &samples);
                assert_eq!(
                    decode(&stream).unwrap().data,
                    samples,
                    "{w}x{h}x{components}"
                );
            }
        }
    }

    #[test]
    fn header_reads_the_frame_without_the_scan() {
        let spec = Encode::new(40, 12, 2, 14);
        let samples = vec![100u16; 40 * 12 * 2];
        let stream = encode(&spec, &samples);
        let head = header(&stream).unwrap();
        assert_eq!(
            (head.width, head.height, head.components, head.precision),
            (40, 12, 2, 14)
        );
        assert!(head.data.is_empty());
        // A stream cut off right after SOS still has a readable
        // header even though it cannot be decoded.
        let scan_start = parse(&stream, true).unwrap().scan_start;
        let cut = &stream[..scan_start];
        assert!(header(cut).is_ok());
        assert!(decode(cut).is_err());
    }

    #[test]
    fn rejects_what_it_is_not() {
        let spec = Encode::new(8, 8, 1, 12);
        let samples = vec![7u16; 64];
        let good = encode(&spec, &samples);

        assert!(decode(&[]).is_err());
        assert!(decode(&[0xFF, 0xD8]).is_err());
        assert!(decode(b"not a jpeg at all").is_err());

        // A baseline JPEG (SOF0) is a different process.
        let mut baseline = good.clone();
        let sof = baseline.windows(2).position(|w| w == [0xFF, 0xC3]).unwrap();
        baseline[sof + 1] = 0xC0;
        assert!(matches!(decode(&baseline), Err(Error::Unsupported(_))));

        // Subsampled components.
        let mut subsampled = good.clone();
        subsampled[sof + 11] = 0x21;
        assert!(matches!(decode(&subsampled), Err(Error::Unsupported(_))));

        // A zero height means DNL.
        let mut dnl = good.clone();
        dnl[sof + 5] = 0;
        dnl[sof + 6] = 0;
        assert!(matches!(decode(&dnl), Err(Error::Unsupported(_))));

        // Predictor 0 is the differential mode.
        let mut differential = good.clone();
        let sos = differential
            .windows(2)
            .position(|w| w == [0xFF, 0xDA])
            .unwrap();
        differential[sos + 7] = 0;
        assert!(matches!(decode(&differential), Err(Error::Unsupported(_))));

        // A frame far larger than its scan.
        let mut huge = good.clone();
        huge[sof + 5] = 0xFF;
        huge[sof + 6] = 0xFF;
        huge[sof + 7] = 0xFF;
        huge[sof + 8] = 0xFF;
        assert!(decode(&huge).is_err());
    }

    #[test]
    fn truncation_never_panics() {
        let mut rng = Rng(0x1357_9BDF_0246_8ACE);
        let mut spec = Encode::new(24, 8, 2, 14);
        spec.restart_interval = 9;
        let samples = random_image(&mut rng, 24 * 8 * 2, 14);
        let stream = encode(&spec, &samples);
        for cut in 0..stream.len() {
            let _ = decode(&stream[..cut]);
            let _ = header(&stream[..cut]);
        }
        // Garbage in the entropy data must not hang or panic either.
        for seed in 0..64u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut broken = stream.clone();
            for _ in 0..8 {
                let at = rng.below(broken.len() as u64) as usize;
                broken[at] = rng.below(256) as u8;
            }
            let _ = decode(&broken);
        }
    }

    // ---------------------------------------------------------------
    // Corpus tests. Gated on SCHIST_RAW_CORPUS, which points at a
    // directory of real files; they compare against the LibRaw oracle
    // TIFFs written beside them by `unprocessed_raw -T`.
    //
    // The TIFF walking here is deliberately minimal and local: the
    // crate's own tiff.rs is being written in parallel, and these
    // tests are about the lossless JPEG decoder alone.
    // ---------------------------------------------------------------

    fn corpus() -> Option<std::path::PathBuf> {
        let dir = std::env::var_os("SCHIST_RAW_CORPUS")?;
        let dir = std::path::PathBuf::from(dir);
        dir.is_dir().then_some(dir)
    }

    /// The smallest TIFF reader that can find sensor data.
    struct MiniTiff<'a> {
        bytes: &'a [u8],
        le: bool,
    }

    #[derive(Debug, Clone, Copy)]
    struct MiniEntry {
        kind: u16,
        count: usize,
        offset: usize,
    }

    impl<'a> MiniTiff<'a> {
        fn new(bytes: &'a [u8]) -> Option<MiniTiff<'a>> {
            let le = match bytes.get(0..4)? {
                b"II*\0" => true,
                b"MM\0*" => false,
                _ => return None,
            };
            Some(MiniTiff { bytes, le })
        }
        fn u16(&self, at: usize) -> u16 {
            let b: [u8; 2] = self.bytes[at..at + 2].try_into().unwrap();
            if self.le {
                u16::from_le_bytes(b)
            } else {
                u16::from_be_bytes(b)
            }
        }
        fn u32(&self, at: usize) -> u32 {
            let b: [u8; 4] = self.bytes[at..at + 4].try_into().unwrap();
            if self.le {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            }
        }
        /// Every IFD reachable from IFD0: the main chain and the
        /// SubIFDs each of them names (one level is enough for DNG).
        fn ifds(&self) -> Vec<std::collections::BTreeMap<u16, MiniEntry>> {
            let mut out = Vec::new();
            let mut next = self.u32(4) as usize;
            let mut chain = Vec::new();
            while next != 0 && next + 2 <= self.bytes.len() && chain.len() < 16 {
                chain.push(next);
                let n = self.u16(next) as usize;
                let after = next + 2 + n * 12;
                if after + 4 > self.bytes.len() {
                    break;
                }
                next = self.u32(after) as usize;
            }
            let mut queue = chain;
            while let Some(at) = queue.pop() {
                let n = self.u16(at) as usize;
                let mut map = std::collections::BTreeMap::new();
                for i in 0..n {
                    let e = at + 2 + i * 12;
                    if e + 12 > self.bytes.len() {
                        break;
                    }
                    let tag = self.u16(e);
                    let kind = self.u16(e + 2);
                    let count = self.u32(e + 4) as usize;
                    let size = match kind {
                        1 | 2 | 6 | 7 => 1,
                        3 | 8 => 2,
                        4 | 9 | 11 => 4,
                        _ => 8,
                    };
                    let offset = if count * size <= 4 {
                        e + 8
                    } else {
                        self.u32(e + 8) as usize
                    };
                    map.insert(
                        tag,
                        MiniEntry {
                            kind,
                            count,
                            offset,
                        },
                    );
                    if tag == 0x014A {
                        // SubIFDs.
                        for j in 0..count.min(8) {
                            let sub = self.u32(offset + j * 4) as usize;
                            if sub + 2 <= self.bytes.len() {
                                queue.push(sub);
                            }
                        }
                    }
                }
                out.push(map);
            }
            out
        }
        fn values(&self, e: &MiniEntry) -> Vec<u64> {
            (0..e.count)
                .map(|i| match e.kind {
                    1 | 2 | 6 | 7 => self.bytes[e.offset + i] as u64,
                    3 | 8 => self.u16(e.offset + i * 2) as u64,
                    _ => self.u32(e.offset + i * 4) as u64,
                })
                .collect()
        }
        fn value(&self, map: &std::collections::BTreeMap<u16, MiniEntry>, tag: u16) -> Option<u64> {
            self.values(map.get(&tag)?).first().copied()
        }
    }

    /// The raw IFD of a DNG: compression 7 (lossless JPEG) over a CFA
    /// or linear-raw photometric, plus its tiles or strips.
    struct RawIfd {
        width: usize,
        height: usize,
        spp: usize,
        tile_width: usize,
        tile_height: usize,
        pieces: Vec<(usize, usize)>,
    }

    fn find_raw_ifd(tiff: &MiniTiff<'_>) -> Option<RawIfd> {
        let mut best: Option<RawIfd> = None;
        for ifd in tiff.ifds() {
            let photometric = tiff.value(&ifd, 0x0106).unwrap_or(0);
            let compression = tiff.value(&ifd, 0x0103).unwrap_or(0);
            if compression != 7 || !matches!(photometric, 32803 | 34892) {
                continue;
            }
            let width = tiff.value(&ifd, 0x0100)? as usize;
            let height = tiff.value(&ifd, 0x0101)? as usize;
            let spp = tiff.value(&ifd, 0x0115).unwrap_or(1) as usize;
            let (tile_width, tile_height, offsets, counts) = match ifd.get(&0x0144) {
                Some(off) => (
                    tiff.value(&ifd, 0x0142)? as usize,
                    tiff.value(&ifd, 0x0143)? as usize,
                    tiff.values(off),
                    tiff.values(ifd.get(&0x0145)?),
                ),
                // Strips: a strip is a full-width tile.
                None => {
                    let rows = tiff.value(&ifd, 0x0116).unwrap_or(height as u64) as usize;
                    (
                        width,
                        rows.min(height),
                        tiff.values(ifd.get(&0x0111)?),
                        tiff.values(ifd.get(&0x0117)?),
                    )
                }
            };
            let pieces = offsets
                .iter()
                .zip(counts.iter())
                .map(|(o, c)| (*o as usize, *c as usize))
                .collect();
            let candidate = RawIfd {
                width,
                height,
                spp,
                tile_width,
                tile_height,
                pieces,
            };
            if best
                .as_ref()
                .is_none_or(|b| b.width * b.height < width * height)
            {
                best = Some(candidate);
            }
        }
        best
    }

    /// Decode every tile of a raw IFD and lay them out into the full
    /// frame. Tiles at the right and bottom edges are encoded at full
    /// size and clipped here, as TIFF requires.
    fn assemble(bytes: &[u8], raw: &RawIfd) -> Result<Vec<u16>> {
        let across = raw.width.div_ceil(raw.tile_width);
        let mut frame = vec![0u16; raw.width * raw.height * raw.spp];
        for (i, (offset, count)) in raw.pieces.iter().enumerate() {
            let piece = bytes
                .get(*offset..*offset + *count)
                .ok_or_else(|| Error::Corrupt("tile outside the file".into()))?;
            let image = decode(piece)?;
            // Whatever the stream calls its width and component
            // count, a row of it is a row of the tile.
            assert_eq!(
                image.width * image.components,
                raw.tile_width * raw.spp,
                "tile {i} decodes {} samples a row, tile is {} wide",
                image.width * image.components,
                raw.tile_width
            );
            let tx = (i % across) * raw.tile_width;
            let ty = (i / across) * raw.tile_height;
            let keep = (raw.width - tx).min(raw.tile_width) * raw.spp;
            for row in 0..image.height {
                if ty + row >= raw.height {
                    break;
                }
                let src = row * raw.tile_width * raw.spp;
                let dst = ((ty + row) * raw.width + tx) * raw.spp;
                frame[dst..dst + keep].copy_from_slice(&image.data[src..src + keep]);
            }
        }
        Ok(frame)
    }

    fn oracle(path: &std::path::Path) -> Option<(usize, usize, Vec<u16>)> {
        let tiff = path.with_extension(format!(
            "{}.tiff",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        let image = image::open(&tiff).ok()?.into_luma16();
        let (w, h) = (image.width() as usize, image.height() as usize);
        Some((w, h, image.into_raw()))
    }

    /// Decode a DNG's raw IFD and compare it with LibRaw's unpacking.
    fn check_dng(path: &std::path::Path) -> bool {
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        let Some(tiff) = MiniTiff::new(&bytes) else {
            return false;
        };
        let Some(raw) = find_raw_ifd(&tiff) else {
            println!("{}: no lossless-JPEG raw IFD, skipped", path.display());
            return false;
        };
        let start = std::time::Instant::now();
        let frame = assemble(&bytes, &raw).expect("decode the raw IFD");
        let elapsed = start.elapsed();
        println!(
            "{}: {}x{}x{} in {} piece(s), {:.3}s ({:.1} MP/s)",
            path.display(),
            raw.width,
            raw.height,
            raw.spp,
            raw.pieces.len(),
            elapsed.as_secs_f64(),
            (raw.width * raw.height) as f64 / 1e6 / elapsed.as_secs_f64(),
        );

        let Some((ow, oh, want)) = oracle(path) else {
            println!("  no oracle TIFF beside it; decoded without error only");
            return true;
        };
        if raw.spp != 1 {
            println!(
                "  {} samples a pixel: the greyscale oracle cannot be compared",
                raw.spp
            );
            return true;
        }
        // LibRaw's raw_width x raw_height is the raw IFD's size for a
        // DNG. If a file ever disagrees, compare what overlaps.
        let cw = ow.min(raw.width);
        let ch = oh.min(raw.height);
        if (ow, oh) != (raw.width, raw.height) {
            println!(
                "  oracle is {ow}x{oh}, frame is {}x{}: comparing the overlap",
                raw.width, raw.height
            );
        }
        let mut bad = 0usize;
        let mut first = None;
        for y in 0..ch {
            for x in 0..cw {
                let got = frame[y * raw.width + x];
                let expected = want[y * ow + x];
                if got != expected {
                    bad += 1;
                    first.get_or_insert((x, y, got, expected));
                }
            }
        }
        assert_eq!(
            bad,
            0,
            "{}: {bad} samples differ, first {:?}",
            path.display(),
            first
        );
        println!("  matches the oracle over {cw}x{ch}");
        true
    }

    #[test]
    fn corpus_dng_tiles_match_libraw() {
        let Some(dir) = corpus() else { return };
        let mut checked = 0;
        // The named files first (the Pentax K-5 is the reference
        // case), then anything else in the tree that turns out to be
        // a lossless-JPEG DNG.
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        let mut stack = vec![dir];
        while let Some(at) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&at) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("dng"))
                {
                    paths.push(path);
                }
            }
        }
        paths.sort();
        for path in &paths {
            if check_dng(path) {
                checked += 1;
            }
        }
        println!("checked {checked} of {} DNGs", paths.len());
    }

    /// Canon's CR2 keeps its sensor data as one lossless JPEG stream
    /// in the fourth IFD's strip. The stream's own width is a slice of
    /// the sensor row and its components are interleaved slices, which
    /// is the CR2 module's problem, not this one's: here we only check
    /// that it decodes.
    #[test]
    fn corpus_cr2_streams_decode() {
        let Some(dir) = corpus() else { return };
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        let mut stack = vec![dir];
        while let Some(at) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&at) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("cr2"))
                {
                    paths.push(path);
                }
            }
        }
        paths.sort();
        let mut decoded = 0usize;
        for path in &paths {
            let bytes = std::fs::read(path).expect("read");
            let Some(tiff) = MiniTiff::new(&bytes) else {
                continue;
            };
            // IFD3 (the fourth) holds the sensor strip.
            let ifds = tiff.ifds();
            let Some(strip) = ifds
                .iter()
                .filter_map(|ifd| {
                    let offset = tiff.value(ifd, 0x0111)? as usize;
                    let count = tiff.value(ifd, 0x0117)? as usize;
                    // The sensor strip is by far the largest.
                    (count > 1 << 20).then_some((offset, count))
                })
                .max_by_key(|(_, count)| *count)
            else {
                continue;
            };
            let piece = &bytes[strip.0..strip.0 + strip.1];
            let head = match header(piece) {
                Ok(head) => head,
                // Canon's sRAW/mRAW frames are subsampled YCbCr; this
                // decoder says so rather than guessing.
                Err(Error::Unsupported(why)) => {
                    println!("{}: skipped, {why}", path.display());
                    continue;
                }
                Err(e) => panic!("{}: {e}", path.display()),
            };
            decoded += 1;
            let start = std::time::Instant::now();
            let image = decode(piece).expect("decode the CR2 stream");
            let elapsed = start.elapsed();
            assert_eq!(image.data.len(), head.width * head.height * head.components);
            let over = image
                .data
                .iter()
                .filter(|v| **v >= 1 << head.precision)
                .count();
            assert_eq!(
                over, 0,
                "samples outside the declared {}-bit range",
                head.precision
            );
            let low = image.data.iter().filter(|v| **v < 4096).count();
            println!(
                "{}: {}x{}x{} at {} bits, {:.3}s, {:.0}% of samples under 4096",
                path.display(),
                head.width,
                head.height,
                head.components,
                head.precision,
                elapsed.as_secs_f64(),
                100.0 * low as f64 / image.data.len() as f64,
            );
            if head.precision <= 12 {
                assert!(
                    low * 10 > image.data.len() * 9,
                    "a 12-bit body's samples should nearly all be under 4096"
                );
            }
            // Canon's slicing only permutes the samples, so the
            // oracle's frame must hold exactly the same multiset of
            // values as the stream decodes to. That checks every
            // sample without knowing where any of them goes.
            if let Some((ow, oh, want)) = oracle(path) {
                if ow * oh == image.data.len() {
                    let mut mine = vec![0u32; 65536];
                    let mut theirs = vec![0u32; 65536];
                    for v in &image.data {
                        mine[*v as usize] += 1;
                    }
                    for v in &want {
                        theirs[*v as usize] += 1;
                    }
                    let differing = mine
                        .iter()
                        .zip(theirs.iter())
                        .filter(|(a, b)| a != b)
                        .count();
                    assert_eq!(
                        differing,
                        0,
                        "{}: the histogram differs from LibRaw's",
                        path.display()
                    );
                    println!("  same multiset of values as the oracle ({ow}x{oh})");
                } else {
                    println!(
                        "  oracle is {ow}x{oh} = {} samples, stream is {}",
                        ow * oh,
                        image.data.len()
                    );
                }
            }
        }
        println!("decoded {decoded} of {} CR2 streams", paths.len());
    }

    /// Cutting a real stream anywhere must give an error or a frame,
    /// never a panic and never a hang.
    #[test]
    fn corpus_truncation_never_panics() {
        let Some(dir) = corpus() else { return };
        let mut streams: Vec<Vec<u8>> = Vec::new();
        for name in ["_MAR0543.DNG", "PXL_20201121_100251397.dng", "IMG_1361.DNG"] {
            let path = dir.join(name);
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Some(tiff) = MiniTiff::new(&bytes) else {
                continue;
            };
            let Some(raw) = find_raw_ifd(&tiff) else {
                continue;
            };
            if let Some((offset, count)) = raw.pieces.first() {
                if let Some(piece) = bytes.get(*offset..*offset + *count) {
                    streams.push(piece.to_vec());
                }
            }
        }
        let path = dir.join("_MG_7191.CR2");
        if let Ok(bytes) = std::fs::read(&path) {
            if let Some(tiff) = MiniTiff::new(&bytes) {
                if let Some((offset, count)) = tiff
                    .ifds()
                    .iter()
                    .filter_map(|ifd| {
                        Some((
                            tiff.value(ifd, 0x0111)? as usize,
                            tiff.value(ifd, 0x0117)? as usize,
                        ))
                    })
                    .max_by_key(|(_, count)| *count)
                {
                    if let Some(piece) = bytes.get(offset..offset + count) {
                        streams.push(piece.to_vec());
                    }
                }
            }
        }
        assert!(
            !streams.is_empty(),
            "the corpus held no lossless JPEG streams"
        );
        let mut rng = Rng(0x0BAD_C0DE_DEAD_BEEF);
        for stream in &streams {
            for _ in 0..20 {
                let cut = rng.below(stream.len() as u64) as usize;
                let _ = decode(&stream[..cut]);
                let _ = header(&stream[..cut]);
            }
        }
    }
}
