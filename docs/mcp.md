# The MCP server

`schist-mcp` exposes Schist to [Model Context Protocol] clients — Claude
Code, Claude Desktop, or anything else that speaks MCP over stdio. It
links the kernel and the same first-party plugin set the app assembles
(plus any installed WebAssembly plugins), so it needs no window, no GPU
and no display: everything runs headless through the same registry,
undo history and compositor the GUI uses.

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

The surface is deliberately generic rather than one MCP tool per
feature: `describe` enumerates what is installed, and four invokers
cover all of it, so a third-party plugin dropped into
`~/.config/schist/plugins` is as reachable as a built-in.

| MCP tool | covers |
|---|---|
| `run_command` | every menu command, `edit.undo`/`edit.redo` included |
| `select_tool`, `set_tool_options`, `tool_stroke`, `tool_input` | all 55 canvas tools, driven by document-space pointer gestures and commit/cancel/key input |
| `apply_filter` | every filter, applied through the selection as one history entry |
| `apply_adjustment` | Image ▸ Adjustments, destructively on the active layer |

Around those: `get_state` (document, layer tree, selection, history,
editor state), `set_active_layer` and `set_layer_props`, `set_editor`
(colours, brush, tolerance), `render` (PNG of the canvas or a region,
returned inline and optionally written to disk), and `save`/`export`
choosing the codec by extension.

The semantics match the app shell: filters read the active raster
layer, write back feathered through the selection, and land as single
undoable edits; tool gestures go through the same `PointerInput`
document-space path as canvas clicks; vector shapes re-rasterize and
layer effects rebuild before every render.
