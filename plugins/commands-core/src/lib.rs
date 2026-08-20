//! Core commands: edit (undo/redo/cut/copy/paste/fill), select
//! (all/deselect/inverse), and layer operations — with their default
//! Photoshop keybindings.

use photoslop_core::{
    blit_rgba8, Document, IntRect, Layer, LayerId, LayerKind, LayerPath, TileCoord, TILE_SIZE,
};
use photoslop_plugin_api::{
    ClipboardImage, Command, CommandCtx, CommandPlugin, PluginManifest, PluginRegistry,
};
use std::sync::Arc;

fn cmd(
    id: &'static str,
    title: &'static str,
    keybind: Option<&'static str>,
    run: impl Fn(&mut CommandCtx) + Send + 'static,
) -> Command {
    Command {
        id,
        title,
        keybind,
        run: Box::new(run),
    }
}

/// Path just above the active layer (or top of stack).
/// Select ▸ Grow and Select ▸ Similar.
///
/// Both extend the selection to pixels that resemble what is already
/// selected; Grow only reaches pixels touching the selection, Similar
/// sweeps the whole layer. Photoshop drives both from the magic wand's
/// tolerance, which is what `state.tolerance` carries.
fn grow_selection(ctx: &mut CommandCtx, contiguous: bool) {
    if ctx.doc.selection.is_empty() {
        return;
    }
    let canvas = ctx.doc.canvas_rect();
    let Some(layer) = ctx.doc.active_layer.and_then(|id| ctx.doc.tree.find(id)) else {
        return;
    };
    let LayerKind::Raster(raster) = &layer.kind else {
        return;
    };
    // Average colour of what is currently selected.
    let bounds = ctx.doc.selection.bounds().intersect(&canvas);
    let mut acc = [0f64; 3];
    let mut n = 0u64;
    for y in bounds.top..bounds.bottom {
        for x in bounds.left..bounds.right {
            if ctx.doc.selection.coverage(x, y) < 128 {
                continue;
            }
            let c = raster.tiles.pixel(x, y);
            acc[0] += c.r as f64;
            acc[1] += c.g as f64;
            acc[2] += c.b as f64;
            n += 1;
        }
    }
    if n == 0 {
        return;
    }
    let mean = [
        (acc[0] / n as f64) as f32,
        (acc[1] / n as f64) as f32,
        (acc[2] / n as f64) as f32,
    ];
    let tol = ctx.state.tolerance as f32 / 255.0;
    let alike = |x: i32, y: i32| {
        let c = raster.tiles.pixel(x, y);
        (c.r - mean[0])
            .abs()
            .max((c.g - mean[1]).abs())
            .max((c.b - mean[2]).abs())
            <= tol
    };

    let mut out: Vec<(i32, i32)> = Vec::new();
    if contiguous {
        // Flood outwards from the selection's own edge.
        let w = canvas.width() as usize;
        let mut seen = vec![false; w * canvas.height() as usize];
        let mut stack = Vec::new();
        for y in bounds.top..bounds.bottom {
            for x in bounds.left..bounds.right {
                if ctx.doc.selection.coverage(x, y) >= 128 {
                    seen[(y - canvas.top) as usize * w + (x - canvas.left) as usize] = true;
                    stack.push((x, y));
                }
            }
        }
        while let Some((cx, cy)) = stack.pop() {
            for (nx, ny) in [(cx - 1, cy), (cx + 1, cy), (cx, cy - 1), (cx, cy + 1)] {
                if !canvas.contains(nx, ny) {
                    continue;
                }
                let ix = (ny - canvas.top) as usize * w + (nx - canvas.left) as usize;
                if seen[ix] {
                    continue;
                }
                seen[ix] = true;
                if alike(nx, ny) {
                    out.push((nx, ny));
                    stack.push((nx, ny));
                }
            }
        }
    } else {
        for y in canvas.top..canvas.bottom {
            for x in canvas.left..canvas.right {
                if ctx.doc.selection.coverage(x, y) < 128 && alike(x, y) {
                    out.push((x, y));
                }
            }
        }
    }
    if out.is_empty() {
        return;
    }
    let name = if contiguous { "Grow" } else { "Similar" };
    let mut edit = ctx.doc.begin_edit(name);
    edit.change_selection(|sel, _| {
        for (x, y) in &out {
            let coord = TileCoord::containing(*x, *y);
            let buf = sel.mask.get_mut_or_insert(coord);
            let lx = x.rem_euclid(TILE_SIZE) as usize;
            let ly = y.rem_euclid(TILE_SIZE) as usize;
            buf[ly * TILE_SIZE as usize + lx] = 255;
        }
        sel.activate();
        sel.recompute_bounds();
    });
    edit.commit();
}

