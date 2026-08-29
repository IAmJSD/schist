//! The generation stream's framing, kept apart from the socket that
//! carries it so it can be tested a frame at a time.

use crate::{Error, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// How many image slots a layout can have.
///
/// Every frame after the layout addresses a slot with the low seven bits
/// of its first byte, so slot 128 has no way to be spoken about. A layout
/// claiming more is rejected up front rather than half-filled.
pub const SLOT_LIMIT: usize = 128;

/// One part of the output, and how many image slots it holds.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LayoutPart {
    pub part_name: String,
    pub children_count: u32,
}

/// One image slot, addressed on the wire by its flat index across the
/// whole layout: with parts of 3 and 2 children, index 4 is the second
/// child of the second part.
#[derive(Debug, Clone, Default)]
pub struct GeneratedChild {
    pub index: usize,
    /// Every image the provider finished for this slot, in arrival order.
    pub images: Vec<Arc<[u8]>>,
    /// Why the provider refused this slot, if it did.
    pub rejected: Option<String>,
}

/// One part of the layout, with its slots in the declared order.
#[derive(Debug, Clone)]
pub struct GeneratedPart {
    pub part_name: String,
    pub children: Vec<GeneratedChild>,
}

/// What the drain reports as it goes.
#[derive(Debug, Clone)]
pub enum GenEvent {
    /// Always first, and only once. Put placeholders up: no image data
    /// can arrive before it.
    Layout(Vec<LayoutPart>),
    /// A finished image for `index`, or — with `image` unset — the slot
    /// ending without one trailing its status.
    ///
    /// `complete` says whether the slot itself is over. A slot can finish
    /// several images before that happens, so a set `image` with
    /// `complete` false is the ordinary case, not a contradiction.
    Image {
        index: usize,
        image: Option<Arc<[u8]>>,
        complete: bool,
    },
    /// The provider refused this slot. Terminal, like a completion.
    Rejected { index: usize, reason: String },
}

