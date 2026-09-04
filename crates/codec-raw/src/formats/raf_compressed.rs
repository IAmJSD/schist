//! Fujifilm's compressed sensor strips, from the X-T2 (2016) onwards.
//!
//! One entropy coder covers all four combinations the cameras ship:
//! lossless and lossy, X-Trans and Bayer. It is a predictive coder in
//! the JPEG-LS family — predict from the neighbours, classify the
//! local gradient into a context, Golomb-Rice the residual with a
//! per-context adaptive shift — wrapped in a frame layout that is
//! entirely Fujifilm's own.
//!
//! # Layout
//!
//! Nothing in the RAF container says a strip is compressed, so the
//! strip is probed: sixteen big-endian header bytes starting `0x4953`
//! and agreeing with themselves ([`Header::parse`]).
//!
//! ```text
//! 0   u16  0x4953, the signature
//! 2   u8   1 = lossless compressed, 0 = lossy compressed
//! 3   u8   filter array: 0x10 X-Trans, 0x00 Bayer
//! 4   u8   bits a sample (12, 14 or 16)
//! 5   u16  frame height, a multiple of six
//! 7   u16  frame width rounded up to a whole number of stripes
//! 9   u16  frame width
//! 11  u16  stripe width in pixels (0x300 in every file)
//! 13  u8   stripes across the frame
//! 14  u16  six-row blocks down the frame (height / 6)
//! ```
//!
//! Then one `u32` byte count per stripe, the *offset* past the table
//! rounded up to sixteen bytes; then, in a lossy file only, one
//! quantiser base per stripe per block, each stripe's run padded to
//! sixteen; then the stripes' entropy data, back to back.
//!
//! Byte 2 is the mode flag, not a version. It is the only reliable way
//! to tell the modes apart: a decoder that guesses from the bit rate
//! mis-reads any unusually flat or unusually noisy frame.
//!
//! # The frame
//!
//! A stripe is 768 columns wide, spans the frame's full height and is
//! completely independent of its neighbours — its own bit reader, line
//! buffers and contexts — so the stripes decode in parallel. The last
//! stripe is *decoded* at full width like all the others and only
//! *copied out* narrow; its trailing symbols are real and must be
//! consumed.
//!
//! Each stripe is a stack of six-row blocks. A block is held in
//! eighteen line buffers — three red, six green, three blue, plus two
//! rows of history a colour — each `line_width + 2` samples, the extra
//! two being guard slots either side that hold the neighbouring line's
//! first and last samples so the edge taps need no special case.
//! `line_width` is `2/3` of the stripe width for X-Trans (three sensor
//! columns share two buffer slots) and half of it for Bayer.
//!
//! A block is coded as six two-row passes, each pairing one green
//! buffer with one red or blue buffer, and within a pass the even
//! positions run five steps ahead of the odd ones — the odd
//! prediction needs the sample to its right. On an X-Trans block some
//! even slots of the red, blue and two of the green buffers cover no
//! photosite; those consume no bits and are filled with the even
//! prediction, but they are ordinary state afterwards and later
//! samples read them as neighbours.
//!
//! # Lossy
//!
//! Lossy mode is a uniform quantiser on the residual: the value is
//! `pred ± (2 * q_base + 1) * residual`, with `q_base` read per block
//! from the header's array. There is no transform and no reconstruction
//! offset. Three fixed "fine" tables with `q_base` 0, 1 and 2 take over
//! in flat neighbourhoods, so smooth areas stay near-lossless; each has
//! its own alphabet, escape width, gradient scale and contexts.
//!
//! Everything here was written from a functional description of the
//! format and checked against real files and their unpacked oracles.

use crate::bits::{BitPump, BitPumpMsb};
use crate::{Cfa, CfaColor, Error, Result};
use rayon::prelude::*;

/// The signature every compressed strip starts with.
pub const SIGNATURE: u16 = 0x4953;

/// Rows of the sensor in one coded block, for both filter arrays.
pub const BLOCK_ROWS: usize = 6;

/// Line buffers a stripe keeps: three red, six green, three blue, each
/// colour preceded by two rows of history from the previous block.
const LINES: usize = 18;

/// Gradient contexts the main table indexes: `|9 * l1 + l2|` with both
/// levels in `-4..=4`.
const CONTEXTS: usize = 41;

/// `sum` and `count` halve once `count` reaches this, which is what
/// makes the coder adapt to the recent past rather than the whole line.
const HALVE_AT: u32 = 0x40;

/// The only stripe width Fujifilm has shipped. The reference decoder
/// rejects anything else outright and so does this one: the pass
/// schedule's `% 4` phases are only correct for a stripe that is a
/// whole number of filter-array periods wide.
const STRIPE_WIDTH: usize = 0x300;

/// A compressed strip's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Byte 2: the coding mode. Lossy files carry a quantiser-base
    /// array the lossless ones do not.
    pub lossless: bool,
    /// 0x10 on the X-Trans bodies, 0 on the Bayer ones.
    pub sensor: u8,
    pub bits: u32,
    pub width: usize,
    pub height: usize,
    /// `width` rounded up to a whole number of stripes.
    pub rounded_width: usize,
    pub stripe_width: usize,
    pub stripes: usize,
    /// Six-row blocks down the frame.
    pub blocks: usize,
}

fn corrupt<T>(why: impl Into<String>) -> Result<T> {
    Err(Error::Corrupt(why.into()))
}

/// `n` rounded up to a multiple of sixteen.
const fn roundup16(n: usize) -> usize {
    (n + 15) & !15
}

impl Header {
    /// Read and check a header. `Err(Corrupt)` when the strip claims a
    /// geometry that contradicts itself or the format.
    ///
    /// Every one of these constraints is a property of the format
    /// rather than of the corpus, which is what makes the check double
    /// as the compression probe: an uncompressed strip that happened to
    /// start `0x4953` would have to satisfy all of them as well.
    pub fn parse(strip: &[u8]) -> Result<Header> {
        let head: &[u8; 16] = match strip.get(..16).and_then(|b| b.try_into().ok()) {
            Some(head) => head,
            None => return corrupt("truncated compressed strip header"),
        };
        let be16 = |at: usize| u16::from_be_bytes([head[at], head[at + 1]]) as usize;
        if be16(0) != SIGNATURE as usize {
            return corrupt("compressed sensor strip without its 0x4953 signature");
        }
        if head[2] > 1 {
            return corrupt(format!("compressed strip mode byte {}", head[2]));
        }
        let header = Header {
            lossless: head[2] == 1,
            sensor: head[3],
            bits: head[4] as u32,
            height: be16(5),
            rounded_width: be16(7),
            width: be16(9),
            stripe_width: be16(11),
            stripes: head[13] as usize,
            blocks: be16(14),
        };
        if !matches!(header.sensor, 0x00 | 0x10) {
            return corrupt(format!(
                "compressed strip filter kind {:#04x}",
                header.sensor
            ));
        }
        if !matches!(header.bits, 12 | 14 | 16) {
            return corrupt(format!("compressed strip at {} bits", header.bits));
        }
        if header.height < BLOCK_ROWS
            || header.height > 0x4002
            || !header.height.is_multiple_of(BLOCK_ROWS)
        {
            return corrupt(format!("compressed strip is {} rows", header.height));
        }
        if header.width < STRIPE_WIDTH || header.width > 0x4200 || !header.width.is_multiple_of(24)
        {
            return corrupt(format!("compressed strip is {} columns", header.width));
        }
        if header.stripe_width != STRIPE_WIDTH {
            return corrupt(format!(
                "compressed strip in stripes of {}",
                header.stripe_width
            ));
        }
        if header.rounded_width > 0x4200
            || header.rounded_width < header.stripe_width
            || !header.rounded_width.is_multiple_of(header.stripe_width)
            || header.rounded_width < header.width
            || header.rounded_width - header.width >= header.stripe_width
        {
            return corrupt(format!(
                "rounded width {} does not round {} up by less than a stripe of {}",
                header.rounded_width, header.width, header.stripe_width
            ));
        }
        if header.stripes == 0
            || header.stripes > 0x10
            || header.stripes * header.stripe_width != header.rounded_width
        {
            return corrupt(format!(
                "{} stripes of {} do not make the rounded width {}",
                header.stripes, header.stripe_width, header.rounded_width
            ));
        }
        if header.blocks == 0
            || header.blocks > 0xAAB
            || header.blocks * BLOCK_ROWS != header.height
        {
            return corrupt(format!(
                "{} six-row blocks do not make {} rows",
                header.blocks, header.height
            ));
        }
        Ok(header)
    }

    /// Whether the header calls the frame X-Trans.
    pub fn is_x_trans(&self) -> bool {
        self.sensor == 0x10
    }

    /// The largest sample the depth can hold.
    fn max_value(&self) -> i32 {
        (1i32 << self.bits) - 1
    }

    /// Samples in one line buffer: three X-Trans sensor columns share
    /// two slots, two Bayer columns share one.
    fn line_width(&self) -> usize {
        if self.is_x_trans() {
            self.stripe_width * 2 / 3
        } else {
            self.stripe_width / 2
        }
    }

