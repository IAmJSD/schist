//! Yjs-compatible shared image model. Each property and raster/mask tile is a
//! separate CRDT key; editing one layer never overwrites a peer's other layer.
use anyhow::{anyhow, bail, ensure, Result};
use schist_color::Depth;
use schist_core::BlendMode;
use schist_core::{Document, Layer, LayerId, LayerKind, TileBuf, TileCoord, TileMap, TILE_PIXELS};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};
use yrs::{
    updates::{decoder::Decode, encoder::Encode},
    Any, Doc, Map, Out, ReadTxn, StateVector, Transact, Update,
};

type Fields = BTreeMap<String, Vec<u8>>;
#[derive(serde::Serialize, serde::Deserialize)]
struct Recovery {
    state: serde_bytes::ByteBuf,
    bootstrap: Option<BTreeMap<String, serde_bytes::ByteBuf>>,
    deferred: BTreeMap<String, Option<serde_bytes::ByteBuf>>,
}
pub struct SharedDocument {
    doc: Doc,
    previous: Fields,
    ids: HashMap<LayerId, String>,
    local_ids: HashMap<String, LayerId>,
    pub revision: u64,
    bootstrap: Option<Fields>,
    deferred: BTreeMap<String, Option<Vec<u8>>>,
    undo: yrs::UndoManager,
}
impl SharedDocument {
    pub fn new(source: &Document) -> Result<Self> {
        let mut s = Self::unseeded(source)?;
        s.seed_if_empty()?;
        Ok(s)
    }
    pub fn unseeded(source: &Document) -> Result<Self> {
        let doc = Doc::new();
        let root = doc.get_or_insert_map("schist.image.v1");
        let mut undo = yrs::UndoManager::with_scope_and_options(
            &doc,
            &root,
            yrs::undo::Options {
                capture_timeout_millis: 500,
                tracked_origins: Default::default(),
                capture_transaction: None,
                timestamp: std::sync::Arc::new(|| {
                    web_time::SystemTime::now()
                        .duration_since(web_time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64
                }),
            },
        );
        undo.include_origin("local");
        let mut s = Self {
            doc,
            undo,
            bootstrap: Some(Fields::new()),
            deferred: BTreeMap::new(),
            previous: Fields::new(),
            ids: HashMap::new(),
            local_ids: HashMap::new(),
            revision: source.revision,
        };
        fn seed(
            layers: &[Layer],
            prefix: &str,
            ids: &mut HashMap<LayerId, String>,
            local: &mut HashMap<String, LayerId>,
        ) {
            for (i, l) in layers.iter().enumerate() {
                let id = format!("{prefix}/{i}");
                ids.insert(l.id, id.clone());
                local.insert(id.clone(), l.id);
                if let Some(children) = l.children() {
                    seed(children, &id, ids, local);
                }
            }
        }
        seed(&source.tree.layers, "seed", &mut s.ids, &mut s.local_ids);
        s.previous = s.capture(source)?;
        s.bootstrap = Some(s.previous.clone());
        Ok(s)
    }
    pub fn seed_if_empty(&mut self) -> Result<()> {
        let root = self.doc.get_or_insert_map("schist.image.v1");
        if let Some(initial) = self.bootstrap.take() {
            if root.len(&self.doc.transact()) == 0 {
                // A join to an existing room wins over the downloaded export.
                // Peers initializing the same original emit an identical seed.
                let seed = Doc::with_client_id(1);
                let map = seed.get_or_insert_map("schist.image.v1");
                {
                    let mut txn = seed.transact_mut();
                    for (k, v) in initial {
                        map.insert(&mut txn, k, Any::Buffer(Arc::from(v)));
                    }
                }
                let update = seed
                    .transact()
                    .encode_state_as_update_v1(&StateVector::default());
                self.apply(&update)?;
            }
            // Edits during join are deltas, not a replacement of the remote snapshot.
            let mut txn = self.doc.transact_mut_with("local");
            for (k, v) in std::mem::take(&mut self.deferred) {
                match v {
                    Some(v) => {
                        root.insert(&mut txn, k, Any::Buffer(Arc::from(v)));
                    }
                    None => {
                        root.remove(&mut txn, &k);
                    }
                }
            }
        }
        Ok(())
    }
    pub fn undo(&mut self, redo: bool) -> bool {
        if redo {
            self.undo.redo_blocking()
        } else {
            self.undo.undo_blocking()
        }
    }
    pub fn state_vector(&self) -> Vec<u8> {
        self.doc.transact().state_vector().encode_v1()
    }
    pub fn diff(&self, vector: &[u8]) -> Result<Vec<u8>> {
        let sv = StateVector::decode_v1(vector)?;
        Ok(self.doc.transact().encode_state_as_update_v1(&sv))
    }
    pub fn full_state(&self) -> Vec<u8> {
        self.doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default())
    }
    pub fn checkpoint(&self) -> Result<Vec<u8>> {
        Ok(rmp_serde::to_vec_named(&Recovery {
            state: self.full_state().into(),
            bootstrap: self.bootstrap.as_ref().map(|fields| {
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone().into()))
                    .collect()
            }),
            deferred: self
                .deferred
                .iter()
                .map(|(k, v)| (k.clone(), v.clone().map(Into::into)))
                .collect(),
        })?)
    }
    pub fn restore(&mut self, bytes: &[u8], source: &Document) -> Result<Document> {
        let saved: Recovery = rmp_serde::from_slice(bytes)?;
        self.apply(&saved.state)?;
        self.bootstrap = saved
            .bootstrap
            .map(|fields| fields.into_iter().map(|(k, v)| (k, v.into_vec())).collect());
        self.deferred = saved
            .deferred
            .into_iter()
            .map(|(k, v)| (k, v.map(|b| b.into_vec())))
            .collect();
        // Display recovered edits immediately, retaining their unjoined deltas for sync.
        let mut preview = Self::unseeded(source)?;
        preview.apply(&self.full_state())?;
        preview.bootstrap = self.bootstrap.clone();
        preview.deferred = self.deferred.clone();
        preview.seed_if_empty()?;
        let document = preview.render()?;
        self.ids = preview.ids;
        self.local_ids = preview.local_ids;
        self.previous = preview.previous;
        self.revision = document.revision;
        Ok(document)
    }
    pub fn apply(&mut self, bytes: &[u8]) -> Result<()> {
        self.doc
            .transact_mut_with("remote")
            .apply_update(Update::decode_v1(bytes)?)?;
        Ok(())
    }
    /// Capture only local deltas against the last rendered state. Concurrent remote
    /// changes already present in the CRDT are never rewritten from a stale UI snapshot.
    pub fn local_changes(&mut self, source: &Document) -> Result<Option<Vec<u8>>> {
        let next = self.capture(source)?;
        let sv = self.doc.transact().state_vector();
        let map = self.doc.get_or_insert_map("schist.image.v1");
        let mut changed = false;
        {
            let mut txn = self.doc.transact_mut_with("local");
            for (k, v) in &next {
                if self.previous.get(k) != Some(v) {
                    if self.bootstrap.is_some() {
                        self.deferred.insert(k.clone(), Some(v.clone()));
                    } else {
                        map.insert(&mut txn, k.as_str(), Any::Buffer(Arc::from(v.clone())));
                    }
                    changed = true;
                }
            }
            for k in self.previous.keys() {
                if !next.contains_key(k) {
                    if self.bootstrap.is_some() {
                        self.deferred.insert(k.clone(), None);
                    } else {
                        map.remove(&mut txn, k);
                    }
                    changed = true;
                }
            }
        }
        self.previous = next;
        self.revision = source.revision;
        Ok(changed.then(|| self.doc.transact().encode_state_as_update_v1(&sv)))
    }
    fn capture(&mut self, source: &Document) -> Result<Fields> {
        let mut out = Fields::new();
        out.insert(
            "document/size".into(),
            rmp_serde::to_vec(&(source.width, source.height, source.resolution_dpi))?,
        );
        out.insert("document/title".into(), source.title.as_bytes().to_vec());
        let mut meta = Document::new("metadata", 1, 1, source.depth);
        meta.mode = source.mode;
        meta.icc_profile = source.icc_profile.clone();
        meta.preserved_resources = source.preserved_resources.clone();
        meta.global_layer_mask = source.global_layer_mask.clone();
        meta.preserved_layer_info = source.preserved_layer_info.clone();
        meta.guides = source.guides.clone();
        meta.artboards = source.artboards.clone();
        meta.slices = source.slices.clone();
        meta.notes = source.notes.clone();
        meta.counts = source.counts.clone();
        meta.paths = source.paths.clone();
        out.insert(
            "document/metadata".into(),
            schist_codec_psd::write_psd(&meta)?,
        );
        self.capture_layers(&source.tree.layers, "root", source.depth, &mut out)?;
        // Comp references use stable shared IDs, never another process's LayerId values.
        let mut references = Vec::new();
        let mut comps = source.layer_comps.clone();
        for comp in &mut comps {
            comp.states.retain_mut(|state| {
                let Some(id) = self.ids.get(&state.layer) else {
                    return false;
                };
                state.layer = LayerId(references.len() as u64);
                references.push(id.clone());
                true
            });
        }
        out.insert(
            "document/comps".into(),
            rmp_serde::to_vec(&(references, comps))?,
        );
        Ok(out)
    }
    fn capture_layers(
        &mut self,
        layers: &[Layer],
        parent: &str,
        depth: Depth,
        out: &mut Fields,
    ) -> Result<()> {
        for (i, layer) in layers.iter().enumerate() {
            let id = self
                .ids
                .entry(layer.id)
                .or_insert_with(|| uuid::Uuid::new_v4().to_string())
                .clone();
            self.local_ids.insert(id.clone(), layer.id);
            let prefix = format!("layer/{id}");
            // Parent + sibling rank is one atomic placement. Tie-break concurrent insertions
            // by stable UUID; no layer is lost if two users insert at the same position.
            out.insert(
                format!("{prefix}/placement"),
                rmp_serde::to_vec(&(parent, i as u64))?,
            );
            for (name, data) in [
                ("name", layer.name.as_bytes().to_vec()),
                ("visible", vec![layer.visible as u8]),
                ("opacity", layer.opacity.to_le_bytes().to_vec()),
                ("fill", layer.fill_opacity.to_le_bytes().to_vec()),
                ("blend", layer.blend.psd_key().to_vec()),
                ("clipping", vec![layer.clipping as u8]),
                ("locked", vec![layer.locked as u8]),
            ] {
                out.insert(format!("{prefix}/{name}"), data);
            }
            let mut template = layer.clone();
            template.id = LayerId(1);
            template.name = "layer".into();
            template.visible = true;
            template.opacity = 1.0;
            template.fill_opacity = 1.0;
            template.blend = BlendMode::Normal;
            template.clipping = false;
            template.locked = false;
            template.styled = None;
            if let Some(r) = template.as_raster_mut() {
                r.tiles = TileMap::new();
            }
            if let Some(mask) = &mut template.mask {
                mask.tiles = Default::default();
            }
            if let LayerKind::Group(g) = &mut template.kind {
                g.children.clear();
            }
            let mut doc = Document::new("layer", 1, 1, depth);
            doc.tree.layers.push(template);
            out.insert(
                format!("{prefix}/template"),
                schist_codec_psd::write_psd(&doc)?,
            );
            if let Some(r) = layer.as_raster() {
                for (c, tile) in r.tiles.iter() {
                    out.insert(
                        format!("{prefix}/pixels/{}/{}", c.tx, c.ty),
                        tile_bytes(tile),
                    );
                }
            }
            if let Some(mask) = &layer.mask {
                for (c, tile) in mask.tiles.iter() {
                    out.insert(
                        format!("{prefix}/mask/{}/{}", c.tx, c.ty),
                        tile.as_slice().to_vec(),
                    );
                }
            }
            if let Some(children) = layer.children() {
                self.capture_layers(children, &id, depth, out)?;
            }
        }
        Ok(())
    }
    pub fn render(&mut self) -> Result<Document> {
        let map = self.doc.get_or_insert_map("schist.image.v1");
        let txn = self.doc.transact();
        let fields: Fields = map
            .iter(&txn)
            .map(|(k, v)| match v {
                Out::Any(Any::Buffer(b)) => Ok((k.to_string(), b.to_vec())),
                _ => Err(anyhow!("Invalid image field")),
            })
            .collect::<Result<_>>()?;
        drop(txn);
        let get = |key: &str| {
            fields
                .get(key)
                .ok_or_else(|| anyhow!("Incomplete shared image: {key}"))
        };
        let mut doc = schist_codec_psd::read_psd(get("document/metadata")?)?;
        let (w, h, dpi): (u32, u32, f32) = rmp_serde::from_slice(get("document/size")?)?;
        ensure!(
            w > 0 && h > 0 && w <= 300000 && h <= 300000,
            "Invalid shared canvas size"
        );
        doc.width = w;
        doc.height = h;
        doc.resolution_dpi = dpi;
        doc.title = String::from_utf8(get("document/title")?.clone())?;
        let mut layers = HashMap::new();
        let mut places: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
        for (key, data) in &fields {
            let Some(id) = key
                .strip_prefix("layer/")
                .and_then(|k| k.strip_suffix("/template"))
            else {
                continue;
            };
            let p = format!("layer/{id}");
            // Deleting a layer removes its placement. Ignore concurrent writes to its old fields.
            let Some(placement) = fields.get(&format!("{p}/placement")) else {
                continue;
            };
            let (parent, rank): (String, u64) = rmp_serde::from_slice(placement)?;
            let mut parsed = schist_codec_psd::read_psd(data)?;
            ensure!(parsed.tree.layers.len() == 1, "Invalid layer template");
            let mut layer = parsed.tree.layers.remove(0);
            layer.id = *self
                .local_ids
                .entry(id.into())
                .or_insert_with(LayerId::next);
            self.ids.insert(layer.id, id.into());
            layer.name = String::from_utf8(get(&format!("{p}/name"))?.clone())?;
            layer.visible = get(&format!("{p}/visible"))?.first() == Some(&1);
            layer.locked = get(&format!("{p}/locked"))?.first() == Some(&1);
            layer.clipping = get(&format!("{p}/clipping"))?.first() == Some(&1);
            let float = |field: &str| -> Result<f32> {
                let b = get(&format!("{p}/{field}"))?;
                let f = f32::from_le_bytes(b.as_slice().try_into()?);
                ensure!(f.is_finite(), "Invalid opacity");
                Ok(f.clamp(0.0, 1.0))
            };
            layer.opacity = float("opacity")?;
            layer.fill_opacity = float("fill")?;
            let blend: [u8; 4] = get(&format!("{p}/blend"))?.as_slice().try_into()?;
            layer.blend =
                BlendMode::from_psd_key(&blend).ok_or_else(|| anyhow!("Invalid blend mode"))?;
            if let Some(r) = layer.as_raster_mut() {
                r.tiles = TileMap::new();
            }
            for (k, b) in fields.range(format!("{p}/")..) {
                if !k.starts_with(&format!("{p}/")) {
                    break;
                }
                for channel in ["pixels", "mask"] {
                    if let Some(pos) = k.strip_prefix(&format!("{p}/{channel}/")) {
                        let (x, y) = pos
                            .split_once('/')
                            .ok_or_else(|| anyhow!("Invalid tile coordinate"))?;
                        let c = TileCoord {
                            tx: x.parse()?,
                            ty: y.parse()?,
                        };
                        ensure!(
                            c.tx.unsigned_abs() < 1_000_000 && c.ty.unsigned_abs() < 1_000_000,
                            "Tile coordinate out of bounds"
                        );
                        if channel == "pixels" {
                            if let Some(r) = layer.as_raster_mut() {
                                r.tiles.insert(c, Arc::new(read_tile(b)?));
                            }
                        } else if let Some(mask) = &mut layer.mask {
                            let a: [u8; TILE_PIXELS] = b.as_slice().try_into()?;
                            mask.tiles.insert(c, Arc::new(a));
                        }
                    }
                }
            }
            places.entry(parent).or_default().push((rank, id.into()));
            layers.insert(id.to_string(), layer);
        }
        fn build(
            parent: &str,
            places: &mut BTreeMap<String, Vec<(u64, String)>>,
            layers: &mut HashMap<String, Layer>,
            seen: &mut HashSet<String>,
            depth: usize,
        ) -> Result<Vec<Layer>> {
            ensure!(depth < 128, "Shared layer tree too deep");
            let mut siblings = places.remove(parent).unwrap_or_default();
            siblings.sort();
            let mut out = Vec::new();
            for (_, id) in siblings {
                if !seen.insert(id.clone()) {
                    continue;
                }
                if let Some(mut l) = layers.remove(&id) {
                    if let LayerKind::Group(g) = &mut l.kind {
                        g.children = build(&id, places, layers, seen, depth + 1)?;
                    }
                    out.push(l);
                }
            }
            Ok(out)
        }
        doc.tree.layers = build("root", &mut places, &mut layers, &mut HashSet::new(), 0)?;
        // Concurrent moves can leave an orphan/cycle. Keep those layers visible at root.
        let mut orphan: Vec<_> = layers.into_iter().collect();
        orphan.sort_by(|a, b| a.0.cmp(&b.0));
        doc.tree.layers.extend(orphan.into_iter().map(|(_, l)| l));
        if let Some(data) = fields.get("document/comps") {
            let (references, mut comps): (Vec<String>, Vec<schist_core::annotate::LayerComp>) =
                rmp_serde::from_slice(data)?;
            for comp in &mut comps {
                comp.states.retain_mut(|state| {
                    let Some(id) = references
                        .get(state.layer.0 as usize)
                        .and_then(|id| self.local_ids.get(id))
                    else {
                        return false;
                    };
                    state.layer = *id;
                    doc.tree.find(*id).is_some()
                });
            }
            doc.layer_comps = comps;
        }
        doc.active_layer = doc.tree.layers.last().map(|l| l.id);
        doc.mark_dirty();
        self.previous = fields;
        self.revision = doc.revision;
        Ok(doc)
    }
}
fn tile_bytes(tile: &TileBuf) -> Vec<u8> {
    let mut out = Vec::with_capacity(tile.byte_len() + 1);
    match tile {
        TileBuf::U8(b) => {
            out.push(8);
            out.extend_from_slice(b);
        }
        TileBuf::U16(b) => {
            out.push(16);
            for v in b.iter() {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        TileBuf::F32(b) => {
            out.push(32);
            for v in b.iter() {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    out
}
fn read_tile(b: &[u8]) -> Result<TileBuf> {
    ensure!(!b.is_empty(), "Empty tile");
    let n = TILE_PIXELS * 4;
    Ok(match b[0] {
        8 => {
            ensure!(b.len() == 1 + n, "Invalid u8 tile");
            TileBuf::U8(b[1..].to_vec().into_boxed_slice())
        }
        16 => {
            ensure!(b.len() == 1 + n * 2, "Invalid u16 tile");
            TileBuf::U16(
                b[1..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect(),
            )
        }
        32 => {
            ensure!(b.len() == 1 + n * 4, "Invalid f32 tile");
            TileBuf::F32(
                b[1..]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes(*c))
                    .collect(),
            )
        }
        _ => bail!("Unknown tile depth"),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Document {
        let mut d = Document::new("Photo", 256, 256, Depth::Eight);
        d.tree.layers.push(Layer::new_raster("A"));
        d.tree.layers.push(Layer::new_raster("B"));
        d
    }
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn concurrent_properties_and_tiles_merge() {
        let mut da = sample();
        let mut a = SharedDocument::new(&da).unwrap();
        let mut b = SharedDocument::new(&da).unwrap();
        let mut db = b.render().unwrap();
        da.tree.layers[0].name = "Alice".into();
        let ua = a.local_changes(&da).unwrap().unwrap();
        db.tree.layers[0].opacity = 0.5;
        db.tree.layers[1]
            .as_raster_mut()
            .unwrap()
            .tiles
            .get_mut_or_insert(TileCoord { tx: 0, ty: 0 }, Depth::Eight)
            .set(0, schist_color::Rgba::new(1.0, 0.0, 0.0, 1.0));
        let ub = b.local_changes(&db).unwrap().unwrap();
        a.apply(&ub).unwrap();
        b.apply(&ua).unwrap();
        for s in [&mut a, &mut b] {
            let d = s.render().unwrap();
            assert_eq!(d.tree.layers[0].name, "Alice");
            assert_eq!(d.tree.layers[0].opacity, 0.5);
            assert_eq!(
                d.tree.layers[1].as_raster().unwrap().tiles.pixel(0, 0).r,
                1.0
            );
        }
    }
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn native_tile_depths_roundtrip() {
        for depth in [Depth::Eight, Depth::Sixteen, Depth::ThirtyTwo] {
            let mut t = TileBuf::new(depth);
            t.set(5, schist_color::Rgba::new(0.2, 0.6, 0.1, 1.0));
            assert_eq!(t, read_tile(&tile_bytes(&t)).unwrap());
        }
    }
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn edits_during_join_merge_with_existing_room() {
        let mut original = sample();
        let mut server = SharedDocument::new(&original).unwrap();
        original.tree.layers[0].name = "Remote".into();
        server.local_changes(&original).unwrap();
        let mut local = sample();
        let mut client = SharedDocument::unseeded(&local).unwrap();
        local.tree.layers[1].opacity = 0.4;
        client.local_changes(&local).unwrap();
        client.apply(&server.full_state()).unwrap();
        client.seed_if_empty().unwrap();
        let result = client.render().unwrap();
        assert_eq!(result.tree.layers[0].name, "Remote");
        assert_eq!(result.tree.layers[1].opacity, 0.4);
    }
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn undo_only_removes_local_changes() {
        let mut local = sample();
        let mut a = SharedDocument::new(&local).unwrap();
        let mut b = SharedDocument::new(&local).unwrap();
        local.tree.layers[0].name = "Local".into();
        let update = a.local_changes(&local).unwrap().unwrap();
        b.apply(&update).unwrap();
        let mut other = b.render().unwrap();
        other.tree.layers[1].name = "Remote".into();
        let update = b.local_changes(&other).unwrap().unwrap();
        a.apply(&update).unwrap();
        assert!(a.undo(false));
        let result = a.render().unwrap();
        assert_eq!(result.tree.layers[0].name, "A");
        assert_eq!(result.tree.layers[1].name, "Remote");
    }
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn recovery_keeps_unjoined_edits_without_overwriting_peers() {
        let mut local = sample();
        let mut client = SharedDocument::unseeded(&local).unwrap();
        local.tree.layers[1].opacity = 0.3;
        client.local_changes(&local).unwrap();
        let checkpoint = client.checkpoint().unwrap();
        let original = sample();
        let mut restored = SharedDocument::unseeded(&original).unwrap();
        let preview = restored.restore(&checkpoint, &original).unwrap();
        assert_eq!(preview.tree.layers[1].opacity, 0.3);
        let mut server = SharedDocument::new(&original).unwrap();
        let mut peer = server.render().unwrap();
        peer.tree.layers[0].name = "Peer".into();
        server.local_changes(&peer).unwrap();
        restored.apply(&server.full_state()).unwrap();
        restored.seed_if_empty().unwrap();
        let result = restored.render().unwrap();
        assert_eq!(result.tree.layers[0].name, "Peer");
        assert_eq!(result.tree.layers[1].opacity, 0.3);
    }
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn layer_comp_references_survive_other_process_ids() {
        let mut local = sample();
        let mut comp = schist_core::LayerComp::new("Alternative");
        comp.states.push(schist_core::LayerCompState {
            layer: local.tree.layers[1].id,
            visible: false,
            opacity: 0.4,
            fill_opacity: 0.8,
            blend: BlendMode::Normal,
            style: Default::default(),
        });
        local.layer_comps.push(comp);
        let source = SharedDocument::new(&local).unwrap();
        let other = sample();
        assert_ne!(local.tree.layers[1].id, other.tree.layers[1].id);
        let mut peer = SharedDocument::unseeded(&other).unwrap();
        peer.apply(&source.full_state()).unwrap();
        peer.seed_if_empty().unwrap();
        let result = peer.render().unwrap();
        assert_eq!(
            result.layer_comps[0].states[0].layer,
            result.tree.layers[1].id
        );
        assert_eq!(result.layer_comps[0].states[0].opacity, 0.4);
        assert!(peer.local_changes(&result).unwrap().is_none());
    }
}
