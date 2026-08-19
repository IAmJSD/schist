use gpui::*;

struct Workspace;

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(rgb(0x1e1e1e))
    }
}

fn main() {
    env_logger::init();
    Application::new().run(|cx: &mut App| {
        cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| Workspace))
            .expect("failed to open window");
    });
}
