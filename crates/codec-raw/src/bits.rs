//! Bit readers and Huffman tables, the ground every compressed raw is
//! decoded on.
//!
//! Four pumps, differing in which end of each byte comes first, the
//! width of the unit they refill from, and whether JPEG's `0xFF 0x00`
//! stuffing is stripped. All are infallible past the end of input:
//! they return zero bits rather than fail, and decoders bound their
//! loops by the pixel count, so a truncated file yields a partly black
//! frame instead of a panic.
//!
//! Every pump keeps a 64-bit accumulator so a `peek` of up to 32 bits
//! is always satisfiable from at most one refill, and refills in
//! multi-byte gulps rather than a bit or a byte at a time: this is the
//! innermost loop of every decoder in the crate.

/// A mask of the low `n` bits, `n` up to 32 (`1 << 32` is why the
/// intermediate is 64-bit).
#[inline(always)]
fn mask(n: u32) -> u32 {
    ((1u64 << n) - 1) as u32
}

/// True if any byte of `word` is `0xFF`. The usual "zero byte in a
/// word" bit trick applied to the complement; used to decide whether a
/// JPEG refill can take the whole word at once or has to walk it
/// looking for stuffing.
#[inline(always)]
fn has_ff(word: u64) -> bool {
    let x = !word;
    (x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080) != 0
}

/// Most-significant-bit-first reader (lossless JPEG without stuffing,
/// Nikon, Pentax, Olympus, Kodak, Hasselblad).
pub struct BitPumpMsb<'a> {
    bytes: &'a [u8],
    /// Next byte to feed the accumulator.
    pos: usize,
    /// The live bits, right-aligned: the next bit out is bit
    /// `nbits - 1`. Bits above `nbits` are spent and ignored.
    cache: u64,
    nbits: u32,
    consumed: usize,
}

/// Least-significant-bit-first reader (Sony ARW, Panasonic's inner
/// stream, Canon sRAW tables). Bytes still arrive in stream order;
/// within a byte the low bit comes first, and a multi-bit value's
/// first-read bit is its low bit.
pub struct BitPumpLsb<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// The live bits, left-aligned at bit 0: the next bit out is bit 0.
    cache: u64,
    nbits: u32,
    consumed: usize,
}

/// MSB-first with JPEG marker stuffing: a `0xFF` followed by `0x00`
/// yields one `0xFF` byte, and a `0xFF` followed by anything else is a
/// marker that ends the stream (further reads give zeros).
pub struct BitPumpJpeg<'a> {
    bytes: &'a [u8],
    pos: usize,
    cache: u64,
    nbits: u32,
    consumed: usize,
    /// Set once a marker has been met; `pos` then rests on its `0xFF`
    /// and no further byte is ever taken from `bytes`.
    marker: bool,
}

/// MSB-first over 32-bit little-endian words (each word's bytes
/// reversed before reading; Olympus/Panasonic-era Sony and some Kodak
/// use it). A trailing group shorter than four bytes is padded with
/// zeros, exactly as a reader that loads a `u32` from a zeroed buffer
/// would see it.
pub struct BitPumpMsb32<'a> {
    bytes: &'a [u8],
    pos: usize,
    cache: u64,
    nbits: u32,
    consumed: usize,
}

/// The operations every pump offers.
pub trait BitPump {
    /// The next `n` bits (0..=32) without consuming them.
    fn peek(&mut self, n: u32) -> u32;
    /// Drop `n` bits (0..=32).
    fn consume(&mut self, n: u32);
    /// Read `n` bits (0..=32).
    fn get(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.consume(n);
        v
    }
    /// Bits consumed so far.
    fn position(&self) -> usize;
}

