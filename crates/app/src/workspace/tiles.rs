//! Composited tiles: the selection outline, the display-tile cache, and
//! the background prefetch that fills it.

use super::*;

impl Workspace {
    // ----- painting -----

    /// The selection's traced boundary, recomputed only when the selection
    /// itself changes.
    pub(super) fn selection_outline(&mut self, generation: u64) -> SelectionOutline {
        if let Some((gen, outline)) = &self.selection_outline {
            if *gen == generation {
                return outline.clone();
            }
        }
        let outline = Arc::new(
            self.doc
                .as_ref()
                .map(|d| d.selection.outline())
                .unwrap_or_default(),
        );
        self.selection_outline = Some((generation, outline.clone()));
        outline
    }

    /// A composited tile after colour management, cached per tile.
    ///
    /// Colour conversion is the expensive part, so it stays tile-cached;
    /// only the cheap assembly below runs per frame.
    pub(super) fn display_tile(&mut self, coord: TileCoord) -> Option<Arc<Vec<u8>>> {
        if let Some(tile) = self.display_tiles.get(&coord) {
            return Some(tile.clone());
        }
        let doc = self.doc.as_ref()?;
        let rgba = self.cache.get(doc, coord);
        let managed = if self.color_managed() {
            let mut buf: Vec<f32> = rgba.iter().map(|&v| v as f32 / 255.0).collect();
            self.to_display(&mut buf);
            Arc::new(
                buf.iter()
                    .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
                    .collect::<Vec<u8>>(),
            )
        } else {
            rgba
        };
        self.display_tiles.insert(coord, managed.clone());
        Some(managed)
    }

    /// Rebuild the idle-prefetch queue: every canvas tile not yet in both
    /// tile caches, ordered so the ones nearest the viewport composite
    /// first -- that is where a scroll will land next.
    ///
    /// `include_visible` queues the on-screen tiles too (ahead of every
    /// ring). The full-quality rebuild renders those itself, so it passes
    /// false; mid-gesture nothing else composites them, and warming them
    /// here is what lets the settle frame land instantly.
    pub(super) fn rebuild_prefetch_queue(
        &mut self,
        canvas: IntRect,
        visible: IntRect,
        include_visible: bool,
    ) {
        let Some(doc) = self.doc.as_ref() else {
            self.prefetch_queue.clear();
            return;
        };
        self.prefetch_stamp = (doc.revision, self.color_epoch);
        let (vx0, vy0) = (
            visible.left.div_euclid(TILE_SIZE),
            visible.top.div_euclid(TILE_SIZE),
        );
        let (vx1, vy1) = (
            (visible.right - 1).div_euclid(TILE_SIZE),
            (visible.bottom - 1).div_euclid(TILE_SIZE),
        );
        let mut missing: Vec<((i64, i64), TileCoord)> = Vec::new();
        for coord in TileCoord::covering(&canvas) {
            if self.cache.contains(coord) && self.display_tiles.contains_key(&coord) {
                continue;
            }
            // Distance outside the visible tile range, in whole tiles.
            let dx = (vx0 - coord.tx).max(coord.tx - vx1).max(0) as i64;
            let dy = (vy0 - coord.ty).max(coord.ty - vy1).max(0) as i64;
            if dx == 0 && dy == 0 && !include_visible {
                continue; // visible; the frame itself renders these
            }
            // Ring first, so coverage grows squarely outward from the
            // viewport; rounder-is-closer breaks ties within a ring.
            missing.push(((dx.max(dy), dx * dx + dy * dy), coord));
        }
        missing.sort_unstable_by_key(|(k, _)| *k);
        missing.truncate(PREFETCH_TILE_BUDGET);
        missing.reverse();
        self.prefetch_queue = missing.into_iter().map(|(_, c)| c).collect();
    }

    /// Start the idle prefetch ticker if there is queued work and none is
    /// running. The task outlives any one queue: `assemble_viewport` swaps
    /// in a fresh queue whenever the view changes and the ticker just
    /// keeps draining whatever is nearest now.
    pub(super) fn kick_prefetch(&mut self, cx: &mut Context<Self>) {
        if self.prefetch_queue.is_empty() || self.prefetch_ticker {
            return;
        }
        self.prefetch_ticker = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(PREFETCH_TICK_MS))
                .await;
            let live = this.update(cx, |ws, _| {
                let more = ws.prefetch_tick();
                if !more {
                    // Cleared in the same update that decides to stop, so
                    // a paint can never observe a ticker that is about to
                    // exit and fail to start a new one.
                    ws.prefetch_ticker = false;
                }
                more
            });
            if !matches!(live, Ok(true)) {
                break;
            }
        })
        .detach();
    }

    /// One idle-prefetch step. Returns whether the ticker should live on.
    pub(super) fn prefetch_tick(&mut self) -> bool {
        // Never compete with an in-flight stroke for the compositor; stay
        // armed until the hand stops. View gestures keep ticking: zoom and
        // pan leave the document-space caches valid, and warming tiles as
        // the view moves is exactly when the prefetch pays off.
        if self.pointer_down {
            return true;
        }
        let stale = match self.doc.as_ref() {
            Some(doc) => (doc.revision, self.color_epoch) != self.prefetch_stamp,
            None => true,
        };
        if stale {
            // An edit landed since the queue was built; the next
            // `assemble_viewport` rebuilds it against fresh damage.
            self.prefetch_queue.clear();
            return false;
        }
        let split = self
            .prefetch_queue
            .len()
            .saturating_sub(PREFETCH_BATCH_TILES);
        let batch = self.prefetch_queue.split_off(split);
        if let Some(doc) = self.doc.as_ref() {
            self.cache.prewarm(doc, &batch);
        }
        for coord in batch {
            self.display_tile(coord);
        }
        !self.prefetch_queue.is_empty()
    }
}
