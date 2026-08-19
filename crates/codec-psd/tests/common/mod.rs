//! Minimal hand-rolled PSD *builder* for tests only.
//!
//! This is deliberately NOT the M6 writer: it emits just enough of the
//! format (header, resources, layer records, lsct group markers, masks,
//! luni names, raw/RLE channels, PSB length widening, composite) to
//! exercise the reader. Byte layout follows the Adobe spec.

#![allow(dead_code)]

/// One image resource block.
pub struct Res {
    pub id: u16,
    pub name: Vec<u8>,
    pub data: Vec<u8>,
}

/// The standard resolution resource (0x03ED), hres/vres as fixed 16.16 dpi.
pub fn resolution_res(dpi: f32) -> Res {
    let fixed = (dpi * 65536.0) as u32;
    let mut d = Vec::new();
    d.extend(fixed.to_be_bytes());
    d.extend(1u16.to_be_bytes()); // display unit: ppi
    d.extend(1u16.to_be_bytes()); // width unit: inches
    d.extend(fixed.to_be_bytes());
    d.extend(1u16.to_be_bytes());
    d.extend(1u16.to_be_bytes());
    Res {
        id: 0x03ED,
        name: Vec::new(),
        data: d,
    }
}

pub struct Mask {
    /// (top, left, bottom, right)
    pub rect: (i32, i32, i32, i32),
    pub default_color: u8,
    pub flags: u8,
    /// Row-major 8-bit coverage, len = w*h of `rect`.
    pub pixels: Vec<u8>,
}

pub struct L {
    pub name: String,
    pub unicode_name: Option<String>,
    /// (top, left, bottom, right)
    pub rect: (i32, i32, i32, i32),
    /// Row-major RGBA8, len = w*h. Used unless `raw_planes` is set.
    pub rgba: Vec<[u8; 4]>,
    /// Direct (channel id, uncompressed big-endian plane bytes) override —
    /// used for 16/32-bit and grayscale tests.
    pub raw_planes: Option<Vec<(i16, Vec<u8>)>>,
    pub blend: [u8; 4],
    pub opacity: u8,
    pub clipping: u8,
    pub flags: u8,
    pub rle: bool,
    /// Emit channel compression method 2 (zip) with a bogus payload.
    pub zip: bool,
    /// ('lsct' divider type, optional blend key inside the lsct block).
    pub lsct: Option<(u32, Option<[u8; 4]>)>,
    pub mask: Option<Mask>,
    /// Extra additional-info blocks (key, data), emitted after lsct/luni.
    pub extra_blocks: Vec<([u8; 4], Vec<u8>)>,
    pub omit_alpha: bool,
}

impl Default for L {
    fn default() -> Self {
        L {
            name: "layer".into(),
            unicode_name: None,
            rect: (0, 0, 0, 0),
            rgba: Vec::new(),
            raw_planes: None,
            blend: *b"norm",
            opacity: 255,
            clipping: 0,
            flags: 0,
            rle: false,
            zip: false,
            lsct: None,
            mask: None,
            extra_blocks: Vec::new(),
            omit_alpha: false,
        }
    }
}

impl L {
    /// A solid-color raster layer.
    pub fn solid(name: &str, rect: (i32, i32, i32, i32), rgba: [u8; 4]) -> L {
        let (t, l, b, r) = rect;
        let n = ((b - t) * (r - l)) as usize;
        L {
            name: name.into(),
            rect,
            rgba: vec![rgba; n],
            ..L::default()
        }
    }

    /// The hidden bounded-section divider that starts a group's children.
    pub fn divider() -> L {
        L {
            name: "</Layer group>".into(),
            lsct: Some((3, None)),
            ..L::default()
        }
    }

    /// The group header layer that closes a group (type 1 open, 2 closed).
    pub fn group_header(name: &str, ty: u32, blend: Option<[u8; 4]>) -> L {
        L {
            name: name.into(),
            lsct: Some((ty, blend)),
            blend: *b"pass",
            ..L::default()
        }
    }
}

pub struct Psd {
    /// 1 = PSD, 2 = PSB.
    pub version: u16,
    pub width: u32,
    pub height: u32,
    pub channels: u16,
    /// Bits per channel: 8/16/32.
    pub depth: u16,
    /// 3 = RGB, 1 = Grayscale.
    pub mode: u16,
    pub color_mode_data: Vec<u8>,
    pub resources: Vec<Res>,
    pub layers: Vec<L>,
    /// Emit the layer count negated (merged-transparency flag).
    pub negative_count: bool,
    /// Merged composite as RGBA8 (only meaningful for 8-bit docs); zeros
    /// otherwise. Channel planes emitted raw (compression 0).
    pub composite_rgba8: Option<Vec<u8>>,
    /// Emit the layer info inside a document-level 'Lr16'/'Lr32'/'Layr'
    /// block (how Photoshop stores 16/32-bit layer trees) instead of the
    /// Layer Info sub-section.
    pub layers_in_lr16: bool,
}

