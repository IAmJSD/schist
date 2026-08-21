//! The macOS menu bar.
//!
//! macOS puts an application's menus in the system bar at the top of the
//! screen rather than inside its windows, so on that platform the in-window
//! bar `panels::menu_bar` draws is replaced by this one. Both are built from
//! the same `panels::menus` description, so the two cannot drift apart.
//!
//! Everything here compiles on every platform — only `sync` is a no-op off
//! macOS — so a change to the menus is type-checked wherever Schist builds.

use crate::actions::*;
use crate::panels::{self, MenuEntry};
use crate::workspace::Workspace;
use gpui::{Action, Context, Menu, MenuItem, SystemMenuType};

/// The bold first menu, named after the app.
const APP_NAME: &str = "Schist";

/// Rebuild the system menu bar, if anything it shows has changed since the
/// last frame. Cheap to call from `render`: the check is a small string
/// compare, and `NSMenu` is only rebuilt when a label or an entry differs.
pub fn sync(ws: &mut Workspace, cx: &mut Context<Workspace>) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let signature = signature(ws);
    if ws.native_menu.as_deref() == Some(signature.as_str()) {
        return;
    }
    ws.native_menu = Some(signature);
    cx.set_menus(build(ws));
}

/// What the built menus depend on beyond the (fixed) plugin registry: the
/// toggle labels, and the layer comps the Layer menu lists.
fn signature(ws: &Workspace) -> String {
    let v = &ws.view;
    let mut out = format!(
        "{}{}{}{}{}{}",
        v.rulers as u8,
        v.grid as u8,
        v.guides as u8,
        v.extras as u8,
        v.snap as u8,
        ws.color.proof.is_some() as u8,
    );
    if let Some(doc) = ws.doc.as_ref() {
        for comp in &doc.layer_comps {
            out.push('\u{1f}');
            out.push_str(&comp.name);
        }
    }
    out
}

fn build(ws: &Workspace) -> Vec<Menu> {
    let mut menus = vec![app_menu()];
    menus.extend(panels::menus(ws).into_iter().map(|(title, entries)| Menu {
        name: title.into(),
        items: items(ws, entries),
    }));
    menus
}

/// The items macOS expects under the application's own menu. Preferences,
/// Quit and Check for Updates live here rather than in File/View, and are
/// dropped from those menus by `item`.
fn app_menu() -> Menu {
    Menu {
        name: APP_NAME.into(),
        items: vec![
            // No About box to open yet; the update check is the other item
            // macOS keeps here.
            MenuItem::action(
                "Check for Updates…",
                RunAppItem {
                    item: AppItem::CheckForUpdates,
                },
            ),
            MenuItem::separator(),
            MenuItem::action("Preferences…", ShowPreferences),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            // No key equivalents: ⌘H is View ▸ Extras here, as it is in
            // Photoshop, and the menu would take the keystroke first.
            MenuItem::action(format!("Hide {APP_NAME}"), HideApp),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action(format!("Quit {APP_NAME}"), Quit),
        ],
    }
}

fn items(ws: &Workspace, entries: Vec<MenuEntry>) -> Vec<MenuItem> {
    let converted = entries.into_iter().filter_map(|e| item(ws, e)).collect();
    trim_separators(converted)
}

fn item(ws: &Workspace, entry: MenuEntry) -> Option<MenuItem> {
    Some(match entry {
        MenuEntry::Sep => MenuItem::Separator,
        MenuEntry::Cmd(id) => {
            let label = ws
                .registry
                .command(id)
                .map(|c| c.title.to_string())
                .unwrap_or_else(|| id.to_string());
            action_item(label, Box::new(RunCommand { id: id.to_string() }))
        }
        MenuEntry::Adjustment(kind) => action_item(
            kind.display_name().to_string(),
            Box::new(AddAdjustment {
                kind: adjustment_id(kind)?.to_string(),
            }),
        ),
        MenuEntry::Filter(id) => {
            let name = ws
                .registry
                .filters()
                .find(|f| f.id() == id)
                .map(|f| format!("{}…", f.name()))
                .unwrap_or_else(|| id.to_string());
            action_item(name, Box::new(OpenFilter { id: id.to_string() }))
        }
        MenuEntry::Dynamic(label, item) => action_item(label, Box::new(RunAppItem { item })),
        MenuEntry::App(label, item, _) => {
            action_item(label_for(ws, label, item), action_for(item)?)
        }
        MenuEntry::Sub(label, children) => MenuItem::submenu(Menu {
            name: label.into(),
            items: items(ws, children),
        }),
    })
}

fn action_item(name: String, action: Box<dyn Action>) -> MenuItem {
    MenuItem::Action {
        name: name.into(),
        action,
        // Cut/Copy/Paste are plugin commands operating on the document, not
        // on a focused text field, so none of them want the OS selector:
        // that would send them down the responder chain instead.
        os_action: None,
    }
}