    /// Bytes of quantiser bases between the size table and the first
    /// stripe: none in a lossless file, one per block per stripe with
    /// each stripe's run padded to sixteen bytes in a lossy one.
    fn qbase_bytes(&self) -> usize {
        if self.lossless {
            0
        } else {
            self.stripes * roundup16(self.blocks)
        }
    }

    /// Where the first stripe's entropy-coded data starts.
    pub fn data_offset(&self) -> usize {
        16 + roundup16(self.stripes * 4) + self.qbase_bytes()
    }

    /// Each stripe's compressed length, in order.
    pub fn stripe_lengths(&self, strip: &[u8]) -> Result<Vec<usize>> {
        let mut out = Vec::with_capacity(self.stripes);
        for i in 0..self.stripes {
            let at = 16 + i * 4;
            let bytes: [u8; 4] = match strip.get(at..at + 4).and_then(|b| b.try_into().ok()) {
                Some(bytes) => bytes,
                None => return corrupt("truncated stripe length table"),
            };
            out.push(u32::from_be_bytes(bytes) as usize);
        }
        Ok(out)
    }

    /// Stripe `s`'s quantiser bases, one a block, or `None` when the
    /// file is lossless. A short array is a corrupt file: the bases
    /// steer the quantiser and a missing one cannot be guessed.
    pub fn qbases<'a>(&self, strip: &'a [u8], s: usize) -> Result<Option<&'a [u8]>> {
        if self.lossless {
            return Ok(None);
        }
        let start = 16 + roundup16(self.stripes * 4) + s * roundup16(self.blocks);
        match strip.get(start..start + self.blocks) {
            Some(run) => Ok(Some(run)),
            None => corrupt("truncated quantiser-base array"),
        }
    }
}

/// Whether a sensor strip holds a compressed frame.
///
/// The container gives no hint, so this is the probe: the signature
/// plus every self-consistency rule the header can be held to.
pub fn is_compressed(strip: &[u8]) -> bool {
    Header::parse(strip).is_ok()
}

/// Bits needed to write `n - 1`, with the format's own two edge cases
/// (`0` for an empty alphabet, `1` for a one-symbol one).
fn ceil_log2(n: u32) -> u32 {
    match n {
        0 => 0,
        1 => 1,
        _ => 32 - (n - 1).leading_zeros(),
    }
}

/// One quantiser: thresholds, alphabet and step.
///
/// Lossless files use a single table with `q_base` 0. Lossy files
/// rebuild a main table whenever the block's quantiser base changes,
/// and keep three fixed fine tables for flat neighbourhoods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Table {
    q_base: i32,
    /// The four gradient thresholds. A fifth (`max_value`) exists in
    /// the format's own description but no rule reads it.
    t: [i32; 4],
    /// Symbols in the alphabet, and the modulus the coder works to.
    total_values: i32,
    /// Literal bits an escape reads — the *table's* width, not the
    /// frame's depth, which is the whole difference in lossy mode.
    raw_bits: u32,
    /// Zero-run length at which a symbol escapes.
    esc: u32,
    /// 9 for the main table, 3 for the fine ones.
    grad_mult: i32,
    /// Largest neighbourhood roughness a fine table will accept.
    max_flatness: i32,
    /// `2 * q_base + 1`: what the decoded residual is multiplied by.
    step: i32,
    /// Every context of the table starts here.
    init_sum: u32,
}

impl Table {
    fn new(q_base: i32, max_value: i32, max_bits: u32, grad_mult: i32, max_flatness: i32) -> Table {
        // The thresholds spread with the quantiser base, and collapse
        // onto each other when the frame's depth cannot carry them —
        // which only happens on hypothetical shallow frames, but the
        // order of the clamps matters and is reproduced as given.
        let mut t1 = 3 * q_base + 0x12;
        let mut t2 = 5 * q_base + 0x43;
        let mut t3 = 7 * q_base + 0x114;
        if t1 > max_value || t1 < q_base + 1 {
            t1 = q_base + 1;
        }
        if t2 < t1 || t2 > max_value {
            t2 = t1;
        }
        if t3 < t2 || t3 > max_value {
            t3 = t2;
        }
        let total_values = (max_value + 2 * q_base) / (2 * q_base + 1) + 1;
        let raw_bits = ceil_log2(total_values as u32);
        Table {
            q_base,
            t: [q_base, t1, t2, t3],
            total_values,
            raw_bits,
            esc: max_bits.saturating_sub(raw_bits + 1),
            grad_mult,
            max_flatness,
            step: 2 * q_base + 1,
            init_sum: 2.max(((total_values as u32) + 0x20) >> 6),
        }
    }

    /// A difference quantised to nine levels.
    ///
    /// The comparisons are deliberately lopsided — `<=` on the
    /// negative side of the outer thresholds, `<` on the positive —
    /// which only shows when a difference lands exactly on a
    /// threshold, and then it shows as a wrong context and a wrong
    /// pixel.
    #[inline(always)]
    fn level(&self, v: i32) -> i32 {
        let [t0, t1, t2, t3] = self.t;
        if v <= -t3 {
            -4
        } else if v <= -t2 {
            -3
        } else if v <= -t1 {
            -2
        } else if v < -t0 {
            -1
        } else if v <= t0 {
            0
        } else if v < t1 {
            1
        } else if v < t2 {
            2
        } else if v < t3 {
            3
        } else {
            4
        }
    }
}

/// One adaptive Golomb-Rice context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ctx {
    sum: u32,
    count: u32,
}

/// Which even slots of a line buffer carry a photosite.
///
/// The red and blue buffers each serve two sensor rows whose colours
/// sit at different phases of the 6x6 array, so their pattern repeats
/// every four slots and differs from pass to pass. Bayer blocks code
/// everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvenRule {
    Coded,
    Filled,
    /// Filled where `slot % 4` is this, coded elsewhere.
    FilledWhen(usize),
}

impl EvenRule {
    #[inline(always)]
    fn coded(self, p: usize) -> bool {
        match self {
            EvenRule::Coded => true,
            EvenRule::Filled => false,
            EvenRule::FilledWhen(m) => p % 4 != m,
        }
    }
}

/// The six two-row passes: which two line buffers each codes, in
/// order. Identical for both filter arrays. Note that the green buffer
/// is not always the first of its pair.
const PASSES: [(usize, usize); 6] = [(2, 7), (8, 15), (3, 9), (10, 16), (4, 11), (12, 17)];

/// Even-position rules per pass, X-Trans. Straight out of the filter
/// array: see [`sensor_map`] for the other half of the same fact.
const XTRANS_RULES: [(EvenRule, EvenRule); 6] = [
    (EvenRule::Filled, EvenRule::Coded),
    (EvenRule::Coded, EvenRule::Filled),
    (EvenRule::FilledWhen(0), EvenRule::Filled),
    (EvenRule::Coded, EvenRule::FilledWhen(2)),
    (EvenRule::FilledWhen(2), EvenRule::Coded),
    (EvenRule::Filled, EvenRule::FilledWhen(0)),
];

const BAYER_RULES: [(EvenRule, EvenRule); 6] = [(EvenRule::Coded, EvenRule::Coded); 6];

/// Line buffer indices, in the fixed order the vertical taps assume:
/// every tap reads the buffer immediately before it, which is why the
/// two history rows sit in front of each colour's current rows.
const RED: [usize; 3] = [2, 3, 4];
const GREEN: [usize; 6] = [7, 8, 9, 10, 11, 12];
const BLUE: [usize; 3] = [15, 16, 17];

/// A zero run, terminator consumed.
///
/// The reader feeds zeros for ever past the end of a stripe, so an
/// unterminated run is how a truncated or forged stream shows itself;
/// no table escapes past 47 zeros, so anything beyond 128 is refused
/// rather than looped on.
#[inline]
fn zero_run(pump: &mut BitPumpMsb<'_>) -> Result<u32> {
    let mut total = 0u32;
    loop {
        let word = pump.peek(32);
        if word != 0 {
            let n = word.leading_zeros();
            pump.consume(n + 1);
            return Ok(total + n);
        }
        pump.consume(32);
        total += 32;
        if total > 128 {
            return corrupt("compressed strip holds an unterminated zero run");
        }
    }
}

/// One stripe's decoder: a bit reader, eighteen line buffers and the
/// adaptive state, all of it private to the stripe.
struct Stripe<'a> {
    pump: BitPumpMsb<'a>,
    /// The eighteen buffers, flat: buffer `x` slot `s` is
    /// `buf[x * stride + s]`. Flattening is what makes the vertical
    /// taps uniform — the buffer above is always `stride` earlier.
    buf: Vec<i32>,
    stride: usize,
    line_width: usize,
    max_value: i32,
    max_bits: u32,
    lossy: bool,
    /// The main table, then the three fine ones.
    tables: [Table; 4],
    /// Per table: three even context sets then three odd ones. A pass
    /// uses set `pass % 3`, and both its buffers share it.
    ctx: [[[Ctx; CONTEXTS]; 6]; 4],
    rules: [(EvenRule, EvenRule); 6],
    /// Symbols consumed, and the per-pass split of the last block —
    /// both only exist because they are the cheapest check there is on
    /// the visit order.
    symbols: u64,
    pass_symbols: [u32; 6],
}

