//! Toolbar groups: which tool a slot shows, flyouts, and cycling.

use super::*;

impl Workspace {
    // ----- toolbar groups -----

    /// Collect the registry's tools into toolbar groups, preserving
    /// registration order both between and within groups.
    pub fn rebuild_tool_groups(&mut self) {
        let mut groups: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
        for tool in self.registry.tools() {
            if !tool.in_toolbar() {
                continue;
            }
            let group = tool.group();
            match groups.iter_mut().find(|(g, _)| *g == group) {
                Some((_, tools)) => tools.push(tool.id()),
                None => groups.push((group, vec![tool.id()])),
            }
        }
        // Lay the slots out the way Photoshop does rather than in plugin
        // registration order. Groups not listed here (third-party tools)
        // keep their registration order and follow the built-ins.
        const ORDER: &[&str] = &[
            "move",
            "marquee",
            "lasso",
            "wand",
            "crop",
            "eyedropper",
            "brush",
            "clone",
            "eraser",
            "gradient",
            "dodge",
            "pen",
            "type",
            "shape",
            "hand",
            "zoom",
        ];
        groups
            .sort_by_key(|(group, _)| ORDER.iter().position(|g| g == group).unwrap_or(ORDER.len()));
        for (group, tools) in &groups {
            self.group_active.entry(group).or_insert(tools[0]);
        }
        self.tool_groups = groups;
    }

    /// The tool a group's toolbar slot currently represents.
    pub fn group_tool(&self, group: &'static str) -> &'static str {
        self.group_active
            .get(group)
            .copied()
            .or_else(|| {
                self.tool_groups
                    .iter()
                    .find(|(g, _)| *g == group)
                    .and_then(|(_, tools)| tools.first().copied())
            })
            .unwrap_or("move")
    }

    /// Keyboard shortcut shown for a group (Photoshop gives every tool in a
    /// group the same letter).
    pub fn group_shortcut(&mut self, group: &'static str) -> Option<&'static str> {
        let ids: Vec<&'static str> = self
            .tool_groups
            .iter()
            .find(|(g, _)| *g == group)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        ids.into_iter()
            .find_map(|id| self.registry.tool_mut(id).and_then(|t| t.shortcut()))
    }

    /// Press on a toolbar slot: hold opens the flyout, a click activates.
    pub fn press_tool_group(
        &mut self,
        group: &'static str,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.tool_press = Some(group);
        let has_siblings = self
            .tool_groups
            .iter()
            .any(|(g, tools)| *g == group && tools.len() > 1);
        if !has_siblings {
            return;
        }
        // Click-and-hold, like Photoshop's nested tools.
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(350))
                .await;
            this.update(cx, |ws, cx| {
                if ws.tool_press == Some(group) {
                    ws.open_tool_flyout(group, position, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Release on a toolbar slot: activate unless the hold already opened
    /// the flyout.
    pub fn release_tool_group(&mut self, group: &'static str, cx: &mut Context<Self>) {
        let pressed = self.tool_press.take();
        if pressed != Some(group) || self.tool_flyout.is_some() {
            return;
        }
        let tool = self.group_tool(group);
        self.activate_tool(tool, cx);
    }

    pub fn open_tool_flyout(
        &mut self,
        group: &'static str,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.tool_flyout = Some((group, position));
        self.context_menu = None;
        cx.notify();
    }

    pub fn close_tool_flyout(&mut self, cx: &mut Context<Self>) {
        self.tool_press = None;
        if self.tool_flyout.take().is_some() {
            cx.notify();
        }
    }

    /// Shift+the group's key steps to the next tool in that group.
    pub fn cycle_tool_group(&mut self, group: &'static str, cx: &mut Context<Self>) {
        let tools: Vec<&'static str> = self
            .tool_groups
            .iter()
            .find(|(g, _)| *g == group)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        if tools.is_empty() {
            return;
        }
        let current = self.group_tool(group);
        let next = tools
            .iter()
            .position(|t| *t == current)
            .map(|i| tools[(i + 1) % tools.len()])
            .unwrap_or(tools[0]);
        self.activate_tool(next, cx);
    }
}