fn insert_path_above_active(doc: &Document) -> LayerPath {
    match doc.active_layer.and_then(|id| doc.tree.path_of(id)) {
        Some(mut path) => {
            *path.0.last_mut().unwrap() += 1;
            path
        }
        None => LayerPath(vec![doc.tree.layers.len()]),
    }
}

/// Deep-clone a layer with fresh ids (duplicate).
fn reid(layer: &mut Layer) {
    layer.id = LayerId::next();
    if let LayerKind::Group(g) = &mut layer.kind {
        for child in &mut g.children {
            reid(child);
        }
    }
}

/// Copy the active layer's pixels within the selection to a ClipboardImage.
fn copy_pixels(doc: &Document, merged: bool) -> Option<ClipboardImage> {
    let canvas = doc.canvas_rect();
    let bounds = if doc.selection.is_empty() {
        canvas
    } else {
        doc.selection.bounds().intersect(&canvas)
    };
    if bounds.is_empty() {
        return None;
    }
    let w = bounds.width() as usize;
    let h = bounds.height() as usize;
    let mut rgba = if merged {
        photoslop_compositor::composite_region_rgba8(doc, bounds)
    } else {
        let layer = doc.active_layer.and_then(|id| doc.tree.find(id))?;
        let LayerKind::Raster(raster) = &layer.kind else {
            return None;
        };
        let mut buf = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let px = raster
                    .tiles
                    .pixel(bounds.left + x as i32, bounds.top + y as i32)
                    .to_u8();
                buf[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&px);
            }
        }
        buf
    };
    // Apply selection coverage to alpha.
    if !doc.selection.is_empty() {
        for y in 0..h {
            for x in 0..w {
                let c = doc
                    .selection
                    .coverage(bounds.left + x as i32, bounds.top + y as i32);
                let a = &mut rgba[(y * w + x) * 4 + 3];
                *a = ((*a as u16 * c as u16) / 255) as u8;
            }
        }
    }
    Some(ClipboardImage { rect: bounds, rgba })
}

/// Clear the selected region of the active layer (used by Cut).
fn clear_selection(ctx: &mut CommandCtx) {
    let Some(id) = ctx.doc.active_layer else {
        return;
    };
    let canvas = ctx.doc.canvas_rect();
    let bounds = if ctx.doc.selection.is_empty() {
        canvas
    } else {
        ctx.doc.selection.bounds().intersect(&canvas)
    };
    if bounds.is_empty() {
        return;
    }
    let selection = ctx.doc.selection.clone();
    let mut edit = ctx.doc.begin_edit("Clear");
    for coord in TileCoord::covering(&bounds) {
        let trect = coord.rect();
        let clip = trect.intersect(&bounds);
        let Some(tile) = edit.writable_tile(id, coord) else {
            break;
        };
        for y in clip.top..clip.bottom {
            for x in clip.left..clip.right {
                let c = selection.coverage(x, y) as f32 / 255.0;
                if c <= 0.0 {
                    continue;
                }
                let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                let mut px = tile.get(ix);
                px.a *= 1.0 - c;
                tile.set(ix, px);
            }
        }
    }
    edit.commit();
}

fn merge_down(ctx: &mut CommandCtx) {
    let Some(id) = ctx.doc.active_layer else {
        return;
    };
    let Some(path) = ctx.doc.tree.path_of(id) else {
        return;
    };
    let ix = *path.0.last().unwrap();
    if ix == 0 {
        return; // nothing below
    }
    let mut below_path = path.clone();
    *below_path.0.last_mut().unwrap() = ix - 1;

    // Composite the pair in a scratch document (COW makes the clones cheap)
    // so masks/blend/opacity semantics come from the one true compositor.
    let (upper, lower) = {
        let upper = ctx.doc.tree.find(id).unwrap();
        let mut lower_probe = path.clone();
        *lower_probe.0.last_mut().unwrap() = ix - 1;
        // Fetch by walking: siblings share a parent, so remove is avoided.
        let parent_children: &[Layer] = {
            let mut layers: &[Layer] = &ctx.doc.tree.layers;
            for &i in &path.0[..path.0.len() - 1] {
                layers = layers[i].children().unwrap();
            }
            layers
        };
        (upper.clone(), parent_children[ix - 1].clone())
    };
    if !matches!(lower.kind, LayerKind::Raster(_)) {
        return; // merging into groups/adjustments lands with M7 flatten work
    }
    let mut scratch = Document::new("merge", ctx.doc.width, ctx.doc.height, ctx.doc.depth);
    let bounds = upper.content_bounds().union(&lower.content_bounds());
    scratch.tree.layers = vec![lower, upper];
    let bounds = bounds.intersect(&scratch.canvas_rect());
    if bounds.is_empty() {
        return;
    }
    let rgba = photoslop_compositor::composite_region_rgba8(&scratch, bounds);

    let mut merged = Layer::new_raster(ctx.doc.tree.find(id).unwrap().name.clone());
    blit_rgba8(
        &mut merged.as_raster_mut().unwrap().tiles,
        ctx.doc.depth,
        bounds,
        &rgba,
    );
    let merged_id = merged.id;

    let below_id = {
        let mut layers: &[Layer] = &ctx.doc.tree.layers;
        for &i in &path.0[..path.0.len() - 1] {
            layers = layers[i].children().unwrap();
        }
        layers[ix - 1].id
    };
    let mut edit = ctx.doc.begin_edit("Merge Down");
    edit.remove_layer(id);
    edit.remove_layer(below_id);
    edit.insert_layer(below_path, merged);
    edit.commit();
    ctx.doc.active_layer = Some(merged_id);
}

