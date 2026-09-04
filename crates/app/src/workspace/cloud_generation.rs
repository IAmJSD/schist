//! Dynamic generation form, live text previews and streamed image slots.
use super::*;
use crate::ui;
use gpui::{img, StatefulInteractiveElement as _, StyledImage as _};
use schist_cloud::generation::{self as api, Input, Inputs, Item};
use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, mpsc},
    time::{Duration, Instant},
};

pub(crate) struct Generated {
    index: usize,
    bytes: Vec<u8>,
    image: Option<Arc<RenderImage>>,
}
enum Job {
    Form(u64, std::result::Result<Vec<Item>, String>),
    Preview(u64, u64, String, std::result::Result<String, String>),
    Event(u64, api::Event),
    Image(u64, Generated),
    Done(u64, std::result::Result<(), String>),
}
pub(crate) struct GenerationState {
    items: Vec<Item>,
    pub values: Inputs,
    pub editing: Option<String>,
    previews: HashMap<String, String>,
    results: Vec<Generated>,
    slots: Vec<(String, bool, Option<String>)>,
    pub cancel: Arc<AtomicBool>,
    jobs: mpsc::Receiver<Job>,
    sender: mpsc::Sender<Job>,
    loading: bool,
    running: bool,
    error: String,
    last_values: Option<Inputs>,
    preview_due: Option<Instant>,
    seq: u64,
    total_bytes: usize,
}
impl Default for GenerationState {
    fn default() -> Self {
        let (sender, jobs) = mpsc::channel();
        Self {
            items: vec![],
            values: Inputs::new(),
            editing: None,
            previews: HashMap::new(),
            results: vec![],
            slots: vec![],
            cancel: Arc::new(AtomicBool::new(false)),
            jobs,
            sender,
            loading: false,
            running: false,
            error: String::new(),
            last_values: None,
            preview_due: None,
            seq: 0,
            total_bytes: 0,
        }
    }
}
impl Drop for GenerationState {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
impl Workspace {
    pub(crate) fn cloud_generate_open(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.cloud.account.clone() else {
            self.cloud_sign_in(cx);
            return;
        };
        self.open_modal(Modal::CloudGenerate, cx);
        let g = &mut self.cloud.generation;
        if !g.items.is_empty() || g.loading {
            return;
        }
        g.loading = true;
        let sender = g.sender.clone();
        let epoch = self.cloud.epoch;
        std::thread::spawn(move || {
            let result = api::form(&account).map_err(|e| e.to_string());
            let _ = sender.send(Job::Form(epoch, result));
        });
    }
    pub(crate) fn cloud_generation_tick(&mut self, cx: &mut Context<Self>) {
        let g = &mut self.cloud.generation;
        let mut changed = false;
        while let Ok(job) = g.jobs.try_recv() {
            changed = true;
            match job {
                Job::Form(epoch, result) if epoch == self.cloud.epoch => {
                    g.loading = false;
                    match result {
                        Ok(items) => g.items = items,
                        Err(e) => g.error = e,
                    }
                }
                Job::Preview(epoch, seq, url, result)
                    if epoch == self.cloud.epoch && seq == g.seq =>
                {
                    g.previews.insert(
                        url,
                        result.unwrap_or_else(|e| format!("Preview unavailable: {e}")),
                    );
                }
                Job::Done(epoch, result) if epoch == self.cloud.epoch => {
                    g.running = false;
                    if let Err(e) = result {
                        g.error = e;
                    }
                }
                Job::Event(epoch, event) if epoch == self.cloud.epoch => match event {
                    api::Event::Layout(parts) => {
                        g.slots.clear();
                        for part in parts {
                            for i in 0..part.children_count {
                                g.slots.push((
                                    format!("{} · {}", part.part_name, i + 1),
                                    false,
                                    None,
                                ));
                            }
                        }
                    }
                    api::Event::Complete { index, rejected } => {
                        if let Some(slot) = g.slots.get_mut(index) {
                            slot.1 = true;
                            slot.2 = rejected;
                        }
                    }
                    api::Event::Image { .. } => unreachable!("image decoded by worker"),
                },
                Job::Image(epoch, generated) if epoch == self.cloud.epoch => {
                    g.total_bytes += generated.bytes.len();
                    g.results.push(generated);
                }
                _ => {}
            }
        }
        if matches!(self.modal, Some(Modal::CloudGenerate)) && !g.loading {
            if g.last_values.as_ref() != Some(&g.values) {
                g.last_values = Some(g.values.clone());
                g.seq += 1;
                g.preview_due = Some(Instant::now() + Duration::from_millis(300));
            }
            if g.preview_due.is_some_and(|due| Instant::now() >= due) {
                g.preview_due = None;
                if let Some(account) = self.cloud.account.clone() {
                    let urls: Vec<_> = g
                        .items
                        .iter()
                        .filter_map(|item| {
                            if let Item::LiveTextPreview { live_preview_url } = item {
                                Some(live_preview_url.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    let inputs = g.values.clone();
                    let seq = g.seq;
                    let epoch = self.cloud.epoch;
                    let sender = g.sender.clone();
                    std::thread::spawn(move || {
                        for url in urls {
                            let result =
                                api::preview(&account, &url, &inputs).map_err(|e| e.to_string());
                            let _ = sender.send(Job::Preview(epoch, seq, url, result));
                        }
                    });
                }
            }
        }
        if changed {
            cx.notify();
        }
    }
    fn cloud_generate_run(&mut self, cx: &mut Context<Self>) {
        self.commit_focused_field();
        let Some(account) = self.cloud.account.clone() else {
            return;
        };
        let g = &mut self.cloud.generation;
        if g.running {
            return;
        }
        if let Err(e) = api::validate(&g.items, &g.values) {
            g.error = e.to_string();
            cx.notify();
            return;
        }
        g.cancel = Arc::new(AtomicBool::new(false));
        g.running = true;
        g.error.clear();
        g.results.clear();
        g.slots.clear();
        g.total_bytes = 0;
        let inputs = g.values.clone();
        let cancel = g.cancel.clone();
        let sender = g.sender.clone();
        let epoch = self.cloud.epoch;
        std::thread::spawn(move || {
            let mut retained = 0usize;
            let result = api::generate(&account, &inputs, &cancel, |event| {
                if let api::Event::Image { index, bytes } = event {
                    retained += bytes.len();
                    if retained > 256 * 1024 * 1024 {
                        cancel.store(true, Ordering::Relaxed);
                        return;
                    }
                    let image = (|| -> anyhow::Result<_> {
                        let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
                            .with_guessed_format()?;
                        let mut limits = image::Limits::default();
                        limits.max_image_width = Some(16384);
                        limits.max_image_height = Some(16384);
                        limits.max_alloc = Some(256 * 1024 * 1024);
                        reader.limits(limits);
                        let i = reader.decode()?.thumbnail(180, 140).to_rgba8();
                        Ok(super::library::rgba_to_render_image(
                            i.width(),
                            i.height(),
                            i.into_raw(),
                        ))
                    })()
                    .ok()
                    .flatten();
                    let _ = sender.send(Job::Image(
                        epoch,
                        Generated {
                            index,
                            bytes,
                            image,
                        },
                    ));
                } else {
                    let _ = sender.send(Job::Event(epoch, event));
                }
            })
            .map_err(|e| e.to_string())
            .and_then(|()| {
                if retained > 256 * 1024 * 1024 {
                    Err("Generated results exceed the memory limit".into())
                } else {
                    Ok(())
                }
            });
            let _ = sender.send(Job::Done(epoch, result));
        });
        cx.notify();
    }
    fn cloud_open_generated(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(generated) = self.cloud.generation.results.get(index) else {
            return;
        };
        let result = self
            .registry
            .codecs()
            .find(|c| c.probe(&generated.bytes))
            .ok_or_else(|| anyhow::anyhow!("Unsupported generated image format"))
            .and_then(|codec| codec.import(&generated.bytes));
        match result {
            Ok(mut doc) => {
                doc.title = format!("Generated {}", generated.index + 1);
                doc.path = None;
                doc.dirty = true;
                self.open_in_tab(doc, true);
                self.library.open = false;
                self.close_modal(cx);
            }
            Err(e) => {
                self.cloud.generation.error = e.to_string();
                cx.notify();
            }
        }
    }
}
pub(crate) fn dialog(ws: &mut Workspace, cx: &mut Context<Workspace>) -> gpui::AnyElement {
    let mut body = div()
        .id("cloud-generation-form")
        .flex()
        .flex_col()
        .gap_2()
        .max_h(px(600.0))
        .overflow_y_scroll();
    let g = &ws.cloud.generation;
    if g.loading {
        body = body.child("Loading generation form…");
    }
    if !g.error.is_empty() {
        body = body.child(div().text_color(gpui::rgb(0xd45b50)).child(g.error.clone()));
    }
    for item in g.items.clone() {
        match item {
            Item::Text {
                id,
                title,
                description,
                required,
            } => {
                let current = match g.values.get(&id) {
                    Some(Input::Text(s)) => s.clone(),
                    _ => String::new(),
                };
                let focused = ws.focused_field == Some("cloud-generation-input")
                    && g.editing.as_ref() == Some(&id);
                let shown = if focused {
                    ws.field_buffer.clone()
                } else {
                    current.clone()
                };
                let control = div()
                    .min_h(px(25.0))
                    .w(px(330.0))
                    .px_1()
                    .bg(gpui::rgb(ui::palette().field_bg))
                    .border_1()
                    .border_color(gpui::rgb(if focused {
                        ui::palette().accent
                    } else {
                        ui::palette().field_bg
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |ws, _, _, cx| {
                            ws.commit_focused_field();
                            ws.cloud.generation.editing = Some(id.clone());
                            ws.focus_field("cloud-generation-input", current.clone());
                            cx.notify();
                        }),
                    )
                    .child(if focused {
                        let at = ws.field_cursor.min(shown.len());
                        ui::caret_run(
                            shown[..at].into(),
                            shown[at..].into(),
                            ws.caret_on(),
                            ui::palette().text,
                        )
                        .into_any_element()
                    } else {
                        div().child(shown).into_any_element()
                    });
                body = body
                    .child(ui::field_row(
                        format!("{title}{}", if required { " *" } else { "" }),
                        control,
                    ))
                    .child(div().text_size(px(11.0)).child(description));
            }
            Item::Select {
                id,
                title,
                description,
                required,
                multiple,
                values,
            } => {
                let selected: Vec<String> = match g.values.get(&id) {
                    Some(Input::Text(s)) => vec![s.clone()],
                    Some(Input::Multiple(v)) => v.clone(),
                    _ => vec![],
                };
                let mut choices = div().flex().flex_wrap().gap_1();
                for choice in values {
                    let field = id.clone();
                    let option = choice.id.clone();
                    let chosen = selected.contains(&option);
                    choices = choices.child(ui::button(
                        format!("{} {}", if chosen { "●" } else { "○" }, choice.text),
                        false,
                        move |ws, _, cx| {
                            let values = &mut ws.cloud.generation.values;
                            if multiple {
                                let mut selected = match values.get(&field) {
                                    Some(Input::Multiple(v)) => v.clone(),
                                    _ => vec![],
                                };
                                if selected.contains(&option) {
                                    selected.retain(|s| s != &option);
                                } else {
                                    selected.push(option.clone());
                                }
                                values.insert(field.clone(), Input::Multiple(selected));
                            } else if chosen && !required {
                                values.remove(&field);
                            } else {
                                values.insert(field.clone(), Input::Text(option.clone()));
                            }
                            cx.notify();
                        },
                        cx,
                    ));
                }
                body = body
                    .child(ui::field_row(
                        format!("{title}{}", if required { " *" } else { "" }),
                        choices,
                    ))
                    .child(div().text_size(px(11.0)).child(description));
            }
            Item::LiveTextPreview { live_preview_url } => {
                body = body.child(
                    div().text_size(px(12.0)).child(
                        g.previews
                            .get(&live_preview_url)
                            .cloned()
                            .unwrap_or_else(|| "Preparing preview…".into()),
                    ),
                )
            }
        }
    }
    for (name, complete, rejected) in &g.slots {
        body = body.child(div().text_size(px(11.0)).child(format!(
            "{name}: {}",
            rejected.as_deref().unwrap_or(if *complete {
                "Complete"
            } else {
                "Generating…"
            })
        )));
    }
    let mut results = div().flex().flex_wrap().gap_2();
    for (i, result) in g.results.iter().enumerate() {
        let mut card = div().flex().flex_col().gap_1();
        if let Some(image) = &result.image {
            card = card.child(
                img(image.clone())
                    .w(px(180.0))
                    .h(px(140.0))
                    .object_fit(gpui::ObjectFit::Contain),
            );
        }
        results = results.child(card.child(ui::button(
            "Open image",
            false,
            move |ws, _, cx| ws.cloud_open_generated(i, cx),
            cx,
        )));
    }
    body = body.child(results);
    let running = g.running;
    let actions = div()
        .flex()
        .gap_2()
        .child(ui::button(
            if running { "Stop" } else { "Close" },
            false,
            |ws, _, cx| {
                ws.cloud.generation.cancel.store(true, Ordering::Relaxed);
                ws.close_modal(cx);
            },
            cx,
        ))
        .children(
            (!running && !g.loading)
                .then(|| ui::button("Generate", true, |ws, _, cx| ws.cloud_generate_run(cx), cx)),
        );
    ui::modal_frame("Generate with Schist Cloud", 680.0, body, actions).into_any_element()
}