impl<'a> Stripe<'a> {
    fn new(data: &'a [u8], header: &Header) -> Stripe<'a> {
        let line_width = header.line_width();
        let max_value = header.max_value();
        let max_bits = 4 * ceil_log2(max_value as u32 + 1);
        let main = Table::new(0, max_value, max_bits, 9, 0);
        let tables = [
            main,
            Table::new(0, max_value, max_bits, 3, 5),
            Table::new(1, max_value, max_bits, 3, 6),
            Table::new(2, max_value, max_bits, 3, 7),
        ];
        let mut stripe = Stripe {
            pump: BitPumpMsb::new(data),
            buf: vec![0; LINES * (line_width + 2)],
            stride: line_width + 2,
            line_width,
            max_value,
            max_bits,
            lossy: !header.lossless,
            tables,
            ctx: [[[Ctx { sum: 0, count: 0 }; CONTEXTS]; 6]; 4],
            rules: if header.is_x_trans() {
                XTRANS_RULES
            } else {
                BAYER_RULES
            },
            symbols: 0,
            pass_symbols: [0; 6],
        };
        for t in 0..4 {
            stripe.reset_contexts(t);
        }
        stripe
    }

    /// Put every context of one table back to its opening state.
    fn reset_contexts(&mut self, t: usize) {
        let start = Ctx {
            sum: self.tables[t].init_sum,
            count: 1,
        };
        for set in self.ctx[t].iter_mut() {
            set.fill(start);
        }
    }

    /// Point the main table at a new quantiser base, resetting its
    /// contexts. The fine tables' contexts survive — they are tuned to
    /// flat neighbourhoods, which do not change character when the
    /// block's coarse step does.
    fn set_q_base(&mut self, q_base: i32) {
        self.tables[0] = Table::new(q_base, self.max_value, self.max_bits, 9, 0);
        self.reset_contexts(0);
    }

    /// Which table codes a sample whose neighbourhood is this rough.
    ///
    /// A fine table is only eligible while the main quantiser is at
    /// least as coarse as it is, so lossless frames (base 0) never
    /// reach one.
    #[inline(always)]
    fn select(&self, flatness: i32) -> usize {
        if !self.lossy {
            return 0;
        }
        let q_base = self.tables[0].q_base;
        for i in 1..=3 {
            if q_base < i as i32 {
                break;
            }
            if flatness <= self.tables[i].max_flatness {
                return i;
            }
        }
        0
    }

    /// Decode one symbol in context `ci` of set `set` of table `t`,
    /// returning the signed residual and updating the context.
    #[inline(always)]
    fn residual(&mut self, t: usize, set: usize, ci: usize) -> Result<i32> {
        let table = &self.tables[t];
        let (esc, raw_bits, total_values) = (table.esc, table.raw_bits, table.total_values);
        let Ctx { sum, count } = self.ctx[t][set][ci];
        // The shift that makes the coder's guess at the residual's
        // magnitude: `sum / count`, rounded to a power of two, and
        // never past 15.
        let k = if count >= sum {
            0
        } else {
            let mut k = 1u32;
            while k < 15 && (count << k) < sum {
                k += 1;
            }
            k
        };
        let z = zero_run(&mut self.pump)?;
        let v = if z < esc {
            ((z << k) + self.pump.get(k)) as i32
        } else {
            // The escape spends the table's full width on a literal,
            // biased by one so it can never re-code a zero residual.
            self.pump.get(raw_bits) as i32 + 1
        };
        if v < 0 || v >= total_values {
            return corrupt("compressed strip holds a symbol outside its alphabet");
        }
        // Fold: even to a positive residual, odd to a negative one.
        let r = if v & 1 == 0 { v >> 1 } else { -1 - (v >> 1) };
        let ctx = &mut self.ctx[t][set][ci];
        ctx.sum = sum + r.unsigned_abs();
        ctx.count = count;
        if count == HALVE_AT {
            ctx.sum >>= 1;
            ctx.count >>= 1;
        }
        ctx.count += 1;
        self.symbols += 1;
        Ok(r)
    }

    /// `pred +/- step * r`, wrapped into the alphabet and clamped.
    ///
    /// The middle step is a modular wrap and not a clamp: the coder
    /// works modulo `total_values * step`, so a residual that would
    /// take a value off one end of the range brings it back at the
    /// other. It fires only on clipped highlights, which is exactly
    /// why getting it wrong survives casual testing.
    #[inline(always)]
    fn reconstruct(&self, t: usize, pred: i32, r: i32, grad: i32) -> i32 {
        let table = &self.tables[t];
        let mut x = if grad < 0 {
            pred - r * table.step
        } else {
            pred + r * table.step
        };
        if x < -table.q_base {
            x += table.total_values * table.step;
        } else if x > table.q_base + self.max_value {
            x -= table.total_values * table.step;
        }
        x.clamp(0, self.max_value)
    }

    /// An even position: coded, or filled from its own prediction.
    #[inline(always)]
    fn even(&mut self, x: usize, p: usize, set: usize, coded: bool) -> Result<()> {
        let here = x * self.stride;
        let up = here - self.stride;
        let up2 = here - 2 * self.stride;
        // Slot p is position p-1, slot p+1 is position p: the guards
        // make the left and right edges ordinary.
        let b = self.buf[up + p + 1];
        let c = self.buf[up + p];
        let d = self.buf[up + p + 2];
        let f = self.buf[up2 + p + 1];
        let dcb = (c - b).abs();
        let dfb = (f - b).abs();
        let ddb = (d - b).abs();
        // A crude edge detector: drop whichever horizontal neighbour
        // lies across the strongest gradient and lean on the sample
        // two rows up instead.
        let pred4 = if dcb > dfb && dcb > ddb {
            f + d + 2 * b
        } else if ddb > dcb && ddb > dfb {
            f + c + 2 * b
        } else {
            d + c + 2 * b
        };
        let pred = pred4 >> 2;
        if !coded {
            self.buf[here + p + 1] = pred;
            return Ok(());
        }
        let t = self.select(dfb + dcb);
        let table = &self.tables[t];
        let grad = table.grad_mult * table.level(b - f) + table.level(c - b);
        let r = self.residual(t, set, grad.unsigned_abs() as usize)?;
        self.buf[here + p + 1] = self.reconstruct(t, pred, r, grad);
        Ok(())
    }

    /// An odd position. Always coded, and it reads the sample to its
    /// right — which is why the even side runs ahead.
    #[inline(always)]
    fn odd(&mut self, x: usize, p: usize, set: usize) -> Result<()> {
        let here = x * self.stride;
        let up = here - self.stride;
        let a = self.buf[here + p];
        let g = self.buf[here + p + 2];
        let b = self.buf[up + p + 1];
        let c = self.buf[up + p];
        let d = self.buf[up + p + 2];
        let pred = if (b > c && b > d) || (b < c && b < d) {
            // The row above turns here, so trust it as a level.
            (g + a + 2 * b) >> 2
        } else {
            (a + g) >> 1
        };
        let t = self.select((b - c).abs() + (c - a).abs());
        let table = &self.tables[t];
        let grad = table.grad_mult * table.level(b - c) + table.level(c - a);
        let r = self.residual(t, set, grad.unsigned_abs() as usize)?;
        self.buf[here + p + 1] = self.reconstruct(t, pred, r, grad);
        Ok(())
    }

    /// Refresh a colour's guard slots, cascading down its buffers: a
    /// line's left guard is the first real sample of the line above it
    /// and its right guard that line's last.
    fn guards(&mut self, first: usize, last: usize) {
        for x in first..=last {
            let up = (x - 1) * self.stride;
            let (left, right) = (self.buf[up + 1], self.buf[up + self.line_width]);
            let here = x * self.stride;
            self.buf[here] = left;
            self.buf[here + self.line_width + 1] = right;
        }
    }

    /// Copy buffer `from` over buffer `to`, guards included.
    fn copy_line(&mut self, to: usize, from: usize) {
        let (to, from) = (to * self.stride, from * self.stride);
        self.buf.copy_within(from..from + self.stride, to);
    }

    /// One six-row block, six two-row passes.
    fn block(&mut self) -> Result<()> {
        for (pass, &(first, second)) in PASSES.iter().enumerate() {
            let before = self.symbols;
            let set = pass % 3;
            let (rule1, rule2) = self.rules[pass];
            let mut even_pos = 0usize;
            let mut odd_pos = 1usize;
            // The odd side starts once the even counter passes eight,
            // tested after the increment, so it trails by five
            // positions. Two would satisfy the right-hand tap; five is
            // what the format does, and the interleave of symbols in
            // the bitstream depends on it exactly.
            loop {
                if even_pos < self.line_width {
                    self.even(first, even_pos, set, rule1.coded(even_pos))?;
                    self.even(second, even_pos, set, rule2.coded(even_pos))?;
                    even_pos += 2;
                }
                if (even_pos > 8 || even_pos >= self.line_width) && odd_pos < self.line_width {
                    self.odd(first, odd_pos, 3 + set)?;
                    self.odd(second, odd_pos, 3 + set)?;
                    odd_pos += 2;
                }
                if even_pos >= self.line_width && odd_pos >= self.line_width {
                    break;
                }
            }
            // Green's guards are refreshed after every pass, red's
            // after the odd-numbered ones and blue's after the even.
            if pass % 2 == 0 {
                self.guards(RED[0], RED[2]);
                self.guards(GREEN[0], GREEN[5]);
            } else {
                self.guards(GREEN[0], GREEN[5]);
                self.guards(BLUE[0], BLUE[2]);
            }
            self.pass_symbols[pass] = (self.symbols - before) as u32;
        }
        Ok(())
    }

    /// Retire the block: keep each colour's last two coded rows as the
    /// next block's history, then clear the current rows, leaving only
    /// the first buffer of each colour holding guards from history.
    ///
    /// Red and blue keep rows 1 and 2 and drop row 0 entirely — with
    /// three rows a block, only two can be kept.
    fn rotate(&mut self) {
        self.copy_line(0, RED[1]);
        self.copy_line(1, RED[2]);
        self.copy_line(5, GREEN[4]);
        self.copy_line(6, GREEN[5]);
        self.copy_line(13, BLUE[1]);
        self.copy_line(14, BLUE[2]);
        for x in RED.iter().chain(GREEN.iter()).chain(BLUE.iter()) {
            let at = x * self.stride;
            self.buf[at..at + self.stride].fill(0);
        }
        for first in [RED[0], GREEN[0], BLUE[0]] {
            let up = (first - 1) * self.stride;
            let (left, right) = (self.buf[up + 1], self.buf[up + self.line_width]);
            let here = first * self.stride;
            self.buf[here] = left;
            self.buf[here + self.line_width + 1] = right;
        }
    }

    /// One sample out of the buffers.
    #[inline(always)]
    fn sample(&self, buffer: usize, slot: usize) -> u16 {
        self.buf[buffer * self.stride + slot + 1] as u16
    }
}

/// Where every sensor column of a block row lives: `(line buffer,
/// slot)`.
///
/// For X-Trans each run of three sensor columns maps onto two slots:
/// the run's first column takes the even slot and its other two take
/// the odd one, in different buffers — a 6x6 array never puts two
/// samples of the same colour in the same row of a run. That is where
/// `line_width = 2 * stripe_width / 3` comes from, and the slots left
/// over are exactly the ones the passes interpolate. For Bayer two
/// columns share a slot and nothing is interpolated.
fn sensor_map(cfa: &Cfa, x_trans: bool, stripe_width: usize) -> Result<Vec<Vec<(usize, usize)>>> {
    let mut map = Vec::with_capacity(BLOCK_ROWS);
    for r in 0..BLOCK_ROWS {
        let mut row = Vec::with_capacity(stripe_width);
        for c in 0..stripe_width {
            // Stripes start on a multiple of 768 and blocks on a
            // multiple of six, so a column's phase in the array is the
            // same in every stripe and every block.
            let color = match cfa.color_at(c, r) {
                Some(color) => color,
                None => return corrupt("compressed strip under a filter array with no colours"),
            };
            let slot = if x_trans {
                2 * (c / 3) + usize::from(!c.is_multiple_of(3))
            } else {
                c / 2
            };
            let buffer = match color {
                CfaColor::Green | CfaColor::Green2 => GREEN[r],
                CfaColor::Red => RED[r / 2],
                CfaColor::Blue => BLUE[r / 2],
                other => {
                    return Err(Error::Unsupported(format!(
                        "RAF: compressed frame with a {other:?} filter"
                    )))
                }
            };
            row.push((buffer, slot));
        }
        map.push(row);
    }
    Ok(map)
}

/// The 6x6 phase the pass schedule is wired to. Every compressed
/// X-Trans RAF carries exactly this array; a file that did not would
/// need a different coded/filled pattern in every pass, so it is
/// refused rather than decoded into a plausible-looking wrong frame.
const XTRANS_PHASE: [[CfaColor; 6]; 6] = {
    use CfaColor::{Blue as B, Green as G, Red as R};
    [
        [G, G, R, G, G, B],
        [G, G, B, G, G, R],
        [B, R, G, R, B, G],
        [G, G, B, G, G, R],
        [G, G, R, G, G, B],
        [R, B, G, B, R, G],
    ]
};

/// Decode one stripe into its columns of the frame.
fn decode_stripe(
    data: &[u8],
    header: &Header,
    qbases: Option<&[u8]>,
    map: &[Vec<(usize, usize)>],
    rows: &mut [&mut [u16]],
) -> Result<()> {
    let mut stripe = Stripe::new(data, header);
    let mut q_base = None;
    for b in 0..header.blocks {
        if let Some(qbases) = qbases {
            let next = qbases.get(b).copied().unwrap_or(0) as i32;
            // The contexts only survive a block boundary when the
            // quantiser does not move.
            if q_base != Some(next) {
                stripe.set_q_base(next);
                q_base = Some(next);
            }
        }
        stripe.block()?;
        // The emit has to come before the retire: the retire clears
        // the very buffers it reads.
        for (r, columns) in map.iter().enumerate() {
            let Some(row) = rows.get_mut(b * BLOCK_ROWS + r) else {
                break;
            };
            for (out, &(buffer, slot)) in row.iter_mut().zip(columns.iter()) {
                *out = stripe.sample(buffer, slot);
            }
        }
        stripe.rotate();
    }
    Ok(())
}

/// Decode a compressed sensor strip into `width * height` samples.
pub fn decode(strip: &[u8], width: usize, height: usize, cfa: &Cfa) -> Result<Vec<u16>> {
    let header = Header::parse(strip)?;
    if header.width != width || header.height != height {
        return corrupt(format!(
            "compressed strip is {}x{} inside a {width}x{height} frame",
            header.width, header.height
        ));
    }
    // Disagreeing with the container about the filter array would mean
    // one of the two was misread, which is a corrupt file rather than
    // an unsupported one.
    let x_trans = matches!(cfa, Cfa::XTrans(_));
    if x_trans != header.is_x_trans() {
        return corrupt(format!(
            "compressed strip says sensor type {:#04x} for a {} frame",
            header.sensor,
            if x_trans { "X-Trans" } else { "Bayer" }
        ));
    }
    if let Cfa::XTrans(grid) = cfa {
        if *grid != XTRANS_PHASE {
            return Err(Error::Unsupported(
                "RAF: compressed X-Trans frame on an unknown filter-array phase".into(),
            ));
        }
    }
    let samples = crate::frame_samples(width, height, 1)?;
    let map = sensor_map(cfa, x_trans, header.stripe_width)?;
    let sizes = header.stripe_lengths(strip)?;

    // Each stripe owns a column range of every row, so handing out the
    // rows split at the stripe boundaries lets the stripes decode
    // straight into the frame in parallel with no sharing at all.
    let mut frame = vec![0u16; samples];
    let mut columns: Vec<Vec<&mut [u16]>> = (0..header.stripes)
        .map(|_| Vec::with_capacity(height))
        .collect();
    for row in frame.chunks_mut(width) {
        let mut rest = row;
        for stripe in columns.iter_mut() {
            let take = rest.len().min(header.stripe_width);
            let (head, tail) = rest.split_at_mut(take);
            stripe.push(head);
            rest = tail;
        }
    }

    let mut at = header.data_offset();
    let mut starts = Vec::with_capacity(header.stripes);
    for size in &sizes {
        starts.push(at);
        at = at.saturating_add(*size);
    }

    columns
        .into_par_iter()
        .enumerate()
        .try_for_each(|(s, mut rows)| -> Result<()> {
            // A stripe's declared length may run past the end of the
            // file; the reader feeds zeros past whatever is there, and
            // a stream that needs them fails on its own.
            let start = starts[s].min(strip.len());
            let end = start.saturating_add(sizes[s]).min(strip.len());
            let qbases = header.qbases(strip, s)?;
            decode_stripe(&strip[start..end], &header, qbases, &map, &mut rows)
        })?;
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The X100F's real strip header, byte for byte.
    const X100F: [u8; 16] = [
        0x49, 0x53, 0x01, 0x10, 0x0e, 0x0f, 0xc6, 0x18, 0x00, 0x17, 0xa0, 0x03, 0x00, 0x08, 0x02,
        0xa1,
    ];
    /// The GFX100S's: lossy, Bayer, sixteen stripes.
    const GFX100S: [u8; 16] = [
        0x49, 0x53, 0x00, 0x00, 0x0e, 0x22, 0x32, 0x30, 0x00, 0x2e, 0x20, 0x03, 0x00, 0x10, 0x05,
        0xb3,
    ];

    #[test]
    fn reads_a_real_lossless_header() {
        let header = Header::parse(&X100F).unwrap();
        assert_eq!(
            header,
            Header {
                lossless: true,
                sensor: 0x10,
                bits: 14,
                height: 4038,
                rounded_width: 6144,
                width: 6048,
                stripe_width: 768,
                stripes: 8,
                blocks: 673,
            }
        );
        // Eight lengths is 32 bytes, already a multiple of 16, and a
        // lossless file carries no quantiser bases.
        assert_eq!(header.data_offset(), 48);
        assert_eq!(header.line_width(), 512);
    }

    #[test]
    fn reads_a_real_lossy_header() {
        let header = Header::parse(&GFX100S).unwrap();
        assert_eq!(
            header,
            Header {
                lossless: false,
                sensor: 0x00,
                bits: 14,
                height: 8754,
                rounded_width: 12288,
                width: 11808,
                stripe_width: 768,
                stripes: 16,
                blocks: 1459,
            }
        );
        // 64 bytes of size table, then sixteen stripes of 1459
        // quantiser bases each padded to 1472.
        assert_eq!(header.data_offset(), 16 + 64 + 16 * 1472);
        assert_eq!(header.data_offset(), 23632);
        assert_eq!(header.line_width(), 384);
    }

    #[test]
    fn the_length_table_is_padded_to_sixteen_bytes() {
        // The X-Pro3's nine stripes: 36 bytes of table, padded to 48.
        let mut header = Header::parse(&X100F).unwrap();
        header.stripes = 9;
        header.rounded_width = 9 * 768;
        assert_eq!(header.data_offset(), 64);
        header.stripes = 16;
        header.rounded_width = 16 * 768;
        assert_eq!(header.data_offset(), 80);
    }

    #[test]
    fn rejects_a_header_that_contradicts_itself() {
        assert!(matches!(Header::parse(&X100F[..8]), Err(Error::Corrupt(_))));
        let bad = |at: usize, to: &[u8]| {
            let mut bad = X100F;
            bad[at..at + to.len()].copy_from_slice(to);
            assert!(
                matches!(Header::parse(&bad), Err(Error::Corrupt(_))),
                "byte {at} = {to:?} was accepted"
            );
        };
        bad(0, &[0, 0]); // no signature
        bad(2, &[2]); // neither lossless nor lossy
        bad(3, &[0x20]); // no such filter array
        bad(4, &[13]); // no such depth
        bad(5, &[0x0e, 0x0d]); // a height that is not six rows a block
        bad(11, &[0x02, 0x00]); // a stripe width Fujifilm never shipped
        bad(13, &[7]); // stripes that do not cover the rounded width
        bad(14, &[0, 100]); // blocks that do not make the height
        bad(9, &[0x03, 0xe8]); // a width rounded up by more than a stripe
                               // A width that is not a whole number of filter-array periods.
        bad(9, &[0x17, 0x9c]);
    }

    #[test]
    fn the_alphabet_width_is_the_bits_the_largest_symbol_needs() {
        assert_eq!(ceil_log2(0), 0);
        assert_eq!(ceil_log2(1), 1);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(16384), 14);
        assert_eq!(ceil_log2(5462), 13);
        assert_eq!(ceil_log2(2342), 12);
    }