fn merge_visible(ctx: &mut CommandCtx) {
    let canvas = ctx.doc.canvas_rect();
    let rgba = photoslop_compositor::composite_region_rgba8(ctx.doc, canvas);
    let mut merged = Layer::new_raster("Merged");
    blit_rgba8(
        &mut merged.as_raster_mut().unwrap().tiles,
        ctx.doc.depth,
        canvas,
        &rgba,
    );
    let merged_id = merged.id;

    let visible_ids: Vec<LayerId> = ctx
        .doc
        .tree
        .layers
        .iter()
        .filter(|l| l.visible)
        .map(|l| l.id)
        .collect();
    let mut edit = ctx.doc.begin_edit("Merge Visible");
    for vid in visible_ids {
        edit.remove_layer(vid);
    }
    let top = LayerPath(vec![edit.doc().tree.layers.len()]);
    edit.insert_layer(top, merged);
    edit.commit();
    ctx.doc.active_layer = Some(merged_id);
}

fn paste(ctx: &mut CommandCtx, in_place: bool) {
    let Some(clip) = ctx.state.clipboard.clone() else {
        return;
    };
    let rect = if in_place {
        clip.rect
    } else {
        // Centered paste, like Photoshop with no selection.
        let cw = ctx.doc.width as i32;
        let ch = ctx.doc.height as i32;
        IntRect::from_xywh(
            (cw - clip.rect.width()) / 2,
            (ch - clip.rect.height()) / 2,
            clip.rect.width() as u32,
            clip.rect.height() as u32,
        )
    };
    let mut layer = Layer::new_raster("Pasted Layer");
    blit_rgba8(
        &mut layer.as_raster_mut().unwrap().tiles,
        ctx.doc.depth,
        rect,
        &clip.rgba,
    );
    let id = layer.id;
    let path = insert_path_above_active(ctx.doc);
    let mut edit = ctx.doc.begin_edit("Paste");
    edit.insert_layer(path, layer);
    edit.commit();
    ctx.doc.active_layer = Some(id);
}

fn fill_selection(ctx: &mut CommandCtx, background: bool) {
    let Some(id) = ctx.doc.active_layer else {
        return;
    };
    let color = if background {
        ctx.state.background
    } else {
        ctx.state.foreground
    };
    let canvas = ctx.doc.canvas_rect();
    let bounds = if ctx.doc.selection.is_empty() {
        canvas
    } else {
        ctx.doc.selection.bounds().intersect(&canvas)
    };
    if bounds.is_empty() {
        return;
    }
    let selection = ctx.doc.selection.clone();
    let mut edit = ctx.doc.begin_edit("Fill");
    for coord in TileCoord::covering(&bounds) {
        let trect = coord.rect();
        let clip = trect.intersect(&bounds);
        let Some(tile) = edit.writable_tile(id, coord) else {
            break;
        };
        for y in clip.top..clip.bottom {
            for x in clip.left..clip.right {
                let c = selection.coverage(x, y) as f32 / 255.0;
                if c <= 0.0 {
                    continue;
                }
                let ix = ((y - trect.top) * TILE_SIZE + (x - trect.left)) as usize;
                let mut src = color;
                src.a *= c;
                tile.set(ix, src.over(tile.get(ix)));
            }
        }
    }
    edit.commit();
}

pub struct CoreCommandsPlugin;

