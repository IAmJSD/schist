//! Cloud gallery controls share the local gallery's sidebar and drag targets.
use super::*;
use crate::ui;
use gpui::{img, AppContext as _, StatefulInteractiveElement as _, StyledImage as _};
use schist_cloud::{
    protocol::{map, value},
    Scope, Value,
};

#[derive(Clone)]
struct RemoteDrag {
    items: Vec<Value>,
    label: String,
}
pub(crate) struct DragLabel(pub String);
#[derive(Clone)]
pub(crate) struct LocalFolderDrag {
    pub path: PathBuf,
}
impl Render for DragLabel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .bg(gpui::rgb(ui::palette().accent))
            .text_color(gpui::rgb(0xffffff))
            .child(self.0.clone())
    }
}
fn caption(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(11.0))
        .text_color(gpui::rgb(ui::palette().text_dim))
        .child(text.into())
}
fn form(
    ws: &mut Workspace,
    kind: &'static str,
    fields: Vec<(&'static str, String, String)>,
    cx: &mut Context<Workspace>,
) {
    ws.open_modal(Modal::Cloud { kind, fields }, cx);
}
fn field(
    key: &'static str,
    label: &str,
    value: impl Into<String>,
) -> (&'static str, String, String) {
    (key, label.into(), value.into())
}
pub(crate) fn filter_fields(
    query: &schist_cloud::AssetQuery,
) -> Vec<(&'static str, String, String)> {
    let f = &query.filters;
    vec![
        field("cloud-query", "Search", query.text.clone()),
        field(
            "cloud-types",
            "File types (MIME, comma separated)",
            f.mime_types
                .as_ref()
                .map(|v| v.join(", "))
                .unwrap_or_default(),
        ),
        field(
            "cloud-tags",
            "Tags (comma separated)",
            f.tags.as_ref().map(|v| v.join(", ")).unwrap_or_default(),
        ),
        field(
            "cloud-edited",
            "Edited: any / yes / no",
            f.edited
                .map(|v| if v { "yes" } else { "no" })
                .unwrap_or("any"),
        ),
        field(
            "cloud-content",
            "Content: all / safe / flagged",
            f.content.as_deref().unwrap_or("all"),
        ),
        field(
            "cloud-rating",
            "Minimum rating (0–5)",
            f.min_rating.map(|v| v.to_string()).unwrap_or_default(),
        ),
        field(
            "cloud-after",
            "Captured after (YYYY-MM-DD)",
            f.captured_after
                .map(schist_cloud::format_date)
                .unwrap_or_default(),
        ),
        field(
            "cloud-before",
            "Captured before (YYYY-MM-DD)",
            f.captured_before
                .map(schist_cloud::format_date)
                .unwrap_or_default(),
        ),
        field(
            "cloud-bounds",
            "Map boundary: south, west, north, east",
            f.bounds
                .as_ref()
                .map(|b| format!("{}, {}, {}, {}", b.south, b.west, b.north, b.east))
                .unwrap_or_default(),
        ),
    ]
}
pub(crate) fn sidebar(ws: &mut Workspace, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    let mut root = div()
        .flex()
        .flex_col()
        .gap_1()
        .pt_3()
        .child(caption("SCHIST CLOUD"));
    if ws.cloud.account.is_none() {
        return root
            .child(ui::button(
                "Sign into Schist Cloud…",
                false,
                |ws, _, cx| ws.cloud_sign_in(cx),
                cx,
            ))
            .into_any_element();
    }
    root = root
        .child(caption(ws.cloud.account.as_ref().unwrap().domain.clone()))
        .child(ui::button(
            "All cloud photos",
            false,
            |ws, _, cx| ws.cloud_browse(Scope::Library, cx),
            cx,
        ))
        .child(ui::button(
            "Find folders / buckets…",
            false,
            |ws, _, cx| {
                form(
                    ws,
                    "catalogue",
                    vec![field(
                        "cloud-query",
                        "Name contains",
                        ws.cloud.catalogue.clone(),
                    )],
                    cx,
                )
            },
            cx,
        ))
        .child(ui::button(
            "+ Cloud folder…",
            false,
            |ws, _, cx| {
                form(
                    ws,
                    "new-folder",
                    vec![field("cloud-name", "Folder name", "")],
                    cx,
                )
            },
            cx,
        ));
    for (i, folder) in ws.cloud.folders.clone().into_iter().enumerate() {
        let id = folder.id.clone();
        let drop_id = id.clone();
        let rename = folder.clone();
        let delete = folder.clone();
        let drag = RemoteDrag {
            items: vec![map([
                ("kind", "folder".into()),
                ("id", id.clone().into()),
                ("recursive", true.into()),
            ])],
            label: folder.name.clone(),
        };
        root = root.child(
            div()
                .id(("cloud-folder", i))
                .flex()
                .flex_col()
                .child(
                    div()
                        .id(("cloud-folder-name", i))
                        .px_2()
                        .py_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(gpui::rgb(ui::palette().field_bg)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _, _, cx| {
                                ws.cloud_browse(
                                    Scope::Folder {
                                        id: id.clone(),
                                        recursive: true,
                                    },
                                    cx,
                                )
                            }),
                        )
                        .on_drag(drag, |drag, _, _, cx| {
                            cx.new(|_| DragLabel(drag.label.clone()))
                        })
                        .on_drop(cx.listener(
                            move |ws, drag: &super::library::GalleryDrag, _, cx| {
                                ws.cloud_drop_local(
                                    None,
                                    Some(drop_id.clone()),
                                    drag.paths.clone(),
                                    cx,
                                )
                            },
                        ))
                        .child(format!("▸ {}", folder.name)),
                )
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(ui::button(
                            "Rename",
                            false,
                            move |ws, _, cx| {
                                ws.cloud.form_target = Some((rename.id.clone(), rename.revision));
                                form(
                                    ws,
                                    "rename-folder",
                                    vec![field("cloud-name", "Name", rename.name.clone())],
                                    cx,
                                )
                            },
                            cx,
                        ))
                        .child(ui::button(
                            "Delete",
                            false,
                            move |ws, _, cx| {
                                ws.cloud.form_target = Some((delete.id.clone(), delete.revision));
                                form(ws, "delete-folder", vec![], cx)
                            },
                            cx,
                        )),
                ),
        );
    }
    root = root.child(catalogue_pages(true, ws, cx)).child(ui::button(
        "+ Cloud bucket…",
        false,
        |ws, _, cx| {
            let mut fields = vec![field("cloud-name", "Bucket name", "")];
            let mut q = ws.cloud.query.clone();
            q.text.clear();
            q.filters = Default::default();
            fields.extend(filter_fields(&q));
            form(ws, "new-bucket", fields, cx)
        },
        cx,
    ));
    for (i, bucket) in ws.cloud.buckets.clone().into_iter().enumerate() {
        let id = bucket.id.clone();
        let local = id.clone();
        let external = id.clone();
        let local_folder = id.clone();
        let remote = id.clone();
        let edit = bucket.clone();
        let delete = bucket.clone();
        root = root.child(
            div()
                .id(("cloud-bucket", i))
                .flex()
                .flex_col()
                .child(
                    div()
                        .id(("cloud-bucket-name", i))
                        .px_2()
                        .py_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(gpui::rgb(ui::palette().field_bg)))
                        .drag_over::<RemoteDrag>(|s, _, _, _| s.bg(gpui::rgb(ui::palette().accent)))
                        .drag_over::<super::library::GalleryDrag>(|s, _, _, _| {
                            s.bg(gpui::rgb(ui::palette().accent))
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _, _, cx| {
                                ws.cloud_browse(Scope::Bucket { id: id.clone() }, cx)
                            }),
                        )
                        .on_drop(cx.listener(
                            move |ws, drag: &super::library::GalleryDrag, _, cx| {
                                cx.stop_propagation();
                                ws.cloud_drop_local(
                                    Some(local.clone()),
                                    None,
                                    drag.paths.clone(),
                                    cx,
                                )
                            },
                        ))
                        .on_drop(cx.listener(move |ws, drag: &LocalFolderDrag, _, cx| {
                            cx.stop_propagation();
                            ws.cloud_drop_local(
                                Some(local_folder.clone()),
                                None,
                                vec![drag.path.clone()],
                                cx,
                            );
                        }))
                        .on_drop(cx.listener(move |ws, drag: &ExternalPaths, _, cx| {
                            cx.stop_propagation();
                            ws.cloud_drop_local(
                                Some(external.clone()),
                                None,
                                drag.paths().to_vec(),
                                cx,
                            )
                        }))
                        .on_drop(cx.listener(move |ws, drag: &RemoteDrag, _, cx| {
                            cx.stop_propagation();
                            ws.cloud_drop_remote(remote.clone(), drag.items.clone());
                            cx.notify();
                        }))
                        .child(format!(
                            "{} {}",
                            if bucket.rule.is_some() { "✦" } else { "▣" },
                            bucket.name
                        )),
                )
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(ui::button(
                            "Edit",
                            false,
                            move |ws, _, cx| {
                                ws.cloud.form_target = Some((edit.id.clone(), edit.revision));
                                let mut fields =
                                    vec![field("cloud-name", "Bucket name", edit.name.clone())];
                                let mut q = schist_cloud::AssetQuery::default();
                                if let Some(rule) = &edit.rule {
                                    q.scope = rule.scope.clone();
                                    q.text = rule.text.clone();
                                    q.filters = rule.filters.clone();
                                }
                                ws.cloud.form_scope = q.scope.clone();
                                fields.extend(filter_fields(&q));
                                form(ws, "edit-bucket", fields, cx)
                            },
                            cx,
                        ))
                        .child(ui::button(
                            "Delete",
                            false,
                            move |ws, _, cx| {
                                ws.cloud.form_target = Some((delete.id.clone(), delete.revision));
                                form(ws, "delete-bucket", vec![], cx)
                            },
                            cx,
                        )),
                ),
        );
    }
    root.child(catalogue_pages(false, ws, cx))
        .into_any_element()
}
fn catalogue_pages(folders: bool, ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let (offset, total) = if folders {
        (ws.cloud.folders_offset, ws.cloud.folders_total)
    } else {
        (ws.cloud.buckets_offset, ws.cloud.buckets_total)
    };
    div()
        .flex()
        .gap_1()
        .children((offset > 0).then(|| {
            ui::button(
                "Previous",
                false,
                move |ws, _, cx| {
                    if folders {
                        ws.cloud.folders_offset = offset.saturating_sub(500);
                    } else {
                        ws.cloud.buckets_offset = offset.saturating_sub(500);
                    }
                    ws.cloud_refresh_catalogue();
                    cx.notify();
                },
                cx,
            )
        }))
        .children((offset + 500 < total).then(|| {
            ui::button(
                "More",
                false,
                move |ws, _, cx| {
                    if folders {
                        ws.cloud.folders_offset = offset + 500;
                    } else {
                        ws.cloud.buckets_offset = offset + 500;
                    }
                    ws.cloud_refresh_catalogue();
                    cx.notify();
                },
                cx,
            )
        }))
}
pub(crate) fn grid(ws: &mut Workspace, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    let selected = ws.cloud.selected.clone();
    let items = ws.cloud.assets.clone();
    let mut rows = Vec::new();
    for (i, asset) in items.into_iter().enumerate() {
        let select = asset.id.clone();
        let open = asset.clone();
        let drag = RemoteDrag {
            items: if selected.contains(&asset.id) {
                selected
                    .iter()
                    .map(|id| map([("kind", "asset".into()), ("id", id.clone().into())]))
                    .collect()
            } else {
                vec![map([
                    ("kind", "asset".into()),
                    ("id", asset.id.clone().into()),
                ])]
            },
            label: asset.name.clone(),
        };
        let mut row = div()
            .id(("cloud-asset", i))
            .w(px(158.0))
            .h(px(176.0))
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .border_1()
            .border_color(gpui::rgb(if selected.contains(&asset.id) {
                ui::palette().accent
            } else {
                ui::palette().field_bg
            }))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |ws, ev: &MouseDownEvent, _, cx| {
                    if ev.click_count == 2 {
                        ws.cloud_open(open.clone(), cx);
                        return;
                    }
                    if !ev.modifiers.control && !ev.modifiers.platform {
                        ws.cloud.selected.clear();
                    }
                    if !ws.cloud.selected.insert(select.clone()) {
                        ws.cloud.selected.remove(&select);
                    }
                    cx.notify();
                }),
            )
            .on_drag(drag, |drag, _, _, cx| {
                cx.new(|_| DragLabel(drag.label.clone()))
            });
        if let Some((_, image)) = ws.cloud.thumbnails.get(&asset.id) {
            row = row.child(
                img(image.clone())
                    .w(px(138.0))
                    .h(px(128.0))
                    .object_fit(gpui::ObjectFit::Contain),
            );
        } else {
            row = row.child(div().w(px(138.0)).h(px(128.0)).child("Preview unavailable"));
        }
        rows.push(
            row.child(div().text_size(px(12.0)).truncate().child(asset.name))
                .into_any_element(),
        );
    }
    let offset = ws.cloud.query.offset;
    let total = ws.cloud.total;
    div()
        .flex()
        .flex_col()
        .flex_grow()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .p_3()
        .gap_2()
        .child(caption(ws.cloud.message.clone()))
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(ui::button(
                    "Search and filters…",
                    false,
                    |ws, _, cx| form(ws, "search", filter_fields(&ws.cloud.query), cx),
                    cx,
                ))
                .child(ui::button(
                    "Clear filters",
                    false,
                    |ws, _, cx| {
                        ws.cloud.query.text.clear();
                        ws.cloud.query.filters = Default::default();
                        ws.cloud.query.offset = 0;
                        ws.cloud_watch_assets();
                        cx.notify();
                    },
                    cx,
                ))
                .child(ui::button(
                    "Download selected…",
                    false,
                    |ws, _, cx| ws.cloud_download_selected(cx),
                    cx,
                ))
                .child(ui::button(
                    "Upload files…",
                    false,
                    |ws, _window, cx| {
                        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
                            files: true,
                            directories: true,
                            multiple: true,
                            prompt: Some("Upload to Schist Cloud".into()),
                        });
                        let (bucket, folder) = match &ws.cloud.query.scope {
                            Scope::Bucket { id } => (Some(id.clone()), None),
                            Scope::Folder { id, .. } => (None, Some(id.clone())),
                            _ => (None, None),
                        };
                        cx.spawn(async move |this, cx| {
                            if let Ok(Ok(Some(paths))) = prompt.await {
                                let _ = this.update(cx, |ws, cx| {
                                    ws.cloud_drop_local(bucket, folder, paths, cx)
                                });
                            }
                        })
                        .detach();
                    },
                    cx,
                )),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .child(caption(format!(
                    "{} photos · {} selected",
                    total,
                    selected.len()
                )))
                .children(
                    matches!(ws.cloud.query.scope, Scope::Bucket { .. }).then(|| {
                        ui::button(
                            "Remove selected from bucket",
                            false,
                            |ws, _, cx| {
                                if let Scope::Bucket { id } = &ws.cloud.query.scope {
                                    ws.cloud_mutate(
                                        "bucket.remove",
                                        vec![
                                            ("id", id.clone().into()),
                                            (
                                                "asset_ids",
                                                value(
                                                    ws.cloud
                                                        .selected
                                                        .iter()
                                                        .cloned()
                                                        .collect::<Vec<_>>(),
                                                ),
                                            ),
                                        ],
                                    );
                                    cx.notify();
                                }
                            },
                            cx,
                        )
                    }),
                ),
        )
        .child(
            div()
                .id("cloud-grid-scroll")
                .flex_grow()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .child(div().flex().flex_row().flex_wrap().gap_2().children(rows)),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .children((offset > 0).then(|| {
                    ui::button(
                        "Previous page",
                        false,
                        move |ws, _, cx| {
                            ws.cloud.query.offset = offset.saturating_sub(100);
                            ws.cloud_watch_assets();
                            cx.notify();
                        },
                        cx,
                    )
                }))
                .children((offset + 100 < total).then(|| {
                    ui::button(
                        "Next page",
                        false,
                        move |ws, _, cx| {
                            ws.cloud.query.offset = offset + 100;
                            ws.cloud_watch_assets();
                            cx.notify();
                        },
                        cx,
                    )
                })),
        )
        .into_any_element()
}
pub(crate) fn dialog(
    ws: &mut Workspace,
    kind: &'static str,
    fields: Vec<(&'static str, String, String)>,
    cx: &mut Context<Workspace>,
) -> gpui::AnyElement {
    let title = match kind {
        "sign-in" => "Sign into Schist Cloud",
        "search" => "Search cloud photos",
        "catalogue" => "Find cloud folders and buckets",
        "new-folder" => "New cloud folder",
        "new-bucket" => "New cloud bucket",
        "edit-bucket" => "Edit cloud bucket",
        "rename-folder" => "Rename cloud folder",
        "delete-folder" => "Delete cloud folder?",
        "delete-bucket" => "Delete cloud bucket?",
        "upload-document" => "Upload document to Schist Cloud",
        "download" => "Download cloud photo",
        _ => "Schist Cloud",
    };
    let mut body = div().flex().flex_col().gap_2();
    if kind.starts_with("delete-") {
        body = body.child(caption(if kind == "delete-folder" {
            "Only an empty folder can be deleted."
        } else {
            "Photos remain in your cloud library."
        }));
    }
    if kind.ends_with("bucket") && !kind.starts_with("delete") {
        body = body.child(caption(
            "Leave search and filters empty for a bucket filled by dragging photos in.",
        ));
    }
    for (key, label, committed) in fields {
        if key == "cloud-download-format" {
            let mut options = vec![(String::new(), "Current editable document".to_string())];
            if let Some(capabilities) = &ws.cloud.capabilities {
                if capabilities.original_download {
                    options.push(("original".into(), "Original file".into()));
                }
                let mut seen = std::collections::HashSet::new();
                for format in &capabilities.formats {
                    if !format.can_export {
                        continue;
                    }
                    for extension in &format.extensions {
                        if extension != "original"
                            && schist_cloud::transfer::valid_format(extension)
                            && seen.insert(extension.clone())
                        {
                            options.push((
                                extension.clone(),
                                format!("{} (.{})", format.name, extension),
                            ));
                        }
                    }
                }
            }
            let mut choices = div()
                .id("cloud-download-formats")
                .flex()
                .flex_col()
                .gap_1()
                .max_h(px(320.0))
                .overflow_y_scroll();
            for (id, name) in options {
                let display = format!("{} {}", if id == committed { "●" } else { "○" }, name);
                choices = choices.child(ui::button(
                    display,
                    false,
                    move |ws, _, cx| {
                        ws.update_modal(|modal| {
                            if let Modal::Cloud { fields, .. } = modal {
                                if let Some((_, _, selected)) = fields
                                    .iter_mut()
                                    .find(|(key, _, _)| *key == "cloud-download-format")
                                {
                                    *selected = id.clone();
                                }
                            }
                        });
                        cx.notify();
                    },
                    cx,
                ));
            }
            body = body.child(ui::field_row("Format", choices));
            continue;
        }
        if key == "cloud-folder" {
            let mut choices = div().flex().flex_col().gap_1();
            for (id, name) in std::iter::once((String::new(), "Unfiled".to_string())).chain(
                ws.cloud
                    .folders
                    .iter()
                    .map(|f| (f.id.clone(), f.name.clone())),
            ) {
                let display = format!("{} {}", if id == committed { "●" } else { "○" }, name);
                choices = choices.child(ui::button(
                    display,
                    false,
                    move |ws, _, cx| {
                        ws.update_modal(|modal| {
                            if let Modal::Cloud { fields, .. } = modal {
                                if let Some((_, _, v)) =
                                    fields.iter_mut().find(|(k, _, _)| *k == "cloud-folder")
                                {
                                    *v = id.clone();
                                }
                            }
                        });
                        cx.notify();
                    },
                    cx,
                ));
            }
            body = body.child(ui::field_row("Folder", choices));
            continue;
        }
        let active = ws.focused_field == Some(key);
        let shown = if active {
            ws.field_buffer.clone()
        } else {
            committed.clone()
        };
        let value = if active {
            let at = ws.field_cursor.min(shown.len());
            ui::caret_run(
                shown[..at].to_string(),
                shown[at..].to_string(),
                ws.caret_on(),
                ui::palette().text,
            )
            .into_any_element()
        } else {
            div().child(shown).into_any_element()
        };
        body = body.child(ui::field_row(
            label,
            div()
                .w(px(270.0))
                .min_h(px(24.0))
                .px_1()
                .bg(gpui::rgb(ui::palette().field_bg))
                .border_1()
                .border_color(gpui::rgb(if active {
                    ui::palette().accent
                } else {
                    ui::palette().field_bg
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _, _, cx| {
                        ws.focus_field(key, committed.clone());
                        cx.notify();
                    }),
                )
                .child(value),
        ));
    }
    let actions = div()
        .flex()
        .gap_2()
        .child(ui::button(
            "Cancel",
            false,
            |ws, _, cx| ws.close_modal(cx),
            cx,
        ))
        .child(ui::button(
            if kind == "sign-in" {
                "Continue in browser"
            } else if kind == "download" {
                "Download…"
            } else if kind.starts_with("delete-") {
                "Delete"
            } else {
                "Apply"
            },
            true,
            move |ws, _, cx| {
                ws.commit_focused_field();
                let Some(Modal::Cloud { fields, .. }) = ws.modal.clone() else {
                    return;
                };
                match ws.cloud_submit(kind, fields, cx) {
                    Ok(()) => ws.close_modal(cx),
                    Err(e) => {
                        ws.status = e.to_string().into();
                        ws.cloud.message = e.to_string();
                        cx.notify();
                    }
                }
            },
            cx,
        ));
    ui::modal_frame(title, 620.0, body, actions).into_any_element()
}