/// `peek`/`consume`/`position` for the pumps whose accumulator is
/// right-aligned MSB-first; each supplies its own `fill`.
macro_rules! msb_pump {
    ($ty:ident) => {
        impl BitPump for $ty<'_> {
            #[inline(always)]
            fn peek(&mut self, n: u32) -> u32 {
                debug_assert!(n <= 32);
                if self.nbits < n {
                    self.fill(n);
                }
                ((self.cache >> (self.nbits - n)) as u32) & mask(n)
            }
            #[inline(always)]
            fn consume(&mut self, n: u32) {
                debug_assert!(n <= 32);
                if self.nbits < n {
                    self.fill(n);
                }
                self.nbits -= n;
                self.consumed += n as usize;
            }
            #[inline(always)]
            fn position(&self) -> usize {
                self.consumed
            }
        }
    };
}

impl<'a> BitPumpMsb<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        BitPumpMsb {
            bytes,
            pos: 0,
            cache: 0,
            nbits: 0,
            consumed: 0,
        }
    }

    /// Top the accumulator up to at least `n` (<= 32) live bits. With
    /// eight bytes in hand this is one shifted OR; at the end of the
    /// input it feeds zeros for ever.
    #[inline]
    fn fill(&mut self, n: u32) {
        // `need` is capped at 7 so the shifts below are never 64.
        // Because `fill` only runs with `nbits < n <= 32`, need is at
        // least 4, so one gulp always satisfies the request.
        if self.pos + 8 <= self.bytes.len() {
            let word = u64::from_be_bytes(self.bytes[self.pos..self.pos + 8].try_into().unwrap());
            let need = (63 - self.nbits) / 8;
            self.cache = (self.cache << (need * 8)) | (word >> (64 - need * 8));
            self.pos += need as usize;
            self.nbits += need * 8;
            return;
        }
        while self.nbits < n {
            let b = self.bytes.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.cache = (self.cache << 8) | b as u64;
            self.nbits += 8;
        }
    }
}
msb_pump!(BitPumpMsb);

impl<'a> BitPumpLsb<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        BitPumpLsb {
            bytes,
            pos: 0,
            cache: 0,
            nbits: 0,
            consumed: 0,
        }
    }

    #[inline]
    fn fill(&mut self, n: u32) {
        if self.pos + 8 <= self.bytes.len() {
            let word = u64::from_le_bytes(self.bytes[self.pos..self.pos + 8].try_into().unwrap());
            let need = (63 - self.nbits) / 8;
            self.cache |= (word & (mask32(need * 8))) << self.nbits;
            self.pos += need as usize;
            self.nbits += need * 8;
            return;
        }
        while self.nbits < n {
            let b = self.bytes.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.cache |= (b as u64) << self.nbits;
            self.nbits += 8;
        }
    }
}

/// Low-`n`-bits mask as a `u64`, `n` up to 63.
#[inline(always)]
fn mask32(n: u32) -> u64 {
    (1u64 << n) - 1
}

impl BitPump for BitPumpLsb<'_> {
    #[inline(always)]
    fn peek(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if self.nbits < n {
            self.fill(n);
        }
        (self.cache as u32) & mask(n)
    }
    #[inline(always)]
    fn consume(&mut self, n: u32) {
        debug_assert!(n <= 32);
        if self.nbits < n {
            self.fill(n);
        }
        self.cache >>= n;
        self.nbits -= n;
        self.consumed += n as usize;
    }
    #[inline(always)]
    fn position(&self) -> usize {
        self.consumed
    }
}