impl CommandPlugin for CoreCommandsPlugin {
    fn commands(&self) -> Vec<Command> {
        vec![
            // --- Edit ---
            cmd("edit.undo", "Undo", Some("cmd-z"), |ctx| {
                ctx.doc.undo();
            }),
            cmd("edit.redo", "Redo", Some("cmd-shift-z"), |ctx| {
                ctx.doc.redo();
            }),
            cmd("edit.copy", "Copy", Some("cmd-c"), |ctx| {
                if let Some(clip) = copy_pixels(ctx.doc, false) {
                    ctx.state.clipboard = Some(Arc::new(clip));
                }
            }),
            cmd(
                "edit.copy_merged",
                "Copy Merged",
                Some("cmd-shift-c"),
                |ctx| {
                    if let Some(clip) = copy_pixels(ctx.doc, true) {
                        ctx.state.clipboard = Some(Arc::new(clip));
                    }
                },
            ),
            cmd("edit.cut", "Cut", Some("cmd-x"), |ctx| {
                if let Some(clip) = copy_pixels(ctx.doc, false) {
                    ctx.state.clipboard = Some(Arc::new(clip));
                    clear_selection(ctx);
                }
            }),
            cmd("edit.paste", "Paste", Some("cmd-v"), |ctx| {
                paste(ctx, false)
            }),
            cmd(
                "edit.paste_in_place",
                "Paste in Place",
                Some("cmd-shift-v"),
                |ctx| paste(ctx, true),
            ),
            cmd(
                "edit.fill_foreground",
                "Fill with Foreground",
                Some("alt-backspace"),
                |ctx| fill_selection(ctx, false),
            ),
            cmd(
                "edit.fill_background",
                "Fill with Background",
                Some("cmd-backspace"),
                |ctx| fill_selection(ctx, true),
            ),
            // --- Select ---
            cmd("select.all", "Select All", Some("cmd-a"), |ctx| {
                let mut edit = ctx.doc.begin_edit("Select All");
                edit.change_selection(|sel, canvas| sel.select_all(canvas));
                edit.commit();
            }),
            cmd("select.deselect", "Deselect", Some("cmd-d"), |ctx| {
                let mut edit = ctx.doc.begin_edit("Deselect");
                edit.change_selection(|sel, _| sel.deselect());
                edit.commit();
            }),
            cmd(
                "select.inverse",
                "Select Inverse",
                Some("cmd-shift-i"),
                |ctx| {
                    let mut edit = ctx.doc.begin_edit("Select Inverse");
                    edit.change_selection(|sel, canvas| sel.invert(canvas));
                    edit.commit();
                },
            ),
            // --- Layer ---
            cmd("layer.new", "New Layer", Some("cmd-shift-n"), |ctx| {
                let path = insert_path_above_active(ctx.doc);
                let n = ctx.doc.tree.len() + 1;
                let mut layer = Layer::new_raster(format!("Layer {n}"));
                layer.name = format!("Layer {n}");
                let id = layer.id;
                let mut edit = ctx.doc.begin_edit("New Layer");
                edit.insert_layer(path, layer);
                edit.commit();
                ctx.doc.active_layer = Some(id);
            }),
            cmd("layer.duplicate", "Duplicate Layer", Some("cmd-j"), |ctx| {
                let Some(id) = ctx.doc.active_layer else {
                    return;
                };
                let Some(src) = ctx.doc.tree.find(id) else {
                    return;
                };
                let mut copy = src.clone();
                copy.name = format!("{} copy", copy.name);
                reid(&mut copy);
                let new_id = copy.id;
                let path = insert_path_above_active(ctx.doc);
                let mut edit = ctx.doc.begin_edit("Duplicate Layer");
                edit.insert_layer(path, copy);
                edit.commit();
                ctx.doc.active_layer = Some(new_id);
            }),
            cmd("layer.delete", "Delete Layer", None, |ctx| {
                let Some(id) = ctx.doc.active_layer else {
                    return;
                };
                let mut edit = ctx.doc.begin_edit("Delete Layer");
                edit.remove_layer(id);
                edit.commit();
            }),
            cmd("layer.group", "Group Layer", Some("cmd-g"), |ctx| {
                // Wraps the active layer in a group (multi-select lands
                // with the layers-panel selection model).
                let Some(id) = ctx.doc.active_layer else {
                    return;
                };
                let Some(path) = ctx.doc.tree.path_of(id) else {
                    return;
                };
                let mut group = Layer::new_group("Group");
                let group_id = group.id;
                let mut edit = ctx.doc.begin_edit("Group Layer");
                // Remove the layer, put it inside the group, insert group.
                let removed = ctx_remove(&mut edit, id);
                if let Some(layer) = removed {
                    if let LayerKind::Group(g) = &mut group.kind {
                        g.children.push(layer);
                    }
                    edit.insert_layer(path, group);
                }
                edit.commit();
                ctx.doc.active_layer = Some(group_id);
            }),
            cmd("select.reselect", "Reselect", Some("cmd-shift-d"), |ctx| {
                let Some(previous) = ctx.doc.last_selection.clone() else {
                    return;
                };
                let mut edit = ctx.doc.begin_edit("Reselect");
                edit.change_selection(|sel, _| *sel = previous);
                edit.commit();
            }),
            cmd("select.feather", "Feather Selection", None, |ctx| {
                let mut edit = ctx.doc.begin_edit("Feather");
                edit.change_selection(|sel, _| sel.feather(2.0));
                edit.commit();
            }),
            cmd("select.grow", "Grow", None, |ctx| {
                grow_selection(ctx, true);
            }),
            cmd("select.similar", "Similar", None, |ctx| {
                grow_selection(ctx, false);
            }),
            cmd("select.save", "Save Selection", None, |ctx| {
                if ctx.doc.selection.is_empty() {
                    return;
                }
                let n = ctx.doc.saved_selections.len() + 1;
                let sel = ctx.doc.selection.clone();
                ctx.doc.saved_selections.push((format!("Alpha {n}"), sel));
            }),
            cmd("select.load", "Load Selection", None, |ctx| {
                // Loads the most recently saved one; the dialog picks by
                // name once there is a channels panel to name them in.
                let Some((_, sel)) = ctx.doc.saved_selections.last().cloned() else {
                    return;
                };
                let mut edit = ctx.doc.begin_edit("Load Selection");
                edit.change_selection(|s, _| *s = sel);
                edit.commit();
            }),
            // --- Layer ordering ---
            cmd("layer.raise", "Bring Forward", Some("cmd-]"), |ctx| {
                move_layer_by(ctx, 1);
            }),
            cmd("layer.lower", "Send Backward", Some("cmd-["), |ctx| {
                move_layer_by(ctx, -1);
            }),
            cmd(
                "layer.to_front",
                "Bring to Front",
                Some("cmd-shift-]"),
                |ctx| {
                    move_layer_to_end(ctx, true);
                },
            ),
            cmd(
                "layer.to_back",
                "Send to Back",
                Some("cmd-shift-["),
                |ctx| {
                    move_layer_to_end(ctx, false);
                },
            ),
            cmd(
                "layer.clipping_mask",
                "Create/Release Clipping Mask",
                Some("cmd-alt-g"),
                |ctx| {
                    let Some(id) = ctx.doc.active_layer else {
                        return;
                    };
                    // A clipping mask needs something to clip to.
                    let Some(path) = ctx.doc.tree.path_of(id) else {
                        return;
                    };
                    if *path.0.last().unwrap() == 0 {
                        return;
                    }
                    let mut edit = ctx.doc.begin_edit("Clipping Mask");
                    edit.change_props(id, |l| l.clipping = !l.clipping);
                    edit.commit();
                },
            ),
            cmd(
                "layer.cut_to_new",
                "Layer via Cut",
                Some("cmd-shift-j"),
                |ctx| {
                    let Some(clip) = copy_pixels(ctx.doc, false) else {
                        return;
                    };
                    clear_selection(ctx);
                    let mut layer = Layer::new_raster("Layer via Cut");
                    blit_rgba8(
                        &mut layer.as_raster_mut().unwrap().tiles,
                        ctx.doc.depth,
                        clip.rect,
                        &clip.rgba,
                    );
                    let id = layer.id;
                    let path = insert_path_above_active(ctx.doc);
                    let mut edit = ctx.doc.begin_edit("Layer via Cut");
                    edit.insert_layer(path, layer);
                    edit.commit();
                    ctx.doc.active_layer = Some(id);
                },
            ),
            cmd("layer.add_mask", "Add Layer Mask", None, |ctx| {
                let Some(id) = ctx.doc.active_layer else {
                    return;
                };
                if ctx.doc.tree.find(id).map(|l| l.mask.is_some()) != Some(false) {
                    return;
                }
                // A mask made from a selection reveals only the selection.
                let selection = ctx.doc.selection.clone();
                let canvas = ctx.doc.canvas_rect();
                let mut mask = photoslop_core::LayerMask::new_revealing();
                if !selection.is_empty() {
                    mask.default_value = 0;
                    mask.bounds = canvas;
                    for coord in TileCoord::covering(&canvas) {
                        let rect = coord.rect();
                        let buf = mask.tiles.get_mut_or_insert(coord);
                        for y in rect.top..rect.bottom {
                            for x in rect.left..rect.right {
                                let ix = ((y - rect.top) * TILE_SIZE + (x - rect.left)) as usize;
                                buf[ix] = selection.coverage(x, y);
                            }
                        }
                    }
                }
                let mut edit = ctx.doc.begin_edit("Add Layer Mask");
                edit.set_mask(id, Some(mask));
                edit.commit();
            }),
            cmd("layer.flatten", "Flatten Image", None, |ctx| {
                merge_visible(ctx);
            }),
            cmd("layer.merge_down", "Merge Down", Some("cmd-e"), merge_down),
            cmd(
                "layer.merge_visible",
                "Merge Visible",
                Some("cmd-shift-e"),
                merge_visible,
            ),
        ]
    }
}