    /// The six quantiser bases the format's own worked table lists at
    /// 14 bits, and what each makes of the main table.
    #[test]
    fn the_main_table_matches_the_formats_worked_values() {
        let max_bits = 4 * ceil_log2(16384);
        assert_eq!(max_bits, 56);
        let want = [
            // q_base, t0..t3, total_values, raw_bits, escape, step, sum
            (0, [0, 18, 67, 276], 16384, 14, 41, 1, 256),
            (1, [1, 21, 72, 283], 5462, 13, 42, 3, 85),
            (2, [2, 24, 77, 290], 3278, 12, 43, 5, 51),
            (3, [3, 27, 82, 297], 2342, 12, 43, 7, 37),
            (4, [4, 30, 87, 304], 1822, 11, 44, 9, 28),
            (5, [5, 33, 92, 311], 1491, 11, 44, 11, 23),
        ];
        for (q, t, total, raw_bits, esc, step, sum) in want {
            let table = Table::new(q, 16383, max_bits, 9, 0);
            assert_eq!(table.t, t, "thresholds at q_base {q}");
            assert_eq!(table.total_values, total, "alphabet at q_base {q}");
            assert_eq!(table.raw_bits, raw_bits, "escape width at q_base {q}");
            assert_eq!(table.esc, esc, "escape threshold at q_base {q}");
            assert_eq!(table.step, step, "step at q_base {q}");
            assert_eq!(table.init_sum, sum, "context seed at q_base {q}");
        }
        // The three fine tables, built once a file and never rebuilt.
        for (q, t, total, raw_bits, esc) in [
            (0, [0, 0x12, 0x43, 0x114], 16384, 14, 41),
            (1, [1, 0x15, 0x48, 0x11b], 5462, 13, 42),
            (2, [2, 0x18, 0x4d, 0x122], 3278, 12, 43),
        ] {
            let table = Table::new(q, 16383, max_bits, 3, 5 + q);
            assert_eq!(table.t, t);
            assert_eq!(table.total_values, total);
            assert_eq!(table.raw_bits, raw_bits);
            assert_eq!(table.esc, esc);
        }
        // Shallower frames: the escape threshold moves with the depth.
        let twelve = Table::new(0, 4095, 4 * ceil_log2(4096), 9, 0);
        assert_eq!(
            (
                twelve.total_values,
                twelve.raw_bits,
                twelve.esc,
                twelve.init_sum
            ),
            (4096, 12, 35, 64)
        );
        let sixteen = Table::new(0, 65535, 4 * ceil_log2(65536), 9, 0);
        assert_eq!(
            (
                sixteen.total_values,
                sixteen.raw_bits,
                sixteen.esc,
                sixteen.init_sum
            ),
            (65536, 16, 47, 1024)
        );
    }