/// Reassembles one generation from the frames of its websocket.
///
/// Frames arrive interleaved across slots, and one slot's image can span
/// any number of them, so the partial chunks live here until the frame
/// with the done bit lets them be joined.
pub struct Reassembler {
    /// Built from the first message. Nothing else means anything until
    /// it exists.
    parts: Option<Vec<GeneratedPart>>,
    /// Flat index -> (part, child). Its length is the total slot count.
    locations: Vec<(usize, usize)>,
    /// Chunks held for the image a slot is part way through.
    chunks: HashMap<usize, Vec<u8>>,
    /// Slots that have had their terminal status.
    complete: HashSet<usize>,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Reassembler {
            parts: None,
            locations: Vec::new(),
            chunks: HashMap::new(),
            complete: HashSet::new(),
        }
    }

    /// Whether every slot has ended, and the socket can be closed.
    ///
    /// A layout with no slots in it has nothing to wait for and is
    /// finished the moment it lands.
    pub fn finished(&self) -> bool {
        self.parts.is_some() && self.complete.len() >= self.locations.len()
    }

    /// The assembled output. Only meaningful once [`finished`] holds.
    ///
    /// [`finished`]: Reassembler::finished
    pub fn into_parts(self) -> Vec<GeneratedPart> {
        self.parts.unwrap_or_default()
    }

    /// Feed a text frame: the layout, or one slot's terminal status.
    pub fn text(&mut self, frame: &str) -> Result<Vec<GenEvent>> {
        let json: serde_json::Value = serde_json::from_str(frame)
            .map_err(|e| Error::Protocol(format!("unreadable JSON on the stream: {e}")))?;

        if self.parts.is_none() {
            return self.set_layout(json);
        }

        let (index, reason) = parse_status(&json)?;
        self.check_index(index)?;
        // Rejected, so the half-finished image being held is never going
        // to be one.
        self.chunks.remove(&index);
        if let Some(reason) = &reason {
            self.child_mut(index).rejected = Some(reason.clone());
        }
        // The status is what ends a slot, whether or not an image came
        // with it, and a repeated status must not end it twice.
        if !self.complete.insert(index) {
            return Ok(Vec::new());
        }
        Ok(vec![match reason {
            Some(reason) => GenEvent::Rejected { index, reason },
            None => GenEvent::Image {
                index,
                image: None,
                complete: true,
            },
        }])
    }

    /// Feed a binary frame: one chunk of one slot's image.
    pub fn binary(&mut self, frame: &[u8]) -> Result<Vec<GenEvent>> {
        if self.parts.is_none() {
            return Err(Error::Protocol(
                "expected the layout before any image data".into(),
            ));
        }
        let Some((&header, chunk)) = frame.split_first() else {
            return Err(Error::Protocol("got an empty frame".into()));
        };
        // Top bit of the header byte is the done flag, the low seven bits
        // are the flat index.
        let done = header & 0b1000_0000 != 0;
        let index = (header & 0b0111_1111) as usize;
        self.check_index(index)?;

        self.chunks
            .entry(index)
            .or_default()
            .extend_from_slice(chunk);
        if !done {
            return Ok(Vec::new());
        }

        // One whole image, but the slot stays open for more until its
        // status turns up.
        let image: Arc<[u8]> = self.chunks.remove(&index).unwrap_or_default().into();
        let complete = self.complete.contains(&index);
        self.child_mut(index).images.push(Arc::clone(&image));
        Ok(vec![GenEvent::Image {
            index,
            image: Some(image),
            complete,
        }])
    }

    fn set_layout(&mut self, json: serde_json::Value) -> Result<Vec<GenEvent>> {
        let layout: Vec<LayoutPart> = serde_json::from_value(json)
            .map_err(|e| Error::Protocol(format!("the layout is unreadable: {e}")))?;
        let total: usize = layout
            .iter()
            .map(|p| p.children_count as usize)
            .sum::<usize>();
        if total > SLOT_LIMIT {
            return Err(Error::Protocol(format!(
                "the layout has {total} image slots, and only {SLOT_LIMIT} can be addressed"
            )));
        }
        let mut parts = Vec::with_capacity(layout.len());
        for (p, part) in layout.iter().enumerate() {
            let mut children = Vec::with_capacity(part.children_count as usize);
            for c in 0..part.children_count as usize {
                children.push(GeneratedChild {
                    index: self.locations.len(),
                    ..Default::default()
                });
                self.locations.push((p, c));
            }
            parts.push(GeneratedPart {
                part_name: part.part_name.clone(),
                children,
            });
        }
        self.parts = Some(parts);
        Ok(vec![GenEvent::Layout(layout)])
    }

    fn check_index(&self, index: usize) -> Result<()> {
        if index >= self.locations.len() {
            return Err(Error::Protocol(format!(
                "index {index} is outside the layout"
            )));
        }
        Ok(())
    }

    /// Only call after [`Reassembler::check_index`].
    fn child_mut(&mut self, index: usize) -> &mut GeneratedChild {
        let (part, child) = self.locations[index];
        &mut self.parts.as_mut().expect("layout")[part].children[child]
    }
}

