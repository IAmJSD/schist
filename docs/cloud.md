# Schist Cloud

In the desktop app, choose **File → Schist Cloud → Sign into Schist Cloud…**,
or use the welcome screen button. The domain prompt starts with `schist.app`.
Enter another provider domain if needed; Continue opens its sign-in page in the
browser. Installed Linux, macOS and Windows packages register the
`schist://ig-callback` handler. A development binary can receive a callback with
`schist 'schist://ig-callback?state=…&code=…'` while the original app is running.

The provider must serve `https://<domain>/.schist/auth-urls.json` and implement
the [Rust protocol types](../crates/cloud/src/protocol.rs) and
[transport contract](../crates/cloud/src/transport.rs) described below. This is a native client
implementation; it does not deploy the provider service or provision accounts.

## Gallery and documents

The gallery sidebar shows cloud folders and buckets after sign-in. Folder and
bucket lists can be searched and paged, and their contents update through live
subscriptions. Search a folder recursively, a bucket, or the whole cloud library.
Filters include MIME types, tags, edited state, content classification, capture
dates, minimum rating and geographic bounds. Smart buckets save a scope, query
and filters. Folder and bucket creation, renaming, deletion and bucket membership
changes use the same connection.

Drag local gallery photos, a watched local folder, or files/folders from the file
manager into a cloud bucket to upload them. Local originals remain in place.
Directory uploads retain relative paths and skip symlinks. Drag remote photos or
a remote folder into a bucket to add references without re-uploading. Smart
buckets combine manually added members with their saved rule's matches. Removing
manual membership can leave a photo visible if it still matches the rule.
Upload Files and Upload Current Document also offer a cloud folder
destination.

Double-clicking a remote asset downloads its export and joins its collaborative
document. **The asset ID is the document ID on the wire; `Asset.folder_id`
associates it with a folder.** Opening does not create or relocate an asset.
Uploads set `folder_id` before the returned asset is bound to the editor.
Bucket membership is independent of folder placement.

Collaborative edits sync automatically. Save waits for acknowledgement; a tab is
marked saved only when its latest local edits have been acknowledged. Undo/redo
tracks the local participant's changes. Reconnect joins with a Yjs state vector
and exchanges missing updates. Closing a tab leaves its document room.

Credentials use the operating system credential store. Crash recovery stores
MessagePack checkpoints under Schist's cloud state directory, including edits
made before joining a document; reopen the cloud asset to merge them. Writes are
serialized and atomically replace the previous checkpoint. Local selection,
history-brush sources and other editor-only state stay local to the tab.

## Wire format

`crates/cloud` owns one authenticated WSS connection for folders, buckets,
queries, mutations and all open collaborative documents. Messages are MessagePack
maps; binary updates and state vectors use MessagePack `bin`, never Base64 or
numeric arrays. Ordinary asset transfers use signed HTTPS URLs obtained through
that socket. Credentials are not forwarded to those transfer URLs.

The client restores subscriptions after reconnecting, rejects obsolete query
snapshots, refreshes credentials and detects dead connections. Unacknowledged
ordinary mutations fail visibly and are not automatically replayed. Collaborative
updates are reconciled through state vectors. Requests carry separate request
and mutation IDs: request IDs correlate replies, while mutation IDs identify
operations for server-side deduplication.

The original image-generation API is also supported: provider-defined text and
choice fields, live text previews, streamed result slots and cancellation. Its
legacy per-generation socket uses the [generation API's](../crates/cloud/src/generation.rs)
JSON/binary slot format; it is
separate from the shared workspace socket.

### Shared image representation

The workspace protocol transports opaque Yjs v1 updates. The native editor supplies an image
model using Yrs, interoperable with Yjs, in a root map named `schist.image.v1`.
Every value in this map is binary:

| Key | Value |
| --- | --- |
| `document/size` | MessagePack tuple: width, height, resolution DPI |
| `document/title` | UTF-8 |
| `document/metadata` | Layerless 1×1 PSD carrying document metadata |
| `document/comps` | MessagePack layer comps with stable layer references |
| `layer/<id>/placement` | MessagePack tuple: parent ID (`root` at top level), sibling rank |
| `layer/<id>/template` | Single-layer 1×1 PSD preserving layer kind and advanced properties, without raster/mask tiles or children |
| `layer/<id>/name` | UTF-8 |
| `layer/<id>/visible`, `locked`, `clipping` | One boolean byte |
| `layer/<id>/opacity`, `fill` | Little-endian float32 |
| `layer/<id>/blend` | Four-byte PSD blend key |
| `layer/<id>/pixels/<x>/<y>` | Depth byte (8, 16, 32), then a complete RGBA tile; multibyte samples are little-endian |
| `layer/<id>/mask/<x>/<y>` | Complete single-channel 8-bit mask tile |

Independent properties and tiles merge independently. Concurrent writes to the
same key use Yjs conflict resolution; painting the same tile is not a per-pixel
merge. Existing layers receive deterministic `seed/…` IDs; newly inserted layers
receive UUIDs. The initial seed reserves Yjs client ID 1, and must be generated
from the same initial export. An existing room takes precedence over that export.
Participants implementing this model must reserve that client ID too.

The provider must persist the Yjs room, enforce access to its asset, and include
the folder ID when returning assets. To make collaboration visible in later
downloads, thumbnails, search indexes and other clients, the provider must
materialize this image model or use a compatible exporter. A generic opaque-Yjs
relay alone cannot produce those image exports.

Workspace messages are capped at 256 MiB.
Oversized updates remain local and show an error; larger documents need protocol
chunking before they can sync. Asset downloads are capped at 512 MiB. Generation
limits are 128 slots, 64 MiB per image and 256 MiB of retained result bytes.

## Checks

`make check-cloud` runs protocol, authentication validation, real local-WebSocket
reconnect/multiplexing, collaborative merge, local-only undo, recovery and tile
round-trip tests, then checks the desktop app. `make app PROFILE=debug` builds
the desktop binary. Live provider authentication and server-side persistence
require a running compatible service and account.
