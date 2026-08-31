//! Writing pixels out: per-artboard/slice region export, and export
//! with explicit encoder settings.

use super::*;

impl Workspace {
    /// Export every artboard, or every slice, as its own file next to the
    /// document.
    pub fn export_regions(&mut self, slices: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else { return };
        let regions: Vec<(String, IntRect)> = if slices {
            doc.slices
                .iter()
                .map(|s| (s.name.clone(), s.rect))
                .collect()
        } else {
            doc.artboards
                .iter()
                .map(|a| (a.name.clone(), a.rect))
                .collect()
        };
        if regions.is_empty() {
            self.status = if slices {
                "No slices to export".into()
            } else {
                "No artboards to export".into()
            };
            cx.notify();
            return;
        }
        // No directory to pick in a browser: each region goes straight
        // out as its own download (the browser may ask once about
        // multiple downloads).
        #[cfg(target_arch = "wasm32")]
        {
            let _ = window;
            let base = PathBuf::from("/web/save/export");
            self.write_regions(&base, &regions, cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let dir = doc
                .path
                .as_ref()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("."));
            let rx = cx.prompt_for_new_path(&dir, Some("export"));
            let doc_regions = regions;
            cx.spawn_in(window, async move |this, cx| {
                if let Ok(Ok(Some(path))) = rx.await {
                    this.update_in(cx, |ws, _window, cx| {
                        ws.write_regions(&path, &doc_regions, cx);
                    })
                    .ok();
                }
            })
            .detach();
        }
    }

    /// Write one PNG per region, named `<stem>-<region>.png`.
    pub(super) fn write_regions(
        &mut self,
        base: &std::path::Path,
        regions: &[(String, IntRect)],
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.doc.as_ref() else { return };
        let stem = base
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "export".into());
        let dir = base.parent().unwrap_or(std::path::Path::new("."));
        let mut written = 0usize;
        for (name, rect) in regions {
            let rect = rect.intersect(&doc.canvas_rect());
            if rect.is_empty() {
                continue;
            }
            // Codecs export a whole document, so each region becomes a
            // one-layer document of its flattened pixels.
            let rgba = schist_compositor::composite_region_rgba8(doc, rect);
            let mut region_doc = Document::new(
                name.clone(),
                rect.width() as u32,
                rect.height() as u32,
                doc.depth,
            );
            let mut layer = Layer::new_raster(name.clone());
            schist_core::blit_rgba8(
                &mut layer.as_raster_mut().unwrap().tiles,
                doc.depth,
                IntRect::from_size(rect.width() as u32, rect.height() as u32),
                &rgba,
            );
            region_doc.push_layer(layer);
            let safe: String = name
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '-' })
                .collect();
            let out = dir.join(format!("{stem}-{safe}.png"));
            let Some(codec) = self.png_codec() else {
                continue;
            };
            match codec.export(&region_doc) {
                Ok(bytes) => {
                    #[cfg(not(target_arch = "wasm32"))]
                    if std::fs::write(&out, bytes).is_ok() {
                        written += 1;
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        let file_name = out
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| format!("{safe}.png"));
                        if crate::web::download_bytes(&file_name, &bytes).is_ok() {
                            written += 1;
                        }
                    }
                }
                Err(e) => log::error!("export {name}: {e}"),
            }
        }
        self.status = format!("Exported {written} region(s)").into();
        cx.notify();
    }

    /// The PNG codec, which is how anything leaves the app as a flat image.
    ///
    /// Both callers used to look for the id `"png"`; every codec is
    /// registered as `codec.<format>`, so the lookup never matched and
    /// exporting slices and copying to the system clipboard both returned
    /// early without a word.
    pub(super) fn png_codec(&self) -> Option<&dyn schist_plugin_api::CodecPlugin> {
        self.registry.codecs().find(|c| c.id() == PNG_CODEC_ID)
    }

    /// Export the flattened document with explicit encoder settings.
    pub fn export_with(
        &mut self,
        codec_id: &str,
        options: schist_plugin_api::ExportOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(codec) = self.registry.codecs().find(|c| c.id() == codec_id) else {
            return;
        };
        let ext = codec.extensions().first().copied().unwrap_or("png");
        let doc = self.doc.as_ref();
        let stem = doc
            .map(|d| {
                std::path::Path::new(&d.title)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "untitled".into())
            })
            .unwrap_or_else(|| "untitled".into());
        let suggested = format!("{stem}.{ext}");
        // The browser flow is synchronous: its own prompt asks for the
        // name, and the encoded bytes leave as a download.
        #[cfg(target_arch = "wasm32")]
        {
            let _ = window;
            let Some(name) = crate::web::prompt_string("Export as:", &suggested) else {
                return;
            };
            let result = (|| -> anyhow::Result<()> {
                let doc = self
                    .doc
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("no document"))?;
                let bytes = codec.export_with(doc, &options)?;
                crate::web::download_bytes(&name, &bytes)
            })();
            self.status = match result {
                Ok(()) => format!("Exported {name}").into(),
                Err(err) => format!("Export failed: {err}").into(),
            };
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let dir = doc
                .and_then(|d| d.path.as_ref())
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("."));
            let codec_id = codec_id.to_string();
            let rx = cx.prompt_for_new_path(&dir, Some(&suggested));
            cx.spawn_in(window, async move |this, cx| {
                if let Ok(Ok(Some(path))) = rx.await {
                    this.update_in(cx, |ws, _window, cx| {
                        let result = (|| -> anyhow::Result<()> {
                            let doc = ws
                                .doc
                                .as_ref()
                                .ok_or_else(|| anyhow::anyhow!("no document"))?;
                            let codec = ws
                                .registry
                                .codecs()
                                .find(|c| c.id() == codec_id)
                                .ok_or_else(|| anyhow::anyhow!("codec vanished"))?;
                            let bytes = codec.export_with(doc, &options)?;
                            std::fs::write(&path, bytes)?;
                            Ok(())
                        })();
                        ws.status = match result {
                            Ok(()) => format!("Exported {}", path.display()).into(),
                            Err(err) => format!("Export failed: {err}").into(),
                        };
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        }
    }
}