impl<'a> BitPumpJpeg<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        BitPumpJpeg {
            bytes,
            pos: 0,
            cache: 0,
            nbits: 0,
            consumed: 0,
            marker: false,
        }
    }

    /// The offset of the next byte the pump would take from its input.
    /// Once [`BitPumpJpeg::at_marker`] is true this is the offset of
    /// that marker's `0xFF`, which is how a decoder finds an `RSTn` to
    /// restart after. Note bits already in the accumulator sit *before*
    /// this offset: it is a bound on how far the entropy data was read,
    /// not the exact position of the last consumed bit.
    #[inline]
    pub fn byte_pos(&self) -> usize {
        self.pos
    }

    /// Whether a marker (anything but `FF 00`) has been met, so all
    /// further bits are zeros.
    #[inline]
    pub fn at_marker(&self) -> bool {
        self.marker
    }

    #[inline]
    fn fill(&mut self, n: u32) {
        // The common case: eight bytes in hand with no 0xFF among
        // them, so the stuffing rules cannot apply and the word goes
        // in whole.
        if !self.marker && self.pos + 8 <= self.bytes.len() {
            let word = u64::from_be_bytes(self.bytes[self.pos..self.pos + 8].try_into().unwrap());
            if !has_ff(word) {
                let need = (63 - self.nbits) / 8;
                self.cache = (self.cache << (need * 8)) | (word >> (64 - need * 8));
                self.pos += need as usize;
                self.nbits += need * 8;
                return;
            }
        }
        while self.nbits < n {
            let b = self.next_byte();
            self.cache = (self.cache << 8) | b as u64;
            self.nbits += 8;
        }
    }

    /// One byte of entropy-coded data: `FF 00` collapses to `FF`, and
    /// any other `FF xx` is a marker, which stops the stream where it
    /// stands (leaving `pos` on the `FF`) and yields zeros from then
    /// on. A trailing lone `FF` at the very end counts as a marker
    /// too: there is no second byte to make it a stuffed one.
    #[inline]
    fn next_byte(&mut self) -> u8 {
        if self.marker {
            return 0;
        }
        let Some(&b) = self.bytes.get(self.pos) else {
            self.marker = true;
            return 0;
        };
        if b != 0xFF {
            self.pos += 1;
            return b;
        }
        match self.bytes.get(self.pos + 1) {
            Some(0) => {
                self.pos += 2;
                0xFF
            }
            _ => {
                self.marker = true;
                0
            }
        }
    }
}
msb_pump!(BitPumpJpeg);

impl<'a> BitPumpMsb32<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        BitPumpMsb32 {
            bytes,
            pos: 0,
            cache: 0,
            nbits: 0,
            consumed: 0,
        }
    }

    #[inline]
    fn fill(&mut self, n: u32) {
        // A whole 32-bit word at a time, its bytes in little-endian
        // order, consumed from the top down. `nbits` is under 32 here
        // (fill runs only when `nbits < n <= 32`), so one word is
        // always enough and the accumulator cannot overflow 64 bits.
        while self.nbits < n {
            let word = match self.bytes.get(self.pos..self.pos + 4) {
                Some(w) => u32::from_le_bytes(w.try_into().unwrap()),
                // Past the end, or a tail shorter than a word: pad
                // with zeros.
                None => {
                    let b = |i: usize| self.bytes.get(self.pos + i).copied().unwrap_or(0) as u32;
                    b(0) | (b(1) << 8) | (b(2) << 16) | (b(3) << 24)
                }
            };
            self.pos += 4;
            self.cache = (self.cache << 32) | word as u64;
            self.nbits += 32;
        }
    }
}
msb_pump!(BitPumpMsb32);

/// How many bits of a code the flat lookup covers. Twelve is 4096
/// entries: small enough to build per tile without hurting, long
/// enough that the slow path is rare in camera-written tables.
const LOOKUP_BITS: u32 = 12;

/// A Huffman table in JPEG's form: sixteen counts of codes of each
/// length, then the symbols in code order. Symbols are "difference
/// lengths" in the lossless-JPEG family, so [`HuffTable::decode_diff`]
/// reads the length then the sign-extended value bits in one go.
#[derive(Debug, Clone)]
pub struct HuffTable {
    /// Indexed by the next [`LOOKUP_BITS`] bits: `len << 16 | symbol`,
    /// or 0 when the code is longer than the lookup and the canonical
    /// walk below has to finish the job.
    lookup: Vec<u32>,
    /// Canonical decoding, per code length 1..=16: the first code of
    /// that length, the last one, and where its symbols start in
    /// `symbols`. `max_code` is -1 for a length with no codes.
    min_code: [i32; 17],
    max_code: [i32; 17],
    val_ptr: [i32; 17],
    symbols: Vec<u8>,
}