    /// The quantiser's comparisons are lopsided, and it only shows on
    /// a difference that lands exactly on a threshold.
    #[test]
    fn the_gradient_quantiser_is_asymmetric_on_its_thresholds() {
        let t = Table::new(0, 16383, 56, 9, 0);
        assert_eq!(t.t, [0, 18, 67, 276]);
        assert_eq!(t.level(0), 0);
        assert_eq!(t.level(1), 1);
        assert_eq!(t.level(-1), -1);
        // On the positive side a threshold belongs to the level above,
        // on the negative side to the level below.
        assert_eq!(t.level(18), 2);
        assert_eq!(t.level(17), 1);
        assert_eq!(t.level(-18), -2);
        assert_eq!(t.level(-17), -1);
        assert_eq!(t.level(276), 4);
        assert_eq!(t.level(-276), -4);
        assert_eq!(t.level(275), 3);
        assert_eq!(t.level(-275), -3);
        assert_eq!(t.level(16383), 4);
        assert_eq!(t.level(-16383), -4);
    }

    /// The shift a context asks for as it warms up. A stripe that
    /// opens on a run of identical samples spends `1 + k` bits a
    /// symbol, and this ladder — one 9-bit code, two 8-bit, four
    /// 7-bit, eight 6-bit, sixteen 5-bit, thirty-two 4-bit — is the
    /// first thing to check against a real file.
    #[test]
    fn the_shift_ladder_warms_up_by_halves() {
        let shift = |count: u32, sum: u32| -> u32 {
            if count >= sum {
                0
            } else {
                let mut k = 1;
                while k < 15 && (count << k) < sum {
                    k += 1;
                }
                k
            }
        };
        let sum = Table::new(0, 16383, 56, 9, 0).init_sum;
        assert_eq!(sum, 256);
        // Zero residuals leave `sum` alone, so only `count` walks.
        let ladder: Vec<u32> = (1..=16).map(|count| shift(count, sum)).collect();
        assert_eq!(ladder, [8, 7, 7, 6, 6, 6, 6, 5, 5, 5, 5, 5, 5, 5, 5, 4]);
        assert_eq!(shift(256, 256), 0);
        assert_eq!(shift(300, 256), 0);
        // A context whose residuals are enormous still stops at 15.
        assert_eq!(shift(1, 1 << 20), 15);
    }

    #[test]
    fn a_zero_run_consumes_its_terminator() {
        // 0b0001_0110: three zeros, the one, then 0b0110 left over.
        let mut pump = BitPumpMsb::new(&[0b0001_0110, 0xff]);
        assert_eq!(zero_run(&mut pump).unwrap(), 3);
        assert_eq!(pump.get(4), 0b0110);
        // A run that spans bytes.
        let mut pump = BitPumpMsb::new(&[0, 0, 0, 0, 0, 0x40]);
        assert_eq!(zero_run(&mut pump).unwrap(), 41);
        // Past the end the reader feeds zeros for ever; the run is
        // refused rather than looped on.
        let mut pump = BitPumpMsb::new(&[0; 4]);
        assert!(zero_run(&mut pump).is_err());
    }

    #[test]
    fn the_even_rules_place_exactly_one_symbol_a_photosite() {
        // Every X-Trans block codes 4608 symbols: the odd half of all
        // twelve buffers, plus whatever evens the rules keep.
        let line_width = 512;
        let mut total = 0;
        for (first, second) in XTRANS_RULES {
            for rule in [first, second] {
                total += line_width / 2; // the odd positions
                total += (0..line_width)
                    .step_by(2)
                    .filter(|p| rule.coded(*p))
                    .count();
            }
        }
        assert_eq!(total, 9 * line_width);
        assert_eq!(total, 6 * STRIPE_WIDTH);
        let bayer: usize = BAYER_RULES.len() * 2 * 384;
        assert_eq!(bayer, 6 * STRIPE_WIDTH);
    }

