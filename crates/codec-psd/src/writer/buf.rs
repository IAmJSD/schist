//! Big-endian output buffer with the length-prefix patterns PSD needs.

/// Growable big-endian byte buffer.
#[derive(Debug, Default)]
pub struct Buf {
    data: Vec<u8>,
}

impl Buf {
    pub fn new() -> Buf {
        Buf { data: Vec::new() }
    }

    #[allow(dead_code)] // used by tests and kept as part of the buffer API
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.data
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.data.extend_from_slice(b);
    }

    pub fn u8(&mut self, v: u8) {
        self.data.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.data.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i16(&mut self, v: i16) {
        self.data.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.data.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.data.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.data.extend_from_slice(&v.to_be_bytes());
    }

    /// Write a length field that is u32 in PSD and u64 in PSB.
    pub fn len_psb(&mut self, v: u64, psb: bool) {
        if psb {
            self.u64(v);
        } else {
            self.u32(v as u32);
        }
    }

    /// Zero-pad until the buffer length is a multiple of `align`.
    pub fn pad_to(&mut self, align: usize) {
        while !self.data.len().is_multiple_of(align) {
            self.data.push(0);
        }
    }

    /// Pascal string: length byte + bytes, whole field padded to `align`.
    /// PSD layer names are padded to 4, image-resource names to 2.
    pub fn pascal(&mut self, s: &str, align: usize) {
        let bytes = s.as_bytes();
        // Truncate on a char boundary. Cutting at byte 255 could land
        // mid-codepoint, and this is the name non-unicode-aware readers
        // show, so it would render as a replacement character there. The
        // real name is regenerated as `luni` either way.
        let mut n = bytes.len().min(255);
        while n > 0 && !s.is_char_boundary(n) {
            n -= 1;
        }
        let start = self.data.len();
        self.data.push(n as u8);
        self.data.extend_from_slice(&bytes[..n]);
        while !(self.data.len() - start).is_multiple_of(align) {
            self.data.push(0);
        }
    }

    /// Reserve a 4-byte length slot; returns its offset for `patch_u32`.
    pub fn reserve_u32(&mut self) -> usize {
        let at = self.data.len();
        self.u32(0);
        at
    }

    /// Reserve a length slot sized for the container format.
    pub fn reserve_len(&mut self, psb: bool) -> usize {
        let at = self.data.len();
        if psb {
            self.u64(0);
        } else {
            self.u32(0);
        }
        at
    }

    /// Backfill a reserved u32 with the byte count written after it.
    pub fn patch_u32(&mut self, at: usize) {
        let len = (self.data.len() - at - 4) as u32;
        self.data[at..at + 4].copy_from_slice(&len.to_be_bytes());
    }

    /// Backfill a reserved u32/u64 with the byte count written after it.
    pub fn patch_len(&mut self, at: usize, psb: bool) {
        if psb {
            let len = (self.data.len() - at - 8) as u64;
            self.data[at..at + 8].copy_from_slice(&len.to_be_bytes());
        } else {
            self.patch_u32(at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_endian_scalars() {
        let mut b = Buf::new();
        b.u16(0x1234);
        b.u32(0xDEAD_BEEF);
        assert_eq!(b.into_vec(), vec![0x12, 0x34, 0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn pascal_pads_to_alignment() {
        let mut b = Buf::new();
        b.pascal("ab", 4); // 1 + 2 = 3 -> pad to 4
        assert_eq!(b.into_vec(), vec![2, b'a', b'b', 0]);

        let mut b = Buf::new();
        b.pascal("abc", 4); // 1 + 3 = 4, already aligned
        assert_eq!(b.len(), 4);
    }

    #[test]
    fn patch_writes_trailing_length() {
        let mut b = Buf::new();
        let at = b.reserve_u32();
        b.bytes(&[1, 2, 3]);
        b.patch_u32(at);
        assert_eq!(b.into_vec(), vec![0, 0, 0, 3, 1, 2, 3]);
    }

    #[test]
    fn patch_len_psb_uses_u64() {
        let mut b = Buf::new();
        let at = b.reserve_len(true);
        b.bytes(&[7, 7]);
        b.patch_len(at, true);
        let v = b.into_vec();
        assert_eq!(&v[..8], &[0, 0, 0, 0, 0, 0, 0, 2]);
    }
    #[test]
    fn a_long_name_truncates_on_a_char_boundary() {
        // The pascal name is what non-unicode-aware readers show, and a
        // cut at byte 255 could land inside a codepoint.
        let mut b = Buf::new();
        // 128 two-byte chars is 256 bytes: the 255 limit lands mid-char.
        let name: String = std::iter::repeat_n('é', 128).collect();
        assert_eq!(name.len(), 256);
        b.pascal(&name, 2);
        let out = b.into_vec();
        let n = out[0] as usize;
        assert_eq!(n % 2, 0, "must not cut a two-byte char in half");
        assert!(
            std::str::from_utf8(&out[1..1 + n]).is_ok(),
            "the truncated name must still be valid utf-8"
        );
    }

    #[test]
    fn an_ascii_name_is_unaffected() {
        let mut b = Buf::new();
        b.pascal("Background", 2);
        let out = b.into_vec();
        assert_eq!(out[0] as usize, "Background".len());
    }
}