/// Move the active layer up (+1) or down (-1) among its siblings.
fn move_layer_by(ctx: &mut CommandCtx, delta: i32) {
    let Some(id) = ctx.doc.active_layer else {
        return;
    };
    let Some(path) = ctx.doc.tree.path_of(id) else {
        return;
    };
    let index = *path.0.last().unwrap() as i32;
    let target = index + delta;
    if target < 0 {
        return;
    }
    let mut to = path.clone();
    *to.0.last_mut().unwrap() = target as usize;
    let mut edit = ctx.doc.begin_edit(if delta > 0 {
        "Bring Forward"
    } else {
        "Send Backward"
    });
    edit.move_layer(path, to);
    edit.commit();
}

/// Move the active layer to the top or bottom of its group.
fn move_layer_to_end(ctx: &mut CommandCtx, to_front: bool) {
    let Some(id) = ctx.doc.active_layer else {
        return;
    };
    let Some(path) = ctx.doc.tree.path_of(id) else {
        return;
    };
    // Siblings are the layers sharing this path prefix.
    let sibling_count = {
        let mut layers: &[Layer] = &ctx.doc.tree.layers;
        for &i in &path.0[..path.0.len() - 1] {
            match layers.get(i).and_then(|l| l.children()) {
                Some(children) => layers = children,
                None => return,
            }
        }
        layers.len()
    };
    let mut to = path.clone();
    *to.0.last_mut().unwrap() = if to_front {
        sibling_count.saturating_sub(1)
    } else {
        0
    };
    if to == path {
        return;
    }
    let mut edit = ctx.doc.begin_edit(if to_front {
        "Bring to Front"
    } else {
        "Send to Back"
    });
    edit.move_layer(path, to);
    edit.commit();
}

