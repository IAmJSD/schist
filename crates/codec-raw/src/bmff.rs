//! ISO base media file format boxes, for Canon CR3.
//!
//! Just the box tree: sizes (32-bit and the 64-bit `largesize` form),
//! fourccs, and `uuid` boxes; containers (`moov`, `trak`, `mdia`,
//! `minf`, `stbl`, Canon's `uuid` with the CRAW id) are descended.
//! Nothing here knows what any box means.

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Box_ {
    pub kind: [u8; 4],
    /// The 16-byte type of a `uuid` box.
    pub uuid: Option<[u8; 16]>,
    /// Absolute range of the payload (after the header).
    pub data: std::ops::Range<usize>,
    pub children: Vec<Box_>,
}

impl Box_ {
    /// First child with this fourcc.
    pub fn child(&self, kind: &[u8; 4]) -> Option<&Box_> {
        self.children.iter().find(|b| &b.kind == kind)
    }
    /// Every descendant with this fourcc, depth-first.
    pub fn find_all(&self, kind: &[u8; 4]) -> Vec<&Box_> {
        let mut out = Vec::new();
        for c in &self.children {
            if &c.kind == kind {
                out.push(c);
            }
            out.extend(c.find_all(kind));
        }
        out
    }
}

/// Canon's own uuid boxes: the CRAW one inside `moov` (metadata: CNCV,
/// CCTP, CTBO, the CMT1..CMT4 TIFF blocks and the THMB thumbnail) and
/// the preview one at the top level (PRVW).
pub const UUID_CANON_CRAW: [u8; 16] = [
    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48,
];
pub const UUID_CANON_PREVIEW: [u8; 16] = [
    0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16,
];

/// Depth and count caps: a CR3 nests six levels
/// (moov/trak/mdia/minf/stbl/stsd/CRAW/CMP1) and holds a few dozen
/// boxes, so anything past these is a file trying to make us work.
const MAX_DEPTH: usize = 10;
const MAX_BOXES: usize = 4096;

/// Boxes whose payload is a plain sequence of child boxes.
fn is_container(kind: &[u8; 4]) -> bool {
    matches!(
        kind,
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"dinf" | b"edts" | b"udta"
    )
}

/// Where a sample entry's child boxes begin, counted from its payload
/// (that is, after the 8-byte box header). A VisualSampleEntry's fixed
/// part is 78 bytes; Canon's CRAW adds four more before its CMP1 /
/// CDI1 / JPEG children, which is where 82 comes from; 86 is the same
/// with one more 4-byte field, an offset seen in the wild rather than
/// derived from a spec. The candidates are tried in turn and only
/// accepted if the boxes from there tile the rest of the entry
/// exactly, so a sample entry of some other shape stays a leaf rather
/// than sprouting nonsense.
const SAMPLE_ENTRY_CHILDREN: [usize; 3] = [82, 78, 86];

/// Leading bytes to skip inside a `uuid` payload before its child
/// boxes. Canon's preview uuid starts with eight bytes of its own
/// before the PRVW box; the CRAW uuid starts with boxes straight away.
const UUID_CHILDREN: [usize; 2] = [0, 8];

/// Parse the top-level boxes of a file, descending known containers.
pub fn parse(bytes: &[u8]) -> Result<Vec<Box_>> {
    let mut budget = MAX_BOXES;
    // The top level is parsed leniently: a file truncated mid-`mdat`
    // still yields the `moov` in front of it, which is where everything
    // this crate wants lives.
    let (boxes, _) = boxes_in(bytes, 0, bytes.len(), 0, &mut budget, false);
    if boxes.is_empty() {
        return Err(crate::Error::Corrupt("no ISO-BMFF boxes".into()));
    }
    Ok(boxes)
}