impl HuffTable {
    /// From DHT-style `bits[1..=16]` (index 0 unused or the 16 counts
    /// from index 0 — accept a 16- or 17-long slice) and the symbols.
    pub fn new(bits: &[u8], symbols: &[u8]) -> Result<HuffTable, crate::Error> {
        // The DHT segment carries sixteen counts; callers that keep
        // JPEG's one-based indexing hand us seventeen with a dead
        // first entry.
        // Both slices are exactly sixteen long here, so the
        // conversion cannot fail; the length check is the match arm.
        let counts: [u8; 16] = match bits.len() {
            16 => bits[..16].try_into().expect("sixteen counts"),
            17 => bits[1..17].try_into().expect("sixteen counts"),
            n => {
                return Err(crate::Error::Corrupt(format!(
                    "huffman table has {n} code-length counts, want 16 or 17"
                )))
            }
        };
        let total: usize = counts.iter().map(|c| *c as usize).sum();
        if total == 0 {
            return Err(crate::Error::Corrupt(
                "huffman table defines no codes".into(),
            ));
        }
        if total > symbols.len() {
            return Err(crate::Error::Corrupt(format!(
                "huffman table promises {total} symbols but carries {}",
                symbols.len()
            )));
        }
        let symbols = symbols[..total].to_vec();

        let mut min_code = [0i32; 17];
        let mut max_code = [-1i32; 17];
        let mut val_ptr = [0i32; 17];
        let mut lookup = vec![0u32; 1 << LOOKUP_BITS];

        let mut code: i32 = 0;
        let mut index: usize = 0;
        for len in 1..=16u32 {
            let n = counts[len as usize - 1] as usize;
            val_ptr[len as usize] = index as i32;
            min_code[len as usize] = code;
            if n > 0 {
                // Every code of this length must fit inside it: an
                // over-subscribed table is a corrupt one, and left
                // unchecked it would alias entries in the lookup.
                if code as i64 + n as i64 > (1i64 << len) {
                    return Err(crate::Error::Corrupt(format!(
                        "huffman table over-subscribed at code length {len}"
                    )));
                }
                for k in 0..n {
                    let symbol = symbols[index + k] as u32;
                    if len <= LOOKUP_BITS {
                        // One entry per continuation of the code:
                        // codes shorter than the lookup width claim a
                        // whole run of indices.
                        let shift = LOOKUP_BITS - len;
                        let base = ((code + k as i32) as usize) << shift;
                        for slot in &mut lookup[base..base + (1 << shift)] {
                            *slot = (len << 16) | symbol;
                        }
                    }
                }
                max_code[len as usize] = code + n as i32 - 1;
                index += n;
                code += n as i32;
            }
            code <<= 1;
        }

        Ok(HuffTable {
            lookup,
            min_code,
            max_code,
            val_ptr,
            symbols,
        })
    }

    /// The next symbol.
    #[inline]
    pub fn decode(&self, pump: &mut impl BitPump) -> u16 {
        let entry = self.lookup[pump.peek(LOOKUP_BITS) as usize];
        if entry != 0 {
            pump.consume(entry >> 16);
            return (entry & 0xFFFF) as u16;
        }
        self.decode_long(pump)
    }

    /// Codes longer than the lookup, walked a bit at a time in the
    /// canonical way. Rare enough that it need not be fast.
    #[cold]
    fn decode_long(&self, pump: &mut impl BitPump) -> u16 {
        let mut code: i32 = 0;
        for len in 1..=16usize {
            code = (code << 1) | pump.get(1) as i32;
            if code <= self.max_code[len] {
                let index = self.val_ptr[len] + (code - self.min_code[len]);
                return self.symbols.get(index as usize).copied().unwrap_or(0) as u16;
            }
        }
        // No code matched in sixteen bits: the stream is garbage (or
        // we have run off the end into the zero fill). Sixteen bits
        // have been consumed, so a decoder's loop still terminates.
        0
    }

    /// Lossless-JPEG difference: decode the length symbol `l`, then
    /// read `l` bits and sign-extend them JPEG-style (a leading 0 bit
    /// means negative: value - (1 << l) + 1). Symbol 16 with no bits
    /// yields 32768 in JPEG; the read-sixteen-bits variant some writers use is the caller's
    /// — see [`HuffTable::decode_diff_ext`].
    #[inline]
    pub fn decode_diff(&self, pump: &mut impl BitPump) -> i32 {
        self.diff(pump, false)
    }