/// `[index]` or `[index, reason]`, and nothing else.
fn parse_status(json: &serde_json::Value) -> Result<(usize, Option<String>)> {
    let bad = || Error::Protocol(format!("{json} is not a generation status"));
    let items = json.as_array().ok_or_else(bad)?;
    let index = items.first().and_then(|v| v.as_u64()).ok_or_else(bad)? as usize;
    match items.len() {
        1 => Ok((index, None)),
        2 => Ok((index, Some(items[1].as_str().ok_or_else(bad)?.to_string()))),
        _ => Err(bad()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYOUT: &str = r#"[{"part_name":"Cover","children_count":2},
                             {"part_name":"Spread","children_count":1}]"#;

    fn started() -> Reassembler {
        let mut r = Reassembler::new();
        let events = r.text(LAYOUT).unwrap();
        assert!(matches!(&events[..], [GenEvent::Layout(l)] if l.len() == 2));
        assert!(!r.finished());
        r
    }

    /// A chunk frame: `done` in the top bit, the slot index below it.
    fn frame(index: u8, done: bool, body: &[u8]) -> Vec<u8> {
        let mut out = vec![if done { index | 0x80 } else { index }];
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn chunks_join_into_one_image_at_the_done_bit() {
        let mut r = started();
        assert!(r.binary(&frame(0, false, b"PN")).unwrap().is_empty());
        assert!(r.binary(&frame(0, false, b"G-")).unwrap().is_empty());
        let events = r.binary(&frame(0, true, b"data")).unwrap();
        match &events[..] {
            [GenEvent::Image {
                index: 0,
                image: Some(image),
                complete: false,
            }] => assert_eq!(&image[..], b"PNG-data"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn frames_for_different_slots_do_not_mix() {
        let mut r = started();
        r.binary(&frame(0, false, b"aa")).unwrap();
        r.binary(&frame(1, false, b"bb")).unwrap();
        let zero = r.binary(&frame(0, true, b"AA")).unwrap();
        let one = r.binary(&frame(1, true, b"BB")).unwrap();
        assert!(matches!(&zero[..], [GenEvent::Image { image: Some(i), .. }] if &i[..] == b"aaAA"));
        assert!(matches!(&one[..], [GenEvent::Image { image: Some(i), .. }] if &i[..] == b"bbBB"));
    }

    #[test]
    fn a_slot_can_finish_several_images_before_its_status() {
        let mut r = started();
        r.binary(&frame(0, true, b"one")).unwrap();
        r.binary(&frame(0, true, b"two")).unwrap();
        // Only the status ends the slot, so neither image claimed to.
        assert!(!r.finished());
        let events = r.text("[0]").unwrap();
        assert!(matches!(
            &events[..],
            [GenEvent::Image {
                index: 0,
                image: None,
                complete: true
            }]
        ));
        let parts = {
            r.text("[1]").unwrap();
            r.text("[2]").unwrap();
            assert!(r.finished());
            r.into_parts()
        };
        assert_eq!(parts[0].children[0].images.len(), 2);
        assert_eq!(&parts[0].children[0].images[0][..], b"one");
    }

    #[test]
    fn a_rejection_ends_the_slot_and_drops_its_partial_image() {
        let mut r = started();
        r.binary(&frame(1, false, b"half")).unwrap();
        let events = r.text(r#"[1,"no thanks"]"#).unwrap();
        match &events[..] {
            [GenEvent::Rejected { index: 1, reason }] => assert_eq!(reason, "no thanks"),
            other => panic!("{other:?}"),
        }
        // The chunks it was holding are gone, not waiting to be joined to
        // whatever arrives next.
        r.text("[0]").unwrap();
        r.text("[2]").unwrap();
        let parts = r.into_parts();
        assert!(parts[0].children[1].images.is_empty());
        assert_eq!(parts[0].children[1].rejected.as_deref(), Some("no thanks"));
    }

    #[test]
    fn an_empty_layout_is_finished_on_arrival() {
        let mut r = Reassembler::new();
        r.text("[]").unwrap();
        assert!(r.finished());
        assert!(r.into_parts().is_empty());
    }

    #[test]
    fn the_flat_index_runs_across_parts() {
        // Parts of 3 and 2: index 4 is the second child of the second part.
        let mut r = Reassembler::new();
        r.text(r#"[{"part_name":"a","children_count":3},{"part_name":"b","children_count":2}]"#)
            .unwrap();
        r.binary(&frame(4, true, b"x")).unwrap();
        for i in 0..5 {
            r.text(&format!("[{i}]")).unwrap();
        }
        let parts = r.into_parts();
        assert_eq!(&parts[1].children[1].images[0][..], b"x");
        assert_eq!(parts[1].children[1].index, 4);
    }

    #[test]
    fn image_data_before_the_layout_is_refused() {
        let mut r = Reassembler::new();
        assert!(r.binary(&frame(0, true, b"x")).is_err());
    }

    #[test]
    fn frames_outside_the_layout_are_refused() {
        let mut r = started();
        assert!(r.binary(&frame(9, true, b"x")).is_err());
        assert!(r.text("[9]").is_err());
        assert!(r.binary(&[]).is_err());
        assert!(r.text(r#"{"index":0}"#).is_err());
        assert!(r.text(r#"[0,"a","b"]"#).is_err());
    }

    #[test]
    fn a_layout_too_big_to_address_is_refused() {
        // 129 slots: the last one has no seven-bit index to arrive under,
        // so the whole generation would strand on it.
        let mut r = Reassembler::new();
        let err = r
            .text(r#"[{"part_name":"a","children_count":129}]"#)
            .unwrap_err();
        assert!(err.to_string().contains("128"), "{err}");
    }

    #[test]
    fn a_repeated_status_does_not_end_a_slot_twice() {
        let mut r = started();
        assert_eq!(r.text("[0]").unwrap().len(), 1);
        assert_eq!(r.text("[0]").unwrap().len(), 0);
        assert!(!r.finished());
    }
}