/// Remove a layer through the builder but get the removed value back
/// (EditBuilder::remove_layer records the op; we reconstruct the layer from
/// the document *before* removal).
fn ctx_remove(edit: &mut photoslop_core::EditBuilder<'_>, id: LayerId) -> Option<Layer> {
    let layer = edit.doc().tree.find(id)?.clone();
    if edit.remove_layer(id) {
        let mut l = layer;
        // Fresh id inside the group so undo (which restores the original
        // remove) can't collide.
        l.id = LayerId::next();
        Some(l)
    } else {
        None
    }
}

impl PluginManifest for CoreCommandsPlugin {
    fn id(&self) -> &'static str {
        "photoslop.commands-core"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_commands(self);
    }
}

#[allow(unused_imports)]
use anyhow as _;
#[allow(unused_imports)]
use photoslop_color as _;

#[cfg(test)]
mod tests {
    use super::*;
    use photoslop_color::{Depth, Rgba};
    use photoslop_core::SelectOp;
    use photoslop_plugin_api::EditorState;

    fn registry() -> PluginRegistry {
        let mut reg = PluginRegistry::new();
        CoreCommandsPlugin.register(&mut reg);
        reg
    }

    fn run(reg: &PluginRegistry, id: &str, doc: &mut Document, state: &mut EditorState) {
        let mut ctx = CommandCtx { doc, state };
        (reg.command(id).expect(id).run)(&mut ctx);
    }

