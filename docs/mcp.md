# The MCP server

`schist-mcp` exposes Schist to [Model Context Protocol] clients — Claude
Code, Claude Desktop, or anything else that speaks MCP over stdio. It
links the kernel and the same first-party plugin set the app assembles
(plus any installed WebAssembly plugins), so it needs no window, no GPU
and no display: everything runs headless through the same registry,
undo history and compositor the GUI uses.

The same catalog and dispatch also run *inside* the app, behind the AI
panel (View ▸ AI Panel) — there they operate on the document open in the
window instead of headless sessions. See [ai-panel.md](ai-panel.md).

[Model Context Protocol]: https://modelcontextprotocol.io

## Getting the binary

Every release ships `schist-mcp` alongside the app, built from the same
commit:

| platform | where it lands |
|---|---|
| Linux | `schist-mcp-linux-x86_64` / `schist-mcp-linux-aarch64` on the release page, next to the AppImages |
| macOS | `schist-mcp-macos.zip` on the release page — unzip it anywhere |
| Windows | `schist-mcp.exe`, installed beside `schist.exe` and also on the release page |

The macOS binary is signed and notarized in its own right, so it runs
without a Gatekeeper prompt; it does not need the app installed. Because
a flat binary cannot carry a stapled ticket, the first run does an online
notarization check.

Building it yourself works too, and needs no display:

```sh
cargo build --release -p schist-mcp
```

Register the binary with your client, e.g. for Claude Code:

```sh
claude mcp add schist -- /usr/local/bin/schist-mcp
```

## Sessions

Everything starts with `create_session` — open a file (PSD/PSB, PNG,
JPEG, WebP, TIFF, or Affinity `.af`/`.afphoto`/`.afdesign`/`.afpub`) or
create a blank document — and every other call takes the returned
session id. Sessions are independent documents with their own tool
state, selection and history, like the app's tabs.

## What a session can do

Everything installed is published as its own MCP tool, with its own
parameters, taken from the same plugin registry the app builds its
menus from. There is no catalog call to make first: the tool list *is*
the catalog, so a filter arrives with its sliders already described —
names, ranges, defaults, units and the choices of any dropdown — and a
third-party plugin dropped into `~/.config/schist/plugins` is published
the same way the built-ins are.

| published as | covers |
|---|---|
| `cmd_*` | every menu command, `cmd_edit_undo`/`cmd_edit_redo` included — 34 of them |
| `tool_*` | all 55 canvas tools; the call selects the tool and sets any of its options-bar values passed with it |
| `filter_*` | every filter, 134 of them, applied through the selection as one history entry |
| `adjust_*` | Image ▸ Adjustments, destructively on the active layer |

That is around 250 tools, which is the honest size of the application;
clients that page or filter their tool list will want to know.

A canvas tool is *driven* separately from being selected: `tool_stroke`
plays a pointer gesture through it in document pixels, and `tool_input`
sends Enter, Escape or raw keys to the modal ones (crop, transform,
type). Adjustments take their sliders as plain numbers, their
checkboxes as booleans, and anything with no slider for it — curve
points, gradient stops, per-range tables — through a `params` object in
the adjustment's own serde form.

Around all that: `get_state` (document, layer tree, selection, history,
editor state), `set_active_layer` and `set_layer_props`, `set_editor`
(colours, brush, tolerance), `render` (PNG of the canvas or a region,
returned inline and optionally written to disk), `save`/`export`
choosing the codec by extension, and `photoshop_plugins` for why a
`.8bf` in the plug-ins folder did not turn into a filter.

The semantics match the app shell: filters read the active raster
layer, write back feathered through the selection, and land as single
undoable edits; tool gestures go through the same `PointerInput`
document-space path as canvas clicks; vector shapes re-rasterize and
layer effects rebuild before every render.