impl Psd {
    pub fn rgb8(width: u32, height: u32) -> Psd {
        Psd {
            version: 1,
            width,
            height,
            channels: 3,
            depth: 8,
            mode: 3,
            color_mode_data: Vec::new(),
            resources: Vec::new(),
            layers: Vec::new(),
            negative_count: false,
            composite_rgba8: None,
            layers_in_lr16: false,
        }
    }

    fn psb(&self) -> bool {
        self.version == 2
    }

    /// A length field that is u32 in PSD, u64 in PSB.
    fn push_len(&self, out: &mut Vec<u8>, v: u64) {
        if self.psb() {
            out.extend(v.to_be_bytes());
        } else {
            out.extend((v as u32).to_be_bytes());
        }
    }

    pub fn build(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // --- header ---
        out.extend(b"8BPS");
        out.extend(self.version.to_be_bytes());
        out.extend([0u8; 6]);
        out.extend(self.channels.to_be_bytes());
        out.extend(self.height.to_be_bytes());
        out.extend(self.width.to_be_bytes());
        out.extend(self.depth.to_be_bytes());
        out.extend(self.mode.to_be_bytes());
        // --- color mode data ---
        out.extend((self.color_mode_data.len() as u32).to_be_bytes());
        out.extend(&self.color_mode_data);
        // --- image resources ---
        let mut res = Vec::new();
        for r in &self.resources {
            res.extend(b"8BIM");
            res.extend(r.id.to_be_bytes());
            res.push(r.name.len() as u8);
            res.extend(&r.name);
            if (1 + r.name.len()) % 2 == 1 {
                res.push(0);
            }
            res.extend((r.data.len() as u32).to_be_bytes());
            res.extend(&r.data);
            if r.data.len() % 2 == 1 {
                res.push(0);
            }
        }
        out.extend((res.len() as u32).to_be_bytes());
        out.extend(res);
        // --- layer & mask info ---
        let li = self.build_layer_info();
        let mut sec = Vec::new();
        if self.layers_in_lr16 {
            self.push_len(&mut sec, 0); // empty Layer Info
            sec.extend(0u32.to_be_bytes()); // empty global layer mask info
            sec.extend(b"8BIM");
            let key: &[u8; 4] = match self.depth {
                16 => b"Lr16",
                32 => b"Lr32",
                _ => b"Layr",
            };
            sec.extend(key);
            // Lr16/Lr32/Layr are u64-length keys in PSB.
            self.push_len(&mut sec, li.len() as u64);
            sec.extend(&li);
            let pad = (4 - li.len() % 4) % 4; // doc-level blocks pad to 4
            sec.extend(std::iter::repeat_n(0u8, pad));
        } else {
            self.push_len(&mut sec, li.len() as u64);
            sec.extend(&li);
            sec.extend(0u32.to_be_bytes()); // empty global layer mask info
        }
        self.push_len(&mut out, sec.len() as u64);
        out.extend(sec);
        // --- merged image data ---
        out.extend(self.build_composite());
        out
    }

    fn build_layer_info(&self) -> Vec<u8> {
        if self.layers.is_empty() && !self.negative_count {
            return Vec::new();
        }
        let mut li = Vec::new();
        let count = self.layers.len() as i16;
        let count = if self.negative_count { -count } else { count };
        li.extend(count.to_be_bytes());
        let chans: Vec<Vec<(i16, Vec<u8>)>> =
            self.layers.iter().map(|l| self.layer_channels(l)).collect();
        for (l, ch) in self.layers.iter().zip(&chans) {
            self.emit_record(&mut li, l, ch);
        }
        for ch in &chans {
            for (_, data) in ch {
                li.extend(data);
            }
        }
        if li.len() % 2 == 1 {
            li.push(0); // layer info section is padded to a multiple of 2
        }
        li
    }

    /// Per-channel (id, compression field + compressed payload).
    fn layer_channels(&self, l: &L) -> Vec<(i16, Vec<u8>)> {
        let (t, lf, b, r) = l.rect;
        let (w, h) = ((r - lf).max(0) as usize, (b - t).max(0) as usize);
        let mut planes: Vec<(i16, Vec<u8>)> = match &l.raw_planes {
            Some(raw) => raw.clone(),
            None => {
                let plane = |c: usize| l.rgba.iter().map(|px| px[c]).collect::<Vec<u8>>();
                let mut v = vec![(0i16, plane(0)), (1, plane(1)), (2, plane(2))];
                if !l.omit_alpha {
                    v.push((-1, plane(3)));
                }
                v
            }
        };
        if let Some(m) = &l.mask {
            planes.push((-2, m.pixels.clone()));
        }
        planes
            .into_iter()
            .map(|(id, plane)| {
                let (rows, row_bytes) = if id == -2 {
                    let (mt, ml, mb, mr) = l.mask.as_ref().unwrap().rect;
                    ((mb - mt).max(0) as usize, (mr - ml).max(0) as usize)
                } else {
                    (h, w * (self.depth as usize / 8))
                };
                let mut data = Vec::new();
                let comp: u16 = if l.zip {
                    2
                } else if l.rle {
                    1
                } else {
                    0
                };
                data.extend(comp.to_be_bytes());
                match comp {
                    1 => {
                        let rows_enc: Vec<Vec<u8>> = (0..rows)
                            .map(|row| packbits(&plane[row * row_bytes..(row + 1) * row_bytes]))
                            .collect();
                        for enc in &rows_enc {
                            if self.psb() {
                                data.extend((enc.len() as u32).to_be_bytes());
                            } else {
                                data.extend((enc.len() as u16).to_be_bytes());
                            }
                        }
                        for enc in rows_enc {
                            data.extend(enc);
                        }
                    }
                    _ => data.extend(&plane), // raw, or bogus zip payload
                }
                (id, data)
            })
            .collect()
    }