    /// The smallest legal frame: one stripe, one block.
    fn tiny(lossless: bool, x_trans: bool) -> Vec<u8> {
        let mut head = vec![
            0x49,
            0x53,
            u8::from(lossless),
            if x_trans { 0x10 } else { 0 },
            14,
        ];
        head.extend_from_slice(&6u16.to_be_bytes()); // height
        head.extend_from_slice(&768u16.to_be_bytes()); // rounded width
        head.extend_from_slice(&768u16.to_be_bytes()); // width
        head.extend_from_slice(&768u16.to_be_bytes()); // stripe width
        head.push(1); // stripes
        head.extend_from_slice(&1u16.to_be_bytes()); // blocks
        assert_eq!(head.len(), 16);
        head
    }

    /// Nothing a forged or truncated strip can say may panic: the
    /// reader feeds zeros past the end and every table index is
    /// bounded by construction, so the only outcomes are a frame and
    /// an error.
    #[test]
    fn hostile_strips_return_errors_rather_than_panicking() {
        let xtrans = Cfa::XTrans(XTRANS_PHASE);
        for (lossless, cfa) in [(true, &xtrans), (false, &Cfa::RGGB)] {
            let x_trans = matches!(cfa, Cfa::XTrans(_));
            let mut strip = tiny(lossless, x_trans);
            // A size table, a quantiser base and a body of noise.
            strip.extend_from_slice(&4096u32.to_be_bytes());
            strip.resize(Header::parse(&strip).unwrap().data_offset(), 0xa5);
            let mut seed = 0x1234_5678u32;
            for _ in 0..4096 {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                strip.push((seed >> 16) as u8);
            }
            for cut in 0..=32 {
                let at = strip.len() * cut / 32;
                let _ = decode(&strip[..at], 768, 6, cfa);
            }
            // And a body of nothing but zero bits, which is an
            // unterminated run rather than a frame of zeros.
            let mut zeros = tiny(lossless, x_trans);
            zeros.resize(20_000, 0);
            assert!(decode(&zeros, 768, 6, cfa).is_err());
        }
        // A strip whose header does not match the container it sits in.
        let strip = tiny(true, true);
        assert!(matches!(
            decode(&strip, 768, 6, &Cfa::RGGB),
            Err(Error::Corrupt(_))
        ));
        assert!(matches!(
            decode(&strip, 100, 6, &Cfa::XTrans(XTRANS_PHASE)),
            Err(Error::Corrupt(_))
        ));
        // An X-Trans phase the pass schedule is not wired to.
        let mut phase = XTRANS_PHASE;
        phase[0][0] = CfaColor::Red;
        assert!(matches!(
            decode(&strip, 768, 6, &Cfa::XTrans(phase)),
            Err(Error::Unsupported(_))
        ));
    }

    /// The column-to-slot map is the other half of the same fact as
    /// the pass rules: a slot the rules interpolate must be a slot the
    /// map never reads.
    #[test]
    fn the_map_reads_only_the_slots_the_passes_code() {
        let cfa = Cfa::XTrans(XTRANS_PHASE);
        let map = sensor_map(&cfa, true, STRIPE_WIDTH).unwrap();
        let line_width = STRIPE_WIDTH * 2 / 3;
        // Which (buffer, slot) pairs the six rows read.
        let mut read = std::collections::HashSet::new();
        for row in &map {
            for &pair in row {
                assert!(read.insert(pair), "two sensor columns share {pair:?}");
            }
        }
        assert_eq!(read.len(), BLOCK_ROWS * STRIPE_WIDTH);
        // And which the passes code.
        let mut coded = std::collections::HashSet::new();
        for (pass, &(first, second)) in PASSES.iter().enumerate() {
            for (buffer, rule) in [
                (first, XTRANS_RULES[pass].0),
                (second, XTRANS_RULES[pass].1),
            ] {
                for p in 0..line_width {
                    if !p.is_multiple_of(2) || rule.coded(p) {
                        coded.insert((buffer, p));
                    }
                }
            }
        }
        assert_eq!(read, coded);
    }
}

/// The format's own worked examples, walked one checkpoint at a time
/// against the files they were taken from.
///
/// Every number here is the decoder's own state at a named point in a
/// named file, so a failure says *which* rule is wrong rather than
/// only that the picture is. Gated on `SCHIST_RAW_CORPUS`; silent when
/// the corpus is not there.
#[cfg(test)]
mod checkpoints {
    use super::*;
    use std::path::PathBuf;

    /// A stripe decoded a few blocks deep, with its state kept at
    /// every step it is worth comparing.
    struct Trace {
        stride: usize,
        /// The eighteen buffers after each block's six passes, before
        /// the history rotates.
        after_block: Vec<Vec<i32>>,
        /// ... and after the rotate and clear.
        after_rotate: Vec<Vec<i32>>,
        /// The sensor rows the blocks emitted.
        rows: Vec<Vec<u16>>,
        pass_symbols: Vec<[u32; 6]>,
    }

    impl Trace {
        /// Slots `0 ..= last` of one line buffer.
        fn line(&self, state: &[i32], buffer: usize, last: usize) -> Vec<i32> {
            state[buffer * self.stride..buffer * self.stride + last + 1].to_vec()
        }
        fn block(&self, b: usize, buffer: usize, last: usize) -> Vec<i32> {
            self.line(&self.after_block[b], buffer, last)
        }
        fn rotated(&self, b: usize, buffer: usize, last: usize) -> Vec<i32> {
            self.line(&self.after_rotate[b], buffer, last)
        }
    }

    /// Decode the first `blocks` blocks of stripe 0 of a corpus file,
    /// keeping everything. `strip_at` is the strip's file offset, and
    /// the header there is checked against `head` so a corpus that has
    /// moved fails loudly instead of quietly decoding rubbish.
    fn trace(name: &str, strip_at: usize, head: &[u8; 16], blocks: usize) -> Option<Trace> {
        let root = std::env::var("SCHIST_RAW_CORPUS").ok()?;
        let path = PathBuf::from(root).join("Fujifilm").join(name);
        let bytes = std::fs::read(&path).ok()?;
        let strip = bytes.get(strip_at..)?;
        assert_eq!(&strip[..16], head, "{name}: the strip has moved");
        let header = Header::parse(strip).unwrap();
        let cfa = if header.is_x_trans() {
            Cfa::XTrans(XTRANS_PHASE)
        } else {
            Cfa::RGGB
        };
        let map = sensor_map(&cfa, header.is_x_trans(), header.stripe_width).unwrap();
        let sizes = header.stripe_lengths(strip).unwrap();
        let start = header.data_offset();
        let end = (start + sizes[0]).min(strip.len());
        let qbases = header.qbases(strip, 0).unwrap();

        let mut stripe = Stripe::new(&strip[start..end], &header);
        let mut trace = Trace {
            stride: stripe.stride,
            after_block: Vec::new(),
            after_rotate: Vec::new(),
            rows: Vec::new(),
            pass_symbols: Vec::new(),
        };
        let mut q_base = None;
        for b in 0..blocks {
            if let Some(qbases) = qbases {
                let next = qbases[b] as i32;
                if q_base != Some(next) {
                    stripe.set_q_base(next);
                    q_base = Some(next);
                }
            }
            stripe.block().unwrap();
            trace.pass_symbols.push(stripe.pass_symbols);
            trace.after_block.push(stripe.buf.clone());
            for columns in &map {
                trace.rows.push(
                    columns
                        .iter()
                        .map(|&(buffer, slot)| stripe.sample(buffer, slot))
                        .collect(),
                );
            }
            stripe.rotate();
            trace.after_rotate.push(stripe.buf.clone());
        }
        Some(trace)
    }

    const X100F: &str = "X100F-DSCF5760_x100f_lossless_compressed_raw_Temple.RAF";
    const X100F_AT: usize = 884224;
    const X100F_HEAD: [u8; 16] = [
        0x49, 0x53, 0x01, 0x10, 0x0e, 0x0f, 0xc6, 0x18, 0x00, 0x17, 0xa0, 0x03, 0x00, 0x08, 0x02,
        0xa1,
    ];
    const XT5: &str = "X-T5-DSCF0021.RAF";
    const XT5_AT: usize = 4041216;
    const XT5_HEAD: [u8; 16] = [
        0x49, 0x53, 0x01, 0x10, 0x0e, 0x14, 0x4c, 0x21, 0x00, 0x1e, 0xc0, 0x03, 0x00, 0x0b, 0x03,
        0x62,
    ];
    const GFX: &str = "GFX100S-Fujifilm-GFX100S-14bits-compress-4_3.RAF";
    const GFX_AT: usize = 3453440;
    const GFX_HEAD: [u8; 16] = [
        0x49, 0x53, 0x00, 0x00, 0x0e, 0x22, 0x32, 0x30, 0x00, 0x2e, 0x20, 0x03, 0x00, 0x10, 0x05,
        0xb3,
    ];

