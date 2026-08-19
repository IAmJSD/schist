//! Bounds-checked big-endian cursor over an input slice.
//!
//! Every read returns `Result` — the reader must never panic on malformed
//! input. Sub-cursors (`sub`) confine section parsers to their declared
//! length, so a bad inner length can never walk past its section.

use crate::error::PsdError;
use byteorder::{BigEndian, ByteOrder};

#[derive(Clone)]
pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Cursor<'a> {
        Cursor { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn truncated(&self, needed: usize) -> PsdError {
        PsdError::Truncated {
            offset: self.pos,
            needed,
        }
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], PsdError> {
        if self.remaining() < n {
            return Err(self.truncated(n - self.remaining()));
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), PsdError> {
        self.take(n).map(|_| ())
    }

    /// A sub-cursor over the next `n` bytes; the parent advances past them.
    pub fn sub(&mut self, n: usize) -> Result<Cursor<'a>, PsdError> {
        Ok(Cursor::new(self.take(n)?))
    }

    pub fn u8(&mut self) -> Result<u8, PsdError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, PsdError> {
        Ok(BigEndian::read_u16(self.take(2)?))
    }

    pub fn i16(&mut self) -> Result<i16, PsdError> {
        Ok(BigEndian::read_i16(self.take(2)?))
    }

    pub fn u32(&mut self) -> Result<u32, PsdError> {
        Ok(BigEndian::read_u32(self.take(4)?))
    }

    pub fn i32(&mut self) -> Result<i32, PsdError> {
        Ok(BigEndian::read_i32(self.take(4)?))
    }

    pub fn u64(&mut self) -> Result<u64, PsdError> {
        Ok(BigEndian::read_u64(self.take(8)?))
    }

    pub fn sig4(&mut self) -> Result<[u8; 4], PsdError> {
        let b = self.take(4)?;
        Ok([b[0], b[1], b[2], b[3]])
    }

    /// A length field that is 4 bytes in PSD and 8 bytes in PSB. Several
    /// fields follow this rule: the Layer & Mask Info section length, the
    /// Layer Info length, per-channel data lengths, RLE row-count entries
    /// (u16 vs u32 — see `len_rle_row`), and *specific* additional-info keys.
    pub fn len_psb(&mut self, psb: bool) -> Result<u64, PsdError> {
        if psb {
            self.u64()
        } else {
            Ok(self.u32()? as u64)
        }
    }

    /// RLE per-row byte-count entries are u16 in PSD but u32 in PSB.
    pub fn len_rle_row(&mut self, psb: bool) -> Result<u32, PsdError> {
        if psb {
            self.u32()
        } else {
            Ok(self.u16()? as u32)
        }
    }

    /// Convert a u64 length (possibly from a PSB field) into a usize we can
    /// slice with, rejecting lengths that can't fit in the remaining input.
    pub fn checked_len(&self, len: u64) -> Result<usize, PsdError> {
        if len > self.remaining() as u64 {
            Err(PsdError::Truncated {
                offset: self.pos,
                needed: (len - self.remaining() as u64) as usize,
            })
        } else {
            Ok(len as usize)
        }
    }
}