/// Parse the boxes filling `start..end`, returning them and the
/// position after the last one parsed. `sample_entries` marks the
/// children of an `stsd`, which are boxes with a fixed header before
/// their own children rather than plain containers.
fn boxes_in(
    bytes: &[u8],
    start: usize,
    end: usize,
    depth: usize,
    budget: &mut usize,
    sample_entries: bool,
) -> (Vec<Box_>, usize) {
    let mut out = Vec::new();
    let mut at = start;
    if depth > MAX_DEPTH || end > bytes.len() {
        return (out, at);
    }
    while at + 8 <= end && *budget > 0 {
        *budget -= 1;
        let size =
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as u64;
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&bytes[at + 4..at + 8]);
        let mut header = 8usize;
        // size == 1 puts the real size in a 64-bit field after the
        // fourcc (mdat in a large CR3 uses it); size == 0 means the box
        // runs to the end of the enclosing range.
        let size = if size == 1 {
            if at + 16 > end {
                break;
            }
            header = 16;
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[at + 8..at + 16]);
            u64::from_be_bytes(b)
        } else if size == 0 {
            (end - at) as u64
        } else {
            size
        };
        let uuid = if &kind == b"uuid" {
            if at + header + 16 > end {
                break;
            }
            let mut id = [0u8; 16];
            id.copy_from_slice(&bytes[at + header..at + header + 16]);
            header += 16;
            Some(id)
        } else {
            None
        };
        let Ok(size) = usize::try_from(size) else {
            break;
        };
        // A box shorter than its own header, or one running past the
        // end of its parent, is where a truncated or garbage file stops
        // making sense: keep what came before it.
        if size < header {
            break;
        }
        let Some(box_end) = at.checked_add(size) else {
            break;
        };
        if box_end > end {
            break;
        }
        let data = at + header..box_end;
        let children = if sample_entries {
            sample_entry_children(bytes, &data, depth, budget)
        } else {
            children_of(bytes, &kind, uuid.is_some(), &data, depth, budget)
        };
        out.push(Box_ {
            kind,
            uuid,
            data,
            children,
        });
        at = box_end;
    }
    (out, at)
}

/// The children of one box, by fourcc.
fn children_of(
    bytes: &[u8],
    kind: &[u8; 4],
    is_uuid: bool,
    data: &std::ops::Range<usize>,
    depth: usize,
    budget: &mut usize,
) -> Vec<Box_> {
    if is_uuid {
        // A uuid box may hold anything at all — Canon puts an XMP
        // packet in one — so its children only count if they tile the
        // payload exactly.
        for skip in UUID_CHILDREN {
            if let Some(children) = exact(
                bytes,
                data.start.saturating_add(skip),
                data.end,
                depth,
                budget,
                false,
            ) {
                return children;
            }
        }
        return Vec::new();
    }
    if is_container(kind) {
        return boxes_in(bytes, data.start, data.end, depth + 1, budget, false).0;
    }
    if kind == b"stsd" {
        // stsd is a full box: version+flags then an entry count, and
        // only then the sample entries.
        let start = data.start.saturating_add(8);
        if start <= data.end {
            return boxes_in(bytes, start, data.end, depth + 1, budget, true).0;
        }
    }
    Vec::new()
}

/// A sample entry (CRAW, CTMD, ...) carries fixed fields before any
/// child boxes; find where they end by trying the known lengths and
/// keeping the first that parses to exactly the end of the entry.
fn sample_entry_children(
    bytes: &[u8],
    data: &std::ops::Range<usize>,
    depth: usize,
    budget: &mut usize,
) -> Vec<Box_> {
    for skip in SAMPLE_ENTRY_CHILDREN {
        let start = data.start.saturating_add(skip);
        if start >= data.end {
            continue;
        }
        if let Some(children) = exact(bytes, start, data.end, depth, budget, false) {
            return children;
        }
    }
    Vec::new()
}