    /// Checkpoint 3: the symbol count. A block codes one symbol a
    /// photosite however the photosites are arranged, and the per-pass
    /// split is the cheapest check there is on the visit order.
    #[test]
    fn a_block_codes_one_symbol_a_photosite() {
        let Some(xtrans) = trace(X100F, X100F_AT, &X100F_HEAD, 2) else {
            return;
        };
        assert_eq!(xtrans.pass_symbols[0], [768, 768, 640, 896, 896, 640]);
        assert_eq!(xtrans.pass_symbols[0].iter().sum::<u32>(), 4608);
        assert_eq!(xtrans.pass_symbols[1], [768, 768, 640, 896, 896, 640]);

        let Some(bayer) = trace(GFX, GFX_AT, &GFX_HEAD, 2) else {
            return;
        };
        // Bayer blocks code every slot of every buffer: 2 * 384 a pass.
        assert_eq!(bayer.pass_symbols[0], [768; 6]);
        assert_eq!(bayer.pass_symbols[0].iter().sum::<u32>(), 4608);
    }

    /// Checkpoint 5: the whole buffer state at the end of the first
    /// block. The first four sensor rows of this frame are blank, so
    /// only the row-5 buffers hold anything, and their zero pattern
    /// *is* the coded-versus-filled table.
    #[test]
    fn the_cold_start_leaves_only_the_last_row_populated() {
        let Some(trace) = trace(X100F, X100F_AT, &X100F_HEAD, 2) else {
            return;
        };
        // R2: `slot % 4` in {0, 3} are row 5's reds; `% 4 == 1` is row
        // 4's, which is blank, and `% 4 == 2` is interpolated filler.
        assert_eq!(
            trace.block(0, 4, 34),
            [
                0, 1067, 0, 0, 1058, 1070, 0, 0, 1062, 1064, 0, 0, 1058, 1064, 0, 0, 1051, 1047, 0,
                0, 1051, 1051, 0, 0, 1065, 1077, 0, 0, 1071, 1058, 0, 0, 1083, 1076, 0
            ]
        );
        // G5: only the odd positions carry photosites.
        assert_eq!(
            trace.block(0, 12, 34),
            [
                0, 0, 1110, 0, 1107, 0, 1120, 0, 1121, 0, 1113, 0, 1122, 0, 1083, 0, 1091, 0, 1096,
                0, 1096, 0, 1102, 0, 1130, 0, 1154, 0, 1113, 0, 1109, 0, 1156, 0, 1116
            ]
        );
        // B2: `slot % 4` in {1, 2}.
        assert_eq!(
            trace.block(0, 17, 34),
            [
                0, 0, 1061, 1069, 0, 0, 1063, 1060, 0, 0, 1046, 1056, 0, 0, 1051, 1040, 0, 0, 1050,
                1049, 0, 0, 1055, 1080, 0, 0, 1066, 1074, 0, 0, 1064, 1068, 0, 0, 1067
            ]
        );
        for buffer in 0..LINES {
            if matches!(buffer, 4 | 12 | 17) {
                continue;
            }
            assert!(
                trace.block(0, buffer, 34).iter().all(|v| *v == 0),
                "buffer {buffer} should still be blank"
            );
        }
    }

    /// Checkpoint 6: the column-to-slot map, read off the frame rather
    /// than off the buffers.
    #[test]
    fn the_first_live_row_lands_where_the_filter_array_says() {
        let Some(trace) = trace(X100F, X100F_AT, &X100F_HEAD, 2) else {
            return;
        };
        for row in &trace.rows[..5] {
            assert!(row.iter().all(|v| *v == 0));
        }
        assert_eq!(
            trace.rows[5][..32],
            [
                1067, 1061, 1110, 1069, 1058, 1107, 1070, 1063, 1120, 1060, 1062, 1121, 1064, 1046,
                1113, 1056, 1058, 1122, 1064, 1051, 1083, 1040, 1051, 1091, 1047, 1050, 1096, 1049,
                1051, 1096, 1051, 1055
            ]
        );
    }

    /// Checkpoint 7: the rotate keeps each colour's last two coded
    /// rows, the clear empties the rest, and only the first buffer of
    /// each colour comes out of it holding guards.
    #[test]
    fn retiring_a_block_keeps_two_rows_and_three_pairs_of_guards() {
        let Some(trace) = trace(X100F, X100F_AT, &X100F_HEAD, 2) else {
            return;
        };
        // R-1 is the old R2, guards and all; R-2 is the old R1, blank.
        assert_eq!(trace.rotated(0, 1, 34), trace.block(0, 4, 34));
        assert!(trace.rotated(0, 0, 34).iter().all(|v| *v == 0));
        assert_eq!(trace.rotated(0, 6, 34), trace.block(0, 12, 34));
        assert_eq!(trace.rotated(0, 14, 34), trace.block(0, 17, 34));
        // Only R0's left guard survives the clear, because only R-1's
        // first sample is non-zero on this frame.
        let mut r0 = vec![0; 35];
        r0[0] = 1067;
        assert_eq!(trace.rotated(0, 2, 34), r0);
        for buffer in [3, 4, 7, 8, 9, 10, 11, 12, 15, 16, 17] {
            assert!(
                trace.rotated(0, buffer, 34).iter().all(|v| *v == 0),
                "buffer {buffer} should have been cleared"
            );
        }
    }

    /// Checkpoint 8: the first block where the history, the guards and
    /// the interpolated fillers are all live at once. If this matches,
    /// the X-Trans lossless path is right.
    #[test]
    fn the_second_block_has_a_live_neighbourhood() {
        let Some(trace) = trace(X100F, X100F_AT, &X100F_HEAD, 2) else {
            return;
        };
        let want: [(usize, [i32; 35]); 15] = [
            (
                1,
                [
                    0, 1067, 0, 0, 1058, 1070, 0, 0, 1062, 1064, 0, 0, 1058, 1064, 0, 0, 1051,
                    1047, 0, 0, 1051, 1051, 0, 0, 1065, 1077, 0, 0, 1071, 1058, 0, 0, 1083, 1076,
                    0,
                ],
            ),
            (
                2,
                [
                    1067, 533, 1066, 0, 1174, 799, 1089, 0, 1077, 797, 1069, 0, 1057, 796, 1045, 0,
                    1062, 786, 1059, 0, 1056, 788, 1070, 0, 1084, 804, 1072, 0, 1078, 796, 1066, 0,
                    1071, 808, 1083,
                ],
            ),
            (
                3,
                [
                    533, 799, 1076, 1063, 1086, 939, 1150, 1139, 1132, 931, 1068, 1083, 1100, 923,
                    1059, 1050, 1063, 919, 1071, 1059, 1074, 920, 1059, 1069, 1075, 939, 1098,
                    1069, 1062, 929, 1075, 1056, 1056, 940, 1086,
                ],
            ),
            (
                4,
                [
                    799, 1067, 1104, 1072, 1077, 1076, 1134, 1140, 1220, 1222, 1107, 1083, 1122,
                    1111, 1060, 1055, 1139, 1110, 1072, 1065, 1087, 1135, 1103, 1068, 1090, 1102,
                    1080, 1074, 1075, 1050, 1055, 1060, 1054, 1047, 1049,
                ],
            ),
            (
                6,
                [
                    0, 0, 1110, 0, 1107, 0, 1120, 0, 1121, 0, 1113, 0, 1122, 0, 1083, 0, 1091, 0,
                    1096, 0, 1096, 0, 1102, 0, 1130, 0, 1154, 0, 1113, 0, 1109, 0, 1156, 0, 1116,
                ],
            ),
            (
                7,
                [
                    0, 1116, 1113, 1147, 1176, 1171, 1165, 1141, 1137, 1131, 1112, 1119, 1104,
                    1102, 1102, 1086, 1094, 1101, 1082, 1084, 1091, 1091, 1090, 1132, 1153, 1186,
                    1140, 1176, 1165, 1118, 1104, 1127, 1132, 1203, 1182,
                ],
            ),
            (
                8,
                [
                    1116, 1155, 1105, 1116, 1187, 1258, 1267, 1265, 1204, 1121, 1107, 1111, 1103,
                    1093, 1112, 1087, 1092, 1098, 1111, 1091, 1096, 1091, 1100, 1122, 1133, 1192,
                    1169, 1122, 1150, 1141, 1107, 1122, 1156, 1165, 1197,
                ],
            ),
            (
                9,
                [
                    1155, 1135, 1122, 1121, 1256, 1242, 1325, 1250, 1196, 1120, 1134, 1108, 1150,
                    1097, 1083, 1088, 1097, 1097, 1109, 1090, 1092, 1092, 1112, 1127, 1147, 1184,
                    1139, 1140, 1163, 1137, 1102, 1119, 1155, 1170, 1124,
                ],
            ),
            (
                10,
                [
                    1135, 1200, 1217, 1155, 1102, 1265, 1348, 1223, 1254, 1230, 1179, 1231, 1211,
                    1166, 1133, 1111, 1129, 1125, 1139, 1136, 1122, 1121, 1124, 1124, 1144, 1156,
                    1150, 1148, 1138, 1145, 1159, 1113, 1088, 1121, 1092,
                ],
            ),
            (
                11,
                [
                    1200, 1142, 1200, 1187, 1144, 1132, 1192, 1274, 1330, 1377, 1296, 1246, 1260,
                    1201, 1157, 1143, 1215, 1167, 1124, 1166, 1189, 1225, 1204, 1147, 1150, 1196,
                    1169, 1136, 1141, 1117, 1104, 1103, 1105, 1096, 1100,
                ],
            ),
            (
                12,
                [
                    1142, 1171, 1146, 1182, 1154, 1150, 1260, 1275, 1389, 1345, 1366, 1245, 1263,
                    1181, 1148, 1138, 1263, 1145, 1132, 1164, 1196, 1210, 1276, 1142, 1149, 1179,
                    1160, 1140, 1105, 1119, 1088, 1103, 1083, 1099, 1097,
                ],
            ),
            (
                14,
                [
                    0, 0, 1061, 1069, 0, 0, 1063, 1060, 0, 0, 1046, 1056, 0, 0, 1051, 1040, 0, 0,
                    1050, 1049, 0, 0, 1055, 1080, 0, 0, 1066, 1074, 0, 0, 1064, 1068, 0, 0, 1067,
                ],
            ),
            (
                15,
                [
                    0, 0, 1054, 799, 1088, 0, 1108, 795, 1060, 0, 1054, 789, 1048, 0, 1051, 782,
                    1048, 0, 1054, 787, 1050, 0, 1058, 803, 1066, 0, 1062, 803, 1064, 0, 1067, 800,
                    1078, 0, 1063,
                ],
            ),
            (
                16,
                [
                    0, 1074, 1078, 930, 1082, 1151, 1130, 927, 1099, 1064, 1080, 920, 1068, 1062,
                    1069, 913, 1052, 1063, 1076, 918, 1063, 1056, 1063, 932, 1063, 1062, 1066, 933,
                    1061, 1064, 1057, 933, 1062, 1051, 1049,
                ],
            ),
            (
                17,
                [
                    1074, 806, 1057, 1076, 1083, 1128, 1102, 1195, 1136, 1076, 1133, 1082, 1098,
                    1065, 1080, 1092, 1087, 1063, 1060, 1071, 1110, 1059, 1137, 1086, 1062, 1063,
                    1056, 1063, 1050, 1061, 1043, 1044, 1049, 1053, 1052,
                ],
            ),
        ];
        for (buffer, slots) in want {
            assert_eq!(trace.block(1, buffer, 34), slots, "buffer {buffer}");
        }
        // The two history buffers red and green skipped stay blank.
        for buffer in [0, 5, 13] {
            assert!(trace.block(1, buffer, 34).iter().all(|v| *v == 0));
        }
        assert_eq!(
            trace.rows[6][..32],
            [
                1116, 1113, 1066, 1147, 1176, 1088, 1171, 1165, 1089, 1141, 1137, 1060, 1131, 1112,
                1069, 1119, 1104, 1048, 1102, 1102, 1045, 1086, 1094, 1048, 1101, 1082, 1059, 1084,
                1091, 1050, 1091, 1090
            ]
        );
    }