/// The action a menu item dispatches. GPUI reads each item's key equivalent
/// out of the keymap by matching on the action, so items that already have a
/// binding must name the same action that binding does — otherwise the
/// shortcut would not show beside them.
///
/// `None` drops the item: it belongs in the application menu instead.
fn action_for(item: AppItem) -> Option<Box<dyn Action>> {
    Some(match item {
        AppItem::Quit | AppItem::Preferences | AppItem::CheckForUpdates => return None,
        AppItem::New => Box::new(NewFile),
        AppItem::Open => Box::new(OpenFile),
        AppItem::Close => Box::new(CloseTab),
        AppItem::Save => Box::new(SaveFile),
        AppItem::SaveAs => Box::new(SaveFileAs),
        AppItem::ZoomIn => Box::new(ZoomIn),
        AppItem::ZoomOut => Box::new(ZoomOut),
        AppItem::ZoomFit => Box::new(ZoomFit),
        AppItem::ZoomActual => Box::new(ZoomActual),
        AppItem::ImageSize => Box::new(ShowImageSize),
        AppItem::CanvasSize => Box::new(ShowCanvasSize),
        AppItem::LayerStyleItem => Box::new(ShowLayerStyle),
        AppItem::FreeTransform => Box::new(ActivateTool {
            id: "transform".into(),
        }),
        AppItem::ToggleRulers => Box::new(ToggleRulers),
        AppItem::ToggleGrid => Box::new(ToggleGrid),
        AppItem::ToggleGuides => Box::new(ToggleGuides),
        AppItem::ToggleExtras => Box::new(ToggleExtras),
        AppItem::ToggleSnap => Box::new(ToggleSnap),
        AppItem::ClearGuides => Box::new(ClearGuides),
        // Deliberately not `CycleScreenMode`: its only binding is a bare
        // "f", and a key equivalent without modifiers is swallowed by the
        // menu before the letter can reach a tool that is taking typing.
        other => Box::new(RunAppItem { item: other }),
    })
}

/// GPUI's menus have no check marks, and the macOS convention for a toggle
/// without one is a label that says what the click will do — Finder's "Hide
/// Sidebar" / "Show Sidebar". The in-window bar keeps its check marks.
fn label_for(ws: &Workspace, label: &'static str, item: AppItem) -> String {
    let toggled = |on: bool, verb: (&str, &str), noun: &str| {
        format!("{} {noun}", if on { verb.1 } else { verb.0 })
    };
    const SHOW: (&str, &str) = ("Show", "Hide");
    const ENABLE: (&str, &str) = ("Enable", "Disable");
    match item {
        AppItem::ToggleRulers => toggled(ws.view.rulers, SHOW, "Rulers"),
        AppItem::ToggleGrid => toggled(ws.view.grid, SHOW, "Grid"),
        AppItem::ToggleGuides => toggled(ws.view.guides, SHOW, "Guides"),
        AppItem::ToggleExtras => toggled(ws.view.extras, SHOW, "Extras"),
        AppItem::ToggleSnap => toggled(ws.view.snap, ENABLE, "Snapping"),
        AppItem::ProofColors => toggled(ws.color.proof.is_some(), ENABLE, "Proof Colors"),
        _ => label.to_string(),
    }
}

/// Drop the separators left hanging by items that moved to the application
/// menu — a menu must not open or close on a divider, or show two in a row.
fn trim_separators(items: Vec<MenuItem>) -> Vec<MenuItem> {
    let mut out: Vec<MenuItem> = Vec::with_capacity(items.len());
    for item in items {
        let separator = matches!(item, MenuItem::Separator);
        if separator && matches!(out.last(), None | Some(MenuItem::Separator)) {
            continue;
        }
        out.push(item);
    }
    if matches!(out.last(), Some(MenuItem::Separator)) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(items: &[MenuItem]) -> String {
        items
            .iter()
            .map(|item| match item {
                MenuItem::Separator => '-',
                _ => 'x',
            })
            .collect()
    }

    #[test]
    fn separators_do_not_open_close_or_double_up() {
        let items = vec![
            MenuItem::Separator,
            MenuItem::action("a", Quit),
            MenuItem::Separator,
            MenuItem::Separator,
            MenuItem::action("b", Quit),
            MenuItem::Separator,
        ];
        assert_eq!(shape(&trim_separators(items)), "x-x");
    }

    #[test]
    fn a_menu_of_nothing_but_separators_is_empty() {
        let items = vec![MenuItem::Separator, MenuItem::Separator];
        assert!(trim_separators(items).is_empty());
    }

    #[test]
    fn the_application_menu_holds_what_the_other_menus_drop() {
        // Quit and Preferences belong in the app menu; `action_for`
        // returning None is what keeps them out of File and View.
        for item in [
            AppItem::Quit,
            AppItem::Preferences,
            AppItem::CheckForUpdates,
        ] {
            assert!(action_for(item).is_none(), "{item:?} would be duplicated");
        }
        assert!(action_for(AppItem::New).is_some());
        assert_eq!(shape(&app_menu().items), "x-x-x-xxx-x");
    }
}
