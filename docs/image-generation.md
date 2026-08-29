# Image generation

**File ▸ Generate Images** signs in to a generation provider, renders
whatever form that provider asks for, and streams the images it generates
back into the open document as layers.

Schist has no model of its own and no provider baked in beyond a default
domain. A provider is any host that answers the five requests below;
`schist.app` is the one the field is pre-filled with, and typing another
domain in its place puts it through the identical flow.

The client lives in [`crates/imagegen`](../crates/imagegen) and knows
nothing about layers or GPUI; the dialog and the layer building are in
`crates/app/src/imagegen.rs`.

## The compile flag

The feature is the `imagegen` feature of `schist-app`, on by default:

```sh
cargo build --release -p schist-app                        # with it
cargo build --release -p schist-app --no-default-features  # without it
```

`schist-app` has no other features, so `--no-default-features` means
exactly this one thing. Built without it, the menu item is absent rather
than disabled, and the protocol client, its websocket and TLS
dependencies, and the account file are not in the binary at all.

## The protocol

Everything is HTTPS. A domain given as `http://` is refused rather than
upgraded, and so is any URL the provider hands back that is not `https:`
(or `wss:` for the stream) — a token is not worth carrying over
cleartext.

### 1. Discovery

`GET https://<domain>/.schist/auth-urls.json`

```json
{
  "authentication_url": "https://example.com/oauth/authorize",
  "code_exchange_url":  "https://example.com/oauth/token"
}
```

Both MUST be absolute. Schist appends `?state=<state>` to
`authentication_url` — 128 bits from the OS entropy pool — and opens it
in the user's browser.

### 2. The browser flow

The provider finishes by redirecting to:

```
schist://ig-callback?state=<the state>&code=<the code>
```

Schist registers the `schist:` scheme in all three packaging targets. On
macOS the URL reaches the running app as an Apple event; elsewhere the OS
starts a second Schist process with the URL on argv, which drops it in
`$XDG_RUNTIME_DIR` (or the config directory) and exits, and the instance
sitting on the dialog picks it up. A callback whose `state` is not the
one this flow started with is discarded.

### 3. The code exchange

`POST` to `code_exchange_url`:

```json
{ "response_type": "code", "code": "…", "state": "…", "schist_spec_version": 1 }
```

and, once the token expires, to the same URL:

```json
{ "response_type": "refresh_token", "refresh_token": "…", "schist_spec_version": 1 }
```

Both answer with:

```json
{
  "access_token": "…",
  "refresh_token": "…",
  "expires_at": 1735689600,
  "generation_endpoint_url": "https://example.com/generate",
  "logout_url": "https://example.com/session/abc"
}
```

`expires_at` is seconds since the Unix epoch, not a duration. Every
request below renews the token first if it has passed, so a provider can
move its own endpoints on any refresh. `DELETE logout_url`, with nothing
else attached, ends the session.

The token is written to `~/.config/schist/imagegen-account.json`, owner-
readable only, and is the only thing this feature persists.

### 4. The form

`GET generation_endpoint_url`, with `Authorization: Bearer <token>`,
answers with the form to draw, in display order:

```json
[
  {"t":"text","title":"Prompt","description":"What to draw",
   "required":true,"id":"prompt"},
  {"t":"select","title":"Style","description":"","required":false,
   "id":"style","multiple":true,
   "values":[{"id":"oil","text":"Oil paint"}]},
  {"t":"live_text_preview","live_preview_url":"https://example.com/preview"}
]
```

A `text` or `select` item contributes its value to the body under its
`id`, as a string or an array of the chosen option ids. `required` items
are enforced by the dialog, before anything is sent.

A `live_text_preview` is not an input: Schist `POST`s the current values
to its `live_preview_url` and shows the text that comes back, refreshed
after a short pause in typing rather than on every keystroke.

An item kind Schist does not know is an error, not something to skip —
dropping it would send a body missing a field the provider asked for.

### 5. Generating

`POST generation_endpoint_url` with the values. The answer is a `wss:`
URL as plain text, and everything after that happens on that socket.

The first message is the layout, as JSON:

```json
[{"part_name":"Cover","children_count":2},{"part_name":"Spread","children_count":1}]
```

Every message after it addresses one **image slot** by its flat index
across the whole layout — with parts of 3 and 2 children, index 4 is the
second child of the second part. Messages are either:

* **binary** — the top bit of the first byte is a done flag and its low
  seven bits are the slot index; the rest is a chunk of that slot's
  current image, buffered until the done bit arrives.
* **JSON** — `[index]`, meaning that slot is complete, or
  `[index, reason]`, meaning the provider refused it.

The done bit means "that image is whole", never "that slot is over": a
slot may finish several images before its status arrives, and only the
status ends it. The generation is over when every slot has had one.

Because a slot index is seven bits, a layout may declare at most 128
slots; a larger one is refused when it arrives rather than stranding on
the slots that could never be addressed.

## What lands in the document

Images are decoded by probing the bytes against the same codecs an opened
file uses, so a provider may send PNG, JPEG, WebP or TIFF without saying
which.

Each layout part becomes one layer named after the part, or — if it
produced more than one image — a group of that name holding `Part 1`,
`Part 2`, … Layers are centred on the canvas, inserted above the active
layer, and go in as a single **Generate Images** history entry, so one
undo takes the whole generation back out. With no document open, one is
created at the size of the largest image.