    /// The same cold start on a second body, with different numbers
    /// and a different geometry (eleven stripes, so a padded size
    /// table).
    #[test]
    fn a_second_body_cold_starts_the_same_way() {
        let Some(trace) = trace(XT5, XT5_AT, &XT5_HEAD, 1) else {
            return;
        };
        assert_eq!(
            trace.block(0, 4, 34),
            [
                0, 1979, 0, 0, 2004, 2016, 0, 0, 1926, 1975, 0, 0, 1967, 1958, 0, 0, 1917, 1963, 0,
                0, 1991, 1946, 0, 0, 1971, 1954, 0, 0, 2028, 1971, 0, 0, 1995, 1995, 0
            ]
        );
        assert_eq!(
            trace.block(0, 12, 34),
            [
                0, 0, 3272, 0, 3292, 0, 3288, 0, 3348, 0, 3236, 0, 3344, 0, 3344, 0, 3308, 0, 3304,
                0, 3316, 0, 3344, 0, 3332, 0, 3328, 0, 3312, 0, 3316, 0, 3340, 0, 3324
            ]
        );
        assert_eq!(
            trace.block(0, 17, 34),
            [
                0, 0, 2717, 2662, 0, 0, 2756, 2729, 0, 0, 2732, 2717, 0, 0, 2701, 2705, 0, 0, 2646,
                2725, 0, 0, 2740, 2744, 0, 0, 2740, 2725, 0, 0, 2799, 2717, 0, 0, 2803
            ]
        );
        assert_eq!(trace.rows[5][..6], [1979, 2717, 3272, 2662, 2004, 3292]);
    }

    /// Checkpoint 10: the lossy path. The quantiser bases, the fine
    /// tables that take over in flat neighbourhoods, the per-block
    /// rebuild and the step that multiplies every residual.
    #[test]
    fn the_lossy_path_quantises_per_block() {
        let Some(root) = std::env::var("SCHIST_RAW_CORPUS").ok() else {
            return;
        };
        let path = PathBuf::from(&root).join("Fujifilm").join(GFX);
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        let strip = &bytes[GFX_AT..];
        let header = Header::parse(strip).unwrap();
        assert!(!header.lossless);
        // The first stripe's data only starts in the right place if
        // the quantiser-base array is accounted for.
        assert_eq!(GFX_AT + header.data_offset(), 3477072);
        assert_eq!(
            header.qbases(strip, 0).unwrap().unwrap()[..16],
            [1, 3, 4, 4, 4, 4, 4, 4, 4, 5, 5, 4, 5, 5, 5, 4]
        );

        let Some(trace) = trace(GFX, GFX_AT, &GFX_HEAD, 2) else {
            return;
        };
        // Row 0 of this frame is blank; row 1 is R0/G0's neighbours,
        // G1 and B0 interleaved, and it is coded on fine table F1
        // even though the block's own quantiser base is 1.
        assert!(trace.rows[0].iter().all(|v| *v == 0));
        assert_eq!(
            trace.rows[1][..32],
            [
                5532, 3709, 5516, 3782, 5537, 3771, 5489, 3714, 5670, 3718, 5549, 3742, 5603, 3743,
                5630, 3737, 5562, 3689, 5617, 3724, 5612, 3694, 5657, 3740, 5619, 3829, 5614, 3700,
                5597, 3734, 5678, 3787
            ]
        );
        assert_eq!(
            trace.rows[2][..32],
            [
                2829, 5483, 2841, 5505, 2889, 5615, 2843, 5636, 2840, 5498, 2791, 5640, 2803, 5605,
                2893, 5570, 2845, 5622, 2908, 5563, 2863, 5759, 2861, 5628, 2860, 5645, 2807, 5669,
                2839, 5501, 2791, 5545
            ]
        );
        // Block 1 raises the quantiser base from 1 to 3, which rebuilds
        // the main table and resets its contexts but not the fine ones.
        // Every slot is populated: a Bayer block fills nothing.
        for (buffer, slots) in [
            (
                2,
                [
                    2835, 2793, 2874, 2830, 2786, 2827, 2870, 2774, 2879, 2847, 2966, 2847, 2851,
                    2860, 2910, 2793, 2822, 2921, 2890, 2904, 2841, 2900, 2873, 2895, 2832, 2866,
                    2824,
                ],
            ),
            (
                3,
                [
                    2793, 2793, 2872, 2809, 2833, 2869, 2825, 2862, 2921, 2842, 2786, 2989, 2873,
                    2884, 2891, 2891, 2869, 2847, 2882, 2789, 2876, 2894, 2845, 2883, 2835, 2850,
                    2832,
                ],
            ),
            (
                7,
                [
                    5521, 5623, 5577, 5481, 5605, 5674, 5525, 5586, 5668, 5540, 5639, 5657, 5554,
                    5639, 5668, 5610, 5514, 5607, 5646, 5573, 5634, 5672, 5614, 5628, 5703, 5698,
                    5711,
                ],
            ),
            (
                8,
                [
                    5623, 5551, 5668, 5645, 5665, 5574, 5558, 5552, 5685, 5574, 5597, 5691, 5623,
                    5583, 5576, 5609, 5623, 5688, 5708, 5658, 5643, 5727, 5592, 5668, 5534, 5653,
                    5633,
                ],
            ),
            (
                15,
                [
                    3637, 3721, 3784, 3736, 3784, 3760, 3713, 3694, 3662, 3716, 3752, 3762, 3752,
                    3751, 3778, 3749, 3805, 3764, 3833, 3734, 3736, 3766, 3829, 3756, 3776, 3788,
                    3770,
                ],
            ),
            (
                16,
                [
                    3721, 3687, 3694, 3718, 3756, 3740, 3739, 3683, 3726, 3732, 3748, 3750, 3685,
                    3746, 3806, 3813, 3776, 3738, 3672, 3678, 3687, 3738, 3716, 3805, 3817, 3773,
                    3766,
                ],
            ),
        ] {
            assert_eq!(trace.block(1, buffer, 26), slots, "buffer {buffer}");
        }
    }
}