    fn doc_with_pixels() -> Document {
        let mut doc = Document::new("t", 100, 100, Depth::Eight);
        let mut layer = Layer::new_raster("bg");
        let buf = [10u8, 20, 30, 255].repeat(100 * 100);
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(100, 100),
            &buf,
        );
        doc.push_layer(layer);
        doc
    }

    #[test]
    fn select_all_inverse_deselect() {
        let reg = registry();
        let mut doc = doc_with_pixels();
        let mut state = EditorState::default();
        run(&reg, "select.all", &mut doc, &mut state);
        assert_eq!(doc.selection.coverage(50, 50), 255);
        assert!(!doc.selection.is_empty());
        run(&reg, "select.inverse", &mut doc, &mut state);
        assert_eq!(doc.selection.coverage(50, 50), 0);
        run(&reg, "select.deselect", &mut doc, &mut state);
        assert!(doc.selection.is_empty());
    }

    #[test]
    fn new_duplicate_delete_layer() {
        let reg = registry();
        let mut doc = doc_with_pixels();
        let mut state = EditorState::default();
        run(&reg, "layer.new", &mut doc, &mut state);
        assert_eq!(doc.tree.layers.len(), 2);
        run(&reg, "layer.duplicate", &mut doc, &mut state);
        assert_eq!(doc.tree.layers.len(), 3);
        run(&reg, "layer.delete", &mut doc, &mut state);
        assert_eq!(doc.tree.layers.len(), 2);
        run(&reg, "edit.undo", &mut doc, &mut state);
        assert_eq!(doc.tree.layers.len(), 3, "undo restores deleted layer");
    }

    #[test]
    fn copy_paste_round_trip() {
        let reg = registry();
        let mut doc = doc_with_pixels();
        let mut state = EditorState::default();
        doc.selection
            .select_rect(IntRect::from_xywh(10, 10, 20, 20), SelectOp::Replace);
        run(&reg, "edit.copy", &mut doc, &mut state);
        let clip = state.clipboard.as_ref().expect("clipboard filled");
        assert_eq!(clip.rect.width(), 20);
        assert_eq!(&clip.rgba[0..4], &[10, 20, 30, 255]);

        run(&reg, "edit.paste_in_place", &mut doc, &mut state);
        assert_eq!(doc.tree.layers.len(), 2);
        let pasted = doc.tree.layers.last().unwrap();
        assert_eq!(pasted.name, "Pasted Layer");
        assert_eq!(
            pasted.as_raster().unwrap().tiles.pixel(15, 15).to_u8(),
            [10, 20, 30, 255]
        );
    }

    #[test]
    fn cut_clears_selection_region() {
        let reg = registry();
        let mut doc = doc_with_pixels();
        let mut state = EditorState::default();
        doc.selection
            .select_rect(IntRect::from_xywh(10, 10, 20, 20), SelectOp::Replace);
        run(&reg, "edit.cut", &mut doc, &mut state);
        let layer = doc.tree.layers.first().unwrap();
        assert_eq!(
            layer.as_raster().unwrap().tiles.pixel(15, 15).to_u8()[3],
            0,
            "cut area cleared"
        );
        assert_eq!(
            layer.as_raster().unwrap().tiles.pixel(50, 50).to_u8(),
            [10, 20, 30, 255],
            "outside intact"
        );
    }

    #[test]
    fn fill_respects_selection() {
        let reg = registry();
        let mut doc = doc_with_pixels();
        let mut state = EditorState {
            foreground: Rgba::new(1.0, 0.0, 0.0, 1.0),
            ..Default::default()
        };
        doc.selection
            .select_rect(IntRect::from_xywh(0, 0, 10, 10), SelectOp::Replace);
        run(&reg, "edit.fill_foreground", &mut doc, &mut state);
        let layer = doc.tree.layers.first().unwrap();
        assert_eq!(
            layer.as_raster().unwrap().tiles.pixel(5, 5).to_u8(),
            [255, 0, 0, 255]
        );
        assert_eq!(
            layer.as_raster().unwrap().tiles.pixel(50, 50).to_u8(),
            [10, 20, 30, 255]
        );
    }

    #[test]
    fn merge_down_composites_pair() {
        let reg = registry();
        let mut doc = doc_with_pixels();
        let mut state = EditorState::default();
        // Add a half-transparent red layer on top.
        let mut top = Layer::new_raster("red");
        let buf = [255u8, 0, 0, 128].repeat(100 * 100);
        blit_rgba8(
            &mut top.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(100, 100),
            &buf,
        );
        doc.push_layer(top);
        assert_eq!(doc.tree.layers.len(), 2);

        run(&reg, "layer.merge_down", &mut doc, &mut state);
        assert_eq!(doc.tree.layers.len(), 1, "two became one");
        let px = doc.tree.layers[0]
            .as_raster()
            .unwrap()
            .tiles
            .pixel(50, 50)
            .to_u8();
        assert!(px[0] > 120 && px[0] < 150, "blended red: {px:?}");
        run(&reg, "edit.undo", &mut doc, &mut state);
        assert_eq!(doc.tree.layers.len(), 2, "merge undoes");
    }

    #[test]
    fn group_wraps_active_layer() {
        let reg = registry();
        let mut doc = doc_with_pixels();
        let mut state = EditorState::default();
        run(&reg, "layer.group", &mut doc, &mut state);
        assert_eq!(doc.tree.layers.len(), 1);
        let group = &doc.tree.layers[0];
        assert!(group.is_group());
        assert_eq!(group.children().unwrap().len(), 1);
        assert_eq!(group.children().unwrap()[0].name, "bg");
    }
}

#[cfg(test)]
mod m11_tests {
    use super::*;
    use photoslop_color::Depth;
    use photoslop_plugin_api::EditorState;

    fn registry() -> PluginRegistry {
        let mut reg = PluginRegistry::new();
        CoreCommandsPlugin.register(&mut reg);
        reg
    }

    fn run(reg: &PluginRegistry, id: &str, doc: &mut Document, state: &mut EditorState) {
        let mut ctx = CommandCtx { doc, state };
        (reg.command(id)
            .unwrap_or_else(|| panic!("missing {id}"))
            .run)(&mut ctx);
    }

    fn doc_with_layers(n: usize) -> Document {
        let mut doc = Document::new("t", 64, 64, Depth::Eight);
        for i in 0..n {
            let mut layer = Layer::new_raster(format!("L{i}"));
            let buf = [(i * 40) as u8, 0, 0, 255].repeat(16 * 16);
            blit_rgba8(
                &mut layer.as_raster_mut().unwrap().tiles,
                Depth::Eight,
                IntRect::from_xywh(0, 0, 16, 16),
                &buf,
            );
            doc.push_layer(layer);
        }
        doc
    }

    fn names(doc: &Document) -> Vec<String> {
        doc.tree.layers.iter().map(|l| l.name.clone()).collect()
    }