    fn emit_record(&self, out: &mut Vec<u8>, l: &L, chans: &[(i16, Vec<u8>)]) {
        let (t, lf, b, r) = l.rect;
        out.extend(t.to_be_bytes());
        out.extend(lf.to_be_bytes());
        out.extend(b.to_be_bytes());
        out.extend(r.to_be_bytes());
        out.extend((chans.len() as u16).to_be_bytes());
        for (id, data) in chans {
            out.extend(id.to_be_bytes());
            self.push_len(out, data.len() as u64); // u64 channel lengths in PSB
        }
        out.extend(b"8BIM");
        out.extend(&l.blend);
        out.push(l.opacity);
        out.push(l.clipping);
        out.push(l.flags);
        out.push(0); // filler

        let mut ex = Vec::new();
        // mask block: 0 = none, 20 = rect + default + flags + 2 pad bytes
        match &l.mask {
            Some(m) => {
                ex.extend(20u32.to_be_bytes());
                let (mt, ml, mb, mr) = m.rect;
                ex.extend(mt.to_be_bytes());
                ex.extend(ml.to_be_bytes());
                ex.extend(mb.to_be_bytes());
                ex.extend(mr.to_be_bytes());
                ex.push(m.default_color);
                ex.push(m.flags);
                ex.extend(0u16.to_be_bytes());
            }
            None => ex.extend(0u32.to_be_bytes()),
        }
        ex.extend(0u32.to_be_bytes()); // blending ranges: empty
        let nb = l.name.as_bytes();
        let nl = nb.len().min(255);
        ex.push(nl as u8);
        ex.extend(&nb[..nl]);
        let pad = (4 - (1 + nl) % 4) % 4; // pascal name pads to 4
        ex.extend(std::iter::repeat_n(0u8, pad));

        if let Some((ty, blend)) = &l.lsct {
            ex.extend(b"8BIM");
            ex.extend(b"lsct");
            match blend {
                Some(key) => {
                    ex.extend(12u32.to_be_bytes());
                    ex.extend(ty.to_be_bytes());
                    ex.extend(b"8BIM");
                    ex.extend(key);
                }
                None => {
                    ex.extend(4u32.to_be_bytes());
                    ex.extend(ty.to_be_bytes());
                }
            }
        }
        if let Some(un) = &l.unicode_name {
            let units: Vec<u16> = un.encode_utf16().collect();
            ex.extend(b"8BIM");
            ex.extend(b"luni");
            ex.extend((4 + units.len() as u32 * 2).to_be_bytes());
            ex.extend((units.len() as u32).to_be_bytes());
            for u in units {
                ex.extend(u.to_be_bytes());
            }
        }
        for (key, data) in &l.extra_blocks {
            ex.extend(b"8BIM");
            ex.extend(key);
            ex.extend((data.len() as u32).to_be_bytes());
            ex.extend(data);
            if data.len() % 2 == 1 {
                ex.push(0); // layer-level blocks pad to 2
            }
        }
        out.extend((ex.len() as u32).to_be_bytes());
        out.extend(ex);
    }

    fn build_composite(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(0u16.to_be_bytes()); // raw
        let plane_bytes = (self.width * self.height) as usize * (self.depth as usize / 8);
        match &self.composite_rgba8 {
            Some(rgba) => {
                assert_eq!(rgba.len(), (self.width * self.height) as usize * 4);
                for c in 0..self.channels as usize {
                    out.extend(rgba.chunks_exact(4).map(|px| px[c.min(3)]));
                }
            }
            None => out.extend(vec![0u8; plane_bytes * self.channels as usize]),
        }
        out
    }
}

/// Minimal PackBits encoder (repeat packets for runs >= 2, literals
/// otherwise), producing both packet kinds for decoder coverage.
pub fn packbits(row: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < row.len() {
        let mut run = 1;
        while i + run < row.len() && row[i + run] == row[i] && run < 128 {
            run += 1;
        }
        if run >= 2 {
            out.push((1i32 - run as i32) as u8);
            out.push(row[i]);
            i += run;
        } else {
            let start = i;
            while i < row.len() && i - start < 128 {
                if i + 1 < row.len() && row[i + 1] == row[i] {
                    break;
                }
                i += 1;
            }
            out.push((i - start - 1) as u8);
            out.extend(&row[start..i]);
        }
    }
    out
}