    /// As [`HuffTable::decode_diff`], but symbol 16 reads sixteen
    /// extra bits instead of standing for 32768 on its own. Some DNG
    /// and Hasselblad writers encode the widest difference that way,
    /// and a reader has to be told which convention a file follows.
    #[inline]
    pub fn decode_diff_ext(&self, pump: &mut impl BitPump) -> i32 {
        self.diff(pump, true)
    }

    #[inline(always)]
    fn diff(&self, pump: &mut impl BitPump, ext: bool) -> i32 {
        let l = self.decode(pump) as u32;
        if l == 0 {
            return 0;
        }
        if l == 16 && !ext {
            // T.81's lossless convention: category 16 holds the single
            // difference -32768, which is 32768 modulo 65536, and no
            // extra bits follow.
            return 32768;
        }
        if l > 16 {
            // A symbol outside 0..=16 cannot be a difference length;
            // treat it as no difference rather than shifting wildly.
            return 0;
        }
        let v = pump.get(l) as i32;
        // A leading zero bit marks the negative half of the category.
        if v < 1 << (l - 1) {
            v - (1 << l) + 1
        } else {
            v
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 0b1010_1100, 0b0011_0101
    const AB: &[u8] = &[0xAC, 0x35];

    #[test]
    fn msb_reads_from_the_top_of_each_byte() {
        let mut p = BitPumpMsb::new(AB);
        assert_eq!(p.get(1), 1);
        assert_eq!(p.get(3), 0b010);
        assert_eq!(p.get(4), 0b1100);
        assert_eq!(p.get(8), 0x35);
        assert_eq!(p.position(), 16);
        // Past the end: zeros for ever, no panic.
        assert_eq!(p.get(32), 0);
        assert_eq!(p.get(32), 0);
        assert_eq!(p.position(), 80);
    }

    #[test]
    fn msb_peek_does_not_consume() {
        let mut p = BitPumpMsb::new(AB);
        assert_eq!(p.peek(16), 0xAC35);
        assert_eq!(p.peek(16), 0xAC35);
        assert_eq!(p.position(), 0);
        assert_eq!(p.peek(0), 0);
        p.consume(16);
        assert_eq!(p.position(), 16);
    }

    #[test]
    fn msb_spans_thirty_two_bits() {
        let bytes = [0x12, 0x34, 0x56, 0x78, 0x9A];
        let mut p = BitPumpMsb::new(&bytes);
        assert_eq!(p.get(4), 1);
        assert_eq!(p.get(32), 0x2345_6789);
        assert_eq!(p.get(4), 0xA);
        assert_eq!(p.position(), 40);
    }

    #[test]
    fn msb_long_input_uses_the_word_path() {
        let bytes: Vec<u8> = (0..64u8).collect();
        let mut p = BitPumpMsb::new(&bytes);
        for want in 0..64u32 {
            assert_eq!(p.get(8), want);
        }
        assert_eq!(p.position(), 512);
    }

    #[test]
    fn lsb_reads_from_the_bottom_of_each_byte() {
        let mut p = BitPumpLsb::new(AB);
        // 0xAC = 1010_1100: low bits first are 0,0,1,1,0,1,0,1.
        assert_eq!(p.get(1), 0);
        assert_eq!(p.get(3), 0b110);
        assert_eq!(p.get(4), 0b1010);
        // The second byte's low bit is next; a wider read puts the
        // earlier bits at the bottom.
        assert_eq!(p.get(8), 0x35);
        assert_eq!(p.position(), 16);
        assert_eq!(p.get(32), 0);
    }

    #[test]
    fn lsb_spans_bytes_low_end_first() {
        let bytes = [0x21, 0x43];
        let mut p = BitPumpLsb::new(&bytes);
        assert_eq!(p.get(16), 0x4321);
    }

    #[test]
    fn lsb_long_input_uses_the_word_path() {
        let bytes: Vec<u8> = (0..64u8).collect();
        let mut p = BitPumpLsb::new(&bytes);
        for want in 0..64u32 {
            assert_eq!(p.get(8), want);
        }
        assert_eq!(p.position(), 512);
    }

    #[test]
    fn jpeg_unstuffs_ff_zero() {
        let bytes = [0xFF, 0x00, 0x12, 0xFF, 0x00, 0x34];
        let mut p = BitPumpJpeg::new(&bytes);
        assert_eq!(p.get(8), 0xFF);
        assert_eq!(p.get(8), 0x12);
        assert_eq!(p.get(8), 0xFF);
        assert_eq!(p.get(8), 0x34);
        assert!(!p.at_marker());
        assert_eq!(p.get(8), 0);
        assert!(p.at_marker());
    }

    #[test]
    fn jpeg_stops_at_a_marker() {
        // Three real bytes, then RST0.
        let bytes = [0x11, 0x22, 0x33, 0xFF, 0xD0, 0x44, 0x55];
        let mut p = BitPumpJpeg::new(&bytes);
        assert_eq!(p.get(24), 0x0011_2233);
        assert_eq!(p.get(16), 0);
        assert!(p.at_marker());
        assert_eq!(p.byte_pos(), 3);
        // Data after the marker is never reached by this pump.
        assert_eq!(p.get(32), 0);
        assert_eq!(p.position(), 72);
    }

    #[test]
    fn jpeg_marker_hidden_behind_a_long_run() {
        // Longer than the eight-byte fast path, so the word gulp has
        // to notice the 0xFF and fall back.
        let mut bytes = vec![0x5A; 20];
        bytes.push(0xFF);
        bytes.push(0xD9);
        bytes.extend_from_slice(&[0x77; 8]);
        let mut p = BitPumpJpeg::new(&bytes);
        for _ in 0..20 {
            assert_eq!(p.get(8), 0x5A);
        }
        assert_eq!(p.get(8), 0);
        assert!(p.at_marker());
        assert_eq!(p.byte_pos(), 20);
    }

    #[test]
    fn jpeg_lone_trailing_ff_is_a_marker() {
        let bytes = [0x01, 0xFF];
        let mut p = BitPumpJpeg::new(&bytes);
        assert_eq!(p.get(8), 0x01);
        assert_eq!(p.get(8), 0x00);
        assert!(p.at_marker());
    }

    #[test]
    fn msb32_reverses_each_word() {
        let bytes = [0x01, 0x02, 0x03, 0x04, 0xAA, 0xBB, 0xCC, 0xDD];
        let mut p = BitPumpMsb32::new(&bytes);
        assert_eq!(p.get(32), 0x0403_0201);
        assert_eq!(p.get(32), 0xDDCC_BBAA);
        assert_eq!(p.get(8), 0);
        assert_eq!(p.position(), 72);
    }

    #[test]
    fn msb32_pads_a_short_tail() {
        let bytes = [0x01, 0x02, 0x03, 0x04, 0xAA, 0xBB];
        let mut p = BitPumpMsb32::new(&bytes);
        assert_eq!(p.get(32), 0x0403_0201);
        // The tail is padded to a word with zeros, so the top half of
        // the word is zero and the two bytes come last.
        assert_eq!(p.get(32), 0x0000_BBAA);
        assert_eq!(p.get(16), 0);
    }

    #[test]
    fn msb32_bit_order_within_a_word() {
        let bytes = [0x00, 0x00, 0x00, 0x81];
        let mut p = BitPumpMsb32::new(&bytes);
        assert_eq!(p.get(1), 1);
        assert_eq!(p.get(7), 1);
        assert_eq!(p.get(24), 0);
    }

    /// A table where the codes are 0, 10, 110, 1110, 11110: symbols
    /// 0..=4 in canonical order.
    fn unary_table() -> HuffTable {
        HuffTable::new(
            &[1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            &[0, 1, 2, 3, 4],
        )
        .unwrap()
    }

    #[test]
    fn huffman_decodes_canonical_codes() {
        let t = unary_table();
        // 0 10 110 1110 11110 -> 0,1,2,3,4, then zero fill decodes 0s.
        let bits = [0b0101_1011, 0b1011_1100];
        let mut p = BitPumpMsb::new(&bits);
        assert_eq!(t.decode(&mut p), 0);
        assert_eq!(t.decode(&mut p), 1);
        assert_eq!(t.decode(&mut p), 2);
        assert_eq!(t.decode(&mut p), 3);
        assert_eq!(t.decode(&mut p), 4);
        assert_eq!(p.position(), 15);
    }

    #[test]
    fn huffman_accepts_the_seventeen_entry_form() {
        let a = HuffTable::new(
            &[1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            &[0, 1, 2, 3, 4],
        )
        .unwrap();
        let b = HuffTable::new(
            &[9, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            &[0, 1, 2, 3, 4],
        )
        .unwrap();
        let mut pa = BitPumpMsb::new(&[0b0101_1011]);
        let mut pb = BitPumpMsb::new(&[0b0101_1011]);
        assert_eq!(a.decode(&mut pa), b.decode(&mut pb));
        assert_eq!(a.decode(&mut pa), b.decode(&mut pb));
    }

    #[test]
    fn huffman_rejects_broken_tables() {
        // More symbols promised than supplied.
        assert!(HuffTable::new(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], &[7]).is_err());
        // Three codes of length one cannot exist.
        assert!(HuffTable::new(
            &[3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            &[1, 2, 3]
        )
        .is_err());
        // Wrong count length.
        assert!(HuffTable::new(&[1, 0, 0], &[1]).is_err());
    }

    #[test]
    fn huffman_handles_codes_longer_than_the_lookup() {
        // One code of every length 1..=15 plus two of 16: a complete
        // code that forces the slow path for the long ones.
        let mut counts = [1u8; 16];
        counts[15] = 2;
        let symbols: Vec<u8> = (0..17u8).collect();
        let t = HuffTable::new(&counts, &symbols).unwrap();
        // Symbol 15's code is fifteen 1s followed by a 0; symbol 16 is
        // sixteen 1s.
        let bits = [0xFF, 0xFE, 0xFF, 0xFF];
        let mut p = BitPumpMsb::new(&bits);
        assert_eq!(t.decode(&mut p), 15);
        assert_eq!(p.position(), 16);
        assert_eq!(t.decode(&mut p), 16);
        assert_eq!(p.position(), 32);
    }

    #[test]
    fn huffman_diff_sign_extends() {
        let t = unary_table();
        // Symbol 3 (code 1110) then three bits.
        for (value, want) in [
            (0b111u32, 7),
            (0b100, 4),
            (0b011, -4),
            (0b000, -7),
            (0b001, -6),
        ] {
            let byte = [((0b1110u32 << 4) | (value << 1)) as u8];
            let mut p = BitPumpMsb::new(&byte);
            assert_eq!(t.decode_diff(&mut p), want, "value {value:03b}");
        }
        // Symbol 0 is a zero difference and reads no extra bits.
        let mut p = BitPumpMsb::new(&[0]);
        assert_eq!(t.decode_diff(&mut p), 0);
        assert_eq!(p.position(), 1);
    }

    #[test]
    fn huffman_diff_sixteen_is_special() {
        let mut counts = [0u8; 16];
        counts[0] = 1;
        let t = HuffTable::new(&counts, &[16]).unwrap();
        // The lone code is a single 0 bit.
        let mut p = BitPumpMsb::new(&[0x0F, 0xFF]);
        assert_eq!(t.decode_diff(&mut p), 32768);
        assert_eq!(p.position(), 1);
        let mut p = BitPumpMsb::new(&[0x0F, 0xFF]);
        // The extension convention reads sixteen bits instead: the
        // fifteen bits left in the input plus one zero from the fill,
        // 0b0001_1111_1111_1110, whose leading zero makes it negative.
        assert_eq!(t.decode_diff_ext(&mut p), 0x1FFE - (1 << 16) + 1);
        assert_eq!(p.position(), 17);
    }
}
