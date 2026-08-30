//! Bridging the internal clipboard to the system one.

use super::*;

impl Workspace {
    /// Push the internal clipboard out to the system clipboard as a PNG.
    ///
    /// Schist's own copy/paste has always worked between its documents;
    /// this is what makes it work with everything else.
    pub fn sync_clipboard_out(&mut self, cx: &mut Context<Self>) {
        let Some(clip) = self.editor.clipboard.clone() else {
            return;
        };
        let (w, h) = (clip.rect.width() as u32, clip.rect.height() as u32);
        if w == 0 || h == 0 {
            return;
        }
        let Some(codec) = self.png_codec() else {
            return;
        };
        // Codecs export documents, so the clipboard becomes a one-layer one.
        let mut doc = Document::new("clipboard", w, h, Depth::Eight);
        let mut layer = Layer::new_raster("clipboard");
        schist_core::blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(w, h),
            &clip.rgba,
        );
        doc.push_layer(layer);
        match codec.export(&doc) {
            Ok(bytes) => {
                let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes);
                cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image));
            }
            Err(e) => log::error!("clipboard export: {e}"),
        }
    }

    /// Pull an image off the system clipboard into the internal one.
    ///
    /// Returns false when the clipboard holds nothing we can use, so the
    /// caller can fall back to whatever was copied inside the app.
    pub fn sync_clipboard_in(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        for entry in item.entries() {
            let gpui::ClipboardEntry::Image(image) = entry else {
                continue;
            };
            if image.bytes.is_empty() {
                continue;
            }
            // Route it through the codecs, so anything Schist can open
            // it can also paste.
            let Some(codec) = self.registry.codecs().find(|c| c.probe(&image.bytes)) else {
                continue;
            };
            match codec.import(&image.bytes) {
                Ok(doc) => {
                    let rect = doc.canvas_rect();
                    let rgba = schist_compositor::composite_region_rgba8(&doc, rect);
                    self.editor.clipboard =
                        Some(Arc::new(schist_plugin_api::ClipboardImage { rect, rgba }));
                    return true;
                }
                Err(e) => log::error!("clipboard import: {e}"),
            }
        }
        false
    }
}