    #[test]
    fn layer_order_commands_move_the_active_layer() {
        let reg = registry();
        let mut doc = doc_with_layers(3);
        let mut state = EditorState::default();
        doc.active_layer = Some(doc.tree.layers[0].id); // bottom

        run(&reg, "layer.raise", &mut doc, &mut state);
        assert_eq!(names(&doc), vec!["L1", "L0", "L2"]);

        run(&reg, "layer.to_front", &mut doc, &mut state);
        assert_eq!(names(&doc), vec!["L1", "L2", "L0"]);

        run(&reg, "layer.to_back", &mut doc, &mut state);
        assert_eq!(names(&doc), vec!["L0", "L1", "L2"]);

        run(&reg, "edit.undo", &mut doc, &mut state);
        assert_eq!(names(&doc), vec!["L1", "L2", "L0"], "ordering undoes");
    }

    #[test]
    fn lowering_the_bottom_layer_is_a_no_op() {
        let reg = registry();
        let mut doc = doc_with_layers(2);
        let mut state = EditorState::default();
        doc.active_layer = Some(doc.tree.layers[0].id);
        run(&reg, "layer.lower", &mut doc, &mut state);
        assert_eq!(names(&doc), vec!["L0", "L1"]);
        assert!(!doc.history.can_undo(), "nothing recorded");
    }

    #[test]
    fn clipping_mask_toggles_and_needs_a_layer_below() {
        let reg = registry();
        let mut doc = doc_with_layers(2);
        let mut state = EditorState::default();
        doc.active_layer = Some(doc.tree.layers[1].id);

        run(&reg, "layer.clipping_mask", &mut doc, &mut state);
        assert!(doc.tree.layers[1].clipping);
        run(&reg, "layer.clipping_mask", &mut doc, &mut state);
        assert!(!doc.tree.layers[1].clipping, "toggles back off");

        // The bottom layer has nothing to clip to.
        doc.active_layer = Some(doc.tree.layers[0].id);
        run(&reg, "layer.clipping_mask", &mut doc, &mut state);
        assert!(!doc.tree.layers[0].clipping);
    }

    #[test]
    fn reselect_restores_the_previous_selection() {
        let reg = registry();
        let mut doc = doc_with_layers(1);
        let mut state = EditorState::default();
        run(&reg, "select.all", &mut doc, &mut state);
        run(&reg, "select.deselect", &mut doc, &mut state);
        assert!(doc.selection.is_empty());
        run(&reg, "select.reselect", &mut doc, &mut state);
        assert!(!doc.selection.is_empty(), "selection came back");
        assert_eq!(doc.selection.coverage(10, 10), 255);
    }

    #[test]
    fn layer_via_cut_moves_pixels_to_a_new_layer() {
        let reg = registry();
        let mut doc = doc_with_layers(1);
        let mut state = EditorState::default();
        doc.selection.select_rect(
            IntRect::from_xywh(0, 0, 8, 8),
            photoslop_core::SelectOp::Replace,
        );
        run(&reg, "layer.cut_to_new", &mut doc, &mut state);

        assert_eq!(doc.tree.layers.len(), 2);
        let source = doc.tree.layers[0].as_raster().unwrap();
        assert_eq!(
            source.tiles.pixel(2, 2).to_u8()[3],
            0,
            "cut from the source"
        );
        let cut = doc.tree.layers[1].as_raster().unwrap();
        assert!(
            cut.tiles.pixel(2, 2).to_u8()[3] > 0,
            "landed on the new layer"
        );
    }

    #[test]
    fn add_layer_mask_uses_the_selection() {
        let reg = registry();
        let mut doc = doc_with_layers(1);
        let mut state = EditorState::default();
        doc.selection.select_rect(
            IntRect::from_xywh(0, 0, 8, 64),
            photoslop_core::SelectOp::Replace,
        );
        run(&reg, "layer.add_mask", &mut doc, &mut state);

        let mask = doc.tree.layers[0].mask.as_ref().expect("mask added");
        assert_eq!(mask.value(2, 2), 255, "selected area revealed");
        assert_eq!(mask.value(20, 2), 0, "unselected area hidden");
        doc.undo();
        assert!(doc.tree.layers[0].mask.is_none(), "undo removes it");
    }

    #[test]
    fn every_command_has_a_unique_id_and_title() {
        let reg = registry();
        let mut ids: Vec<&str> = reg.commands().iter().map(|c| c.id).collect();
        let count = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate command ids");
        assert!(reg.commands().iter().all(|c| !c.title.is_empty()));
    }

    #[test]
    fn keybindings_do_not_collide() {
        let reg = registry();
        let mut binds: Vec<&str> = reg.commands().iter().filter_map(|c| c.keybind).collect();
        let count = binds.len();
        binds.sort();
        binds.dedup();
        assert_eq!(binds.len(), count, "two commands share a keybinding");
    }
}