/// Boxes that tile `start..end` exactly, or `None`: the test that says
/// "this really was a sequence of boxes and not something else".
fn exact(
    bytes: &[u8],
    start: usize,
    end: usize,
    depth: usize,
    budget: &mut usize,
    sample_entries: bool,
) -> Option<Vec<Box_>> {
    if start >= end {
        return None;
    }
    let mut probe = *budget;
    let (boxes, consumed) = boxes_in(bytes, start, end, depth + 1, &mut probe, sample_entries);
    if boxes.is_empty()
        || consumed != end
        || !boxes
            .iter()
            .all(|b| b.kind.iter().all(|c| c.is_ascii_graphic()))
    {
        return None;
    }
    *budget = probe;
    Some(boxes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // Hand-built boxes.
    // ---------------------------------------------------------------

    /// A box with a 32-bit size.
    fn small(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }
    /// A box in the `size == 1` form, with a 64-bit largesize.
    fn large(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = 1u32.to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(&((payload.len() + 16) as u64).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }
    /// A `uuid` box: the fourcc, then the 16-byte type, then payload.
    fn uuid(id: &[u8; 16], payload: &[u8]) -> Vec<u8> {
        let mut body = id.to_vec();
        body.extend_from_slice(payload);
        small(b"uuid", &body)
    }
    /// A box whose size field is zero: it runs to the end of the file.
    fn to_end(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = 0u32.to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    /// Every box in a tree, self included, depth-first.
    fn flatten(boxes: &[Box_]) -> Vec<&Box_> {
        let mut out = Vec::new();
        for b in boxes {
            out.push(b);
            out.extend(flatten(&b.children));
        }
        out
    }

    fn kinds(boxes: &[Box_]) -> Vec<String> {
        boxes
            .iter()
            .map(|b| String::from_utf8_lossy(&b.kind).to_string())
            .collect()
    }

    #[test]
    fn top_level_boxes() {
        let mut file = small(b"ftyp", b"crx isom");
        file.extend(small(b"mdat", &[0u8; 32]));
        let boxes = parse(&file).unwrap();
        assert_eq!(kinds(&boxes), ["ftyp", "mdat"]);
        assert_eq!(boxes[0].data, 8..16);
        assert_eq!(boxes[1].data.len(), 32);
        // mdat is not a container: its payload is pixels, not boxes.
        assert!(boxes[1].children.is_empty());
    }

    #[test]
    fn largesize_and_run_to_end_forms() {
        let mut file = large(b"mdat", &[7u8; 40]);
        file.extend(to_end(b"free", &[0u8; 9]));
        let boxes = parse(&file).unwrap();
        assert_eq!(kinds(&boxes), ["mdat", "free"]);
        assert_eq!(boxes[0].data.len(), 40);
        assert_eq!(boxes[1].data.len(), 9);
    }

    #[test]
    fn containers_are_descended() {
        let stbl = small(b"stbl", &small(b"stsz", &[0u8; 12]));
        let minf = small(b"minf", &[small(b"vmhd", &[0u8; 8]), stbl].concat());
        let mdia = small(b"mdia", &minf);
        let trak = small(b"trak", &mdia);
        let moov = small(b"moov", &trak);
        let boxes = parse(&moov).unwrap();
        let all = flatten(&boxes);
        let names: Vec<_> = all
            .iter()
            .map(|b| String::from_utf8_lossy(&b.kind).to_string())
            .collect();
        assert_eq!(
            names,
            ["moov", "trak", "mdia", "minf", "vmhd", "stbl", "stsz"]
        );
        assert_eq!(boxes[0].find_all(b"stsz").len(), 1);
        assert!(boxes[0].child(b"trak").is_some());
        assert!(
            boxes[0].child(b"stbl").is_none(),
            "child() is not recursive"
        );
    }

    #[test]
    fn uuid_boxes_carry_their_type_and_children() {
        // Canon's metadata uuid holds boxes directly.
        let craw = uuid(
            &UUID_CANON_CRAW,
            &[
                small(b"CNCV", b"CanonCR3_001"),
                small(b"CMT1", b"II\x2a\x00"),
            ]
            .concat(),
        );
        // The preview uuid holds eight bytes of its own first.
        let mut preview_payload = vec![0u8; 8];
        preview_payload.extend(small(b"PRVW", &[0xff, 0xd8, 0xff, 0xdb]));
        let preview = uuid(&UUID_CANON_PREVIEW, &preview_payload);
        // And an XMP packet in a uuid is not boxes at all.
        let xmp = uuid(
            &[0xbe; 16],
            b"\0Packet <x:xmpmeta xmlns:x='adobe:ns:meta/'>",
        );
        let file = [craw, preview, xmp].concat();

        let boxes = parse(&file).unwrap();
        assert_eq!(boxes.len(), 3);
        assert_eq!(boxes[0].uuid, Some(UUID_CANON_CRAW));
        assert_eq!(kinds(&boxes[0].children), ["CNCV", "CMT1"]);
        assert_eq!(boxes[1].uuid, Some(UUID_CANON_PREVIEW));
        assert_eq!(kinds(&boxes[1].children), ["PRVW"]);
        assert_eq!(
            boxes[2].children,
            vec![],
            "an opaque uuid payload stays opaque"
        );
    }

    #[test]
    fn stsd_sample_entries_and_their_children() {
        // A CRAW sample entry: 8-byte box header, 82 bytes of fixed
        // fields (SampleEntry + VisualSampleEntry + Canon's four), then
        // child boxes.
        let mut craw = vec![0u8; 82];
        craw[24..26].copy_from_slice(&1618u16.to_be_bytes()); // width, for flavour
        craw.extend(small(b"CMP1", &[0u8; 52]));
        craw.extend(small(b"CDI1", &[0u8; 44]));
        let mut stsd_payload = vec![0, 0, 0, 0, 0, 0, 0, 1]; // version+flags, entry count
        stsd_payload.extend(small(b"CRAW", &craw));
        let file = small(b"moov", &small(b"stbl", &small(b"stsd", &stsd_payload)));

        let boxes = parse(&file).unwrap();
        let stsd = boxes[0].find_all(b"stsd");
        assert_eq!(stsd.len(), 1);
        assert_eq!(kinds(&stsd[0].children), ["CRAW"]);
        assert_eq!(kinds(&stsd[0].children[0].children), ["CMP1", "CDI1"]);
        // The sample entry keeps its whole payload, fixed fields
        // included, so a decoder can read the dimensions out of it.
        assert_eq!(stsd[0].children[0].data.len(), 82 + 60 + 52);
    }

    #[test]
    fn a_sample_entry_of_another_shape_stays_a_leaf() {
        // CTMD has no child boxes; the bytes at offset 82 must not be
        // mistaken for any.
        let mut stsd_payload = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd_payload.extend(small(b"CTMD", &(0..100u8).collect::<Vec<_>>()));
        let file = small(b"stbl", &small(b"stsd", &stsd_payload));
        let boxes = parse(&file).unwrap();
        let stsd = &boxes[0].children[0];
        assert_eq!(kinds(&stsd.children), ["CTMD"]);
        assert!(stsd.children[0].children.is_empty());
    }

    #[test]
    fn garbage_and_truncation_are_errors_not_panics() {
        let mut craw = vec![0u8; 82];
        craw.extend(small(b"CMP1", &[0u8; 52]));
        let mut stsd_payload = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd_payload.extend(small(b"CRAW", &craw));
        let mut good = small(b"ftyp", b"crx isom");
        good.extend(small(
            b"moov",
            &[
                uuid(&UUID_CANON_CRAW, &small(b"CMT1", b"II\x2a\x00")),
                small(b"stbl", &small(b"stsd", &stsd_payload)),
            ]
            .concat(),
        ));
        good.extend(large(b"mdat", &[0u8; 64]));

        // Every prefix of a real-shaped file.
        for cut in 0..good.len() {
            let result = std::panic::catch_unwind(|| parse(&good[..cut]));
            assert!(result.is_ok(), "panic on a {cut}-byte prefix");
        }
        // Sizes that lie: smaller than the header, bigger than the file.
        for bad in [
            vec![0, 0, 0, 3, b'm', b'o', b'o', b'v'],
            vec![0, 0, 0, 7, b'm', b'o', b'o', b'v', 0],
            vec![0xff, 0xff, 0xff, 0xff, b'm', b'd', b'a', b't'],
            vec![
                0, 0, 0, 1, b'm', b'd', b'a', b't', 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            ],
            // A uuid box that ends inside its own 16-byte type.
            vec![0, 0, 0, 12, b'u', b'u', b'i', b'd', 1, 2, 3, 4],
        ] {
            let result = std::panic::catch_unwind(|| parse(&bad));
            assert!(result.is_ok(), "panic on {bad:02x?}");
            assert!(result.unwrap().is_err(), "{bad:02x?} should not parse");
        }
        // Boxes nested past the depth cap: bounded work, no stack blow.
        let mut deep = small(b"moov", b"");
        for _ in 0..200 {
            deep = small(b"moov", &deep);
        }
        assert!(std::panic::catch_unwind(|| parse(&deep)).is_ok());
    }

    // ---------------------------------------------------------------
    // Corpus: real CR3 files.
    // ---------------------------------------------------------------

    #[test]
    fn corpus_cr3_files_have_the_boxes_a_decoder_needs() {
        let Some(root) = crate::tiff::tests::corpus() else {
            return;
        };
        let mut checked = 0;
        let mut problems: Vec<String> = Vec::new();
        for path in crate::tiff::tests::samples(&root) {
            let is_cr3 = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_uppercase() == "CR3")
                .unwrap_or(false);
            if !is_cr3 {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let name = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
            let boxes = match parse(&bytes) {
                Ok(boxes) => boxes,
                Err(e) => {
                    problems.push(format!("{name}: {e}"));
                    continue;
                }
            };
            checked += 1;
            let all = flatten(&boxes);
            // Collected, not asserted one at a time: one run should
            // report everything wrong with the corpus at once.
            fn want(problems: &mut Vec<String>, name: &str, cond: bool, what: &str) {
                if !cond {
                    problems.push(format!("{name}: no {what}"));
                }
            }
            want(
                &mut problems,
                &name,
                boxes.iter().any(|b| &b.kind == b"ftyp"),
                "top-level ftyp",
            );
            want(
                &mut problems,
                &name,
                boxes.iter().any(|b| &b.kind == b"moov"),
                "top-level moov",
            );
            want(
                &mut problems,
                &name,
                boxes.iter().any(|b| &b.kind == b"mdat"),
                "top-level mdat",
            );
            let craw = all.iter().find(|b| b.uuid == Some(UUID_CANON_CRAW));
            match craw {
                None => problems.push(format!("{name}: no Canon CRAW uuid box")),
                Some(craw) => {
                    for tag in [b"CNCV", b"CMT1", b"CMT2", b"CMT3", b"CMT4", b"THMB"] {
                        if craw.child(tag).is_none() {
                            problems.push(format!(
                                "{name}: CRAW uuid has no {}",
                                String::from_utf8_lossy(tag)
                            ));
                        }
                    }
                    // CMT1..CMT4 are whole little TIFFs (IFD0, Exif,
                    // GPS and the maker's own), which is where the CR3
                    // decoder reads its metadata.
                    if let Some(cmt1) = craw.child(b"CMT1") {
                        match crate::tiff::Tiff::parse_embedded(&bytes, cmt1.data.start) {
                            Ok(tiff) => {
                                let (make, _) = tiff.make_model();
                                if !make.to_ascii_uppercase().starts_with("CANON") {
                                    problems.push(format!("{name}: CMT1 Make is {make:?}"));
                                }
                            }
                            Err(e) => problems.push(format!("{name}: CMT1 is not a TIFF: {e}")),
                        }
                    }
                    // THMB is a full box. In version 0 (every body up
                    // to the EOS R generation) it is width, height,
                    // size and two unknown bytes, then a JFIF stream:
                    // 16 bytes of payload before the JPEG. Version 1,
                    // on the R8 and its contemporaries, is a different
                    // shape holding an HEVC-coded thumbnail (a CISZ
                    // box and an hvcC record), and there is no JPEG in
                    // it at all — a CR3 decoder wanting a preview
                    // should take PRVW there.
                    if let Some(thmb) = craw.child(b"THMB") {
                        let payload = bytes
                            .get(thmb.data.start..thmb.data.end)
                            .unwrap_or_default();
                        match payload.first() {
                            Some(0) => {
                                if !matches!(payload.get(16..18), Some([0xff, 0xd8])) {
                                    problems.push(format!("{name}: THMB v0 holds no JPEG"));
                                }
                            }
                            Some(version) => {
                                eprintln!(
                                    "{name}: THMB version {version} (no JPEG; HEVC thumbnail)"
                                );
                                if !payload.windows(4).take(32).any(|w| w == *b"CISZ") {
                                    problems.push(format!(
                                        "{name}: THMB v{version} is not the HEVC shape either"
                                    ));
                                }
                            }
                            None => problems.push(format!("{name}: empty THMB")),
                        }
                    }
                }
            }
            let preview = all.iter().find(|b| b.uuid == Some(UUID_CANON_PREVIEW));
            match preview {
                None => problems.push(format!("{name}: no Canon preview uuid box")),
                Some(preview) => want(
                    &mut problems,
                    &name,
                    preview.child(b"PRVW").is_some(),
                    "PRVW in the preview uuid",
                ),
            }
            // The raw track's sample entry and its CMP1 (the CRX
            // codec's parameters) must be reachable.
            want(
                &mut problems,
                &name,
                !all.iter()
                    .any(|b| &b.kind == b"stsd" && b.children.is_empty()),
                "children under every stsd",
            );
            let craw_entries: Vec<_> = all.iter().filter(|b| &b.kind == b"CRAW").collect();
            want(
                &mut problems,
                &name,
                !craw_entries.is_empty(),
                "CRAW sample entry",
            );
            want(
                &mut problems,
                &name,
                craw_entries.iter().any(|b| b.child(b"CMP1").is_some()),
                "CMP1 inside a CRAW sample entry",
            );
        }
        assert!(
            problems.is_empty(),
            "{} problems:\n{}",
            problems.len(),
            problems.join("\n")
        );
        eprintln!("corpus: {checked} CR3 files parsed");
    }
}
