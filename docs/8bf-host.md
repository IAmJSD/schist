# Hosting Photoshop plug-ins

A Photoshop plug-in is a native shared library with a renamed extension
— `.8bf` for a filter, `.8bi`/`.8be` for format import/export, `.8ba`
for automation. On Windows it is a DLL; on macOS a bundle. What makes it
a plug-in rather than a library is a **PiPL** resource: a small property
list naming the module's kind, its menu name and category, the image
modes it handles, and the entry point symbol to call.

The entry point is one C function:

```c
void ENTRYPOINT(short selector, FilterRecordPtr fr, intptr_t *data, short *result);
```

The host calls it with six selectors — About, Parameters, Prepare,
Start, Continue, Finish — and hands it a `FilterRecord`: a large
fixed-layout struct of rectangles, plane counts, colours, and pointers to
callback suites the *host* has to implement. The plug-in drives the pixel
loop from there: it sets `inRect`, the host fills `inData`, it writes
`outData`, and it repeats until it leaves an empty rectangle behind.

## Staging

| Stage | Scope | State |
|---|---|---|
| **1** | PiPL parse, filter selectors, `advanceState`, 8-bit RGB, the plug-in's own dialog | **done**, verified against nine plug-in families on 32- and 64-bit |
| **2** | Out-of-process helper, shared pixel buffer, the buffer/handle/property/colour suites, 16- and 32-bit, selections and transparency | **done** |
| **3** | Wine on Linux, 32-bit helper, FEX on Arm Linux, Rosetta on Apple Silicon, packaging | **policy and Wine path done**; macOS discovery and packaging still to do |
| **4** | Descriptor recording, format plug-ins, big-document coordinates | **big documents done**; descriptors written but not served, see below; format modules need a source that documents `FormatRecord`, and the API Guide does not |

### Scripting

Recording a filter's parameters and playing them back — what Last Filter
and actions are made of — is written, tested and **not served**. The read
and write sub-suites of `PIDescriptorParameters` are null.

The reason is the member order. Adobe documents every routine's
signature and no struct, and lists the routines alphabetically after Open
and Close, which is not the layout: handed the read suite in that order,
Filter Foundry opened a descriptor and then called slot 2 a million and a
half times without stopping. It was iterating keys, so `GetKey` is the
third member — one position of eighteen, and the write suite counts
sixteen routines while naming thirteen.

Serving a suite whose slots are in the wrong places is worse than not
serving one: a plug-in that works today stops. Null is the documented way
to say scripting is unavailable, and plug-ins fall back to keeping
parameters in the `parameters` handle, which is what they already do.

What would settle it is a plug-in whose recorded keys are known, so the
getter it reaches for can be identified the way `GetKey` was.
`SCHIST_8BF_TRACE` names whichever slot gets called.

## Where a plug-in runs

Schist does not load a `.8bf` itself. It writes the pixels into a file,
starts a **helper process** built for the plug-in's own architecture, and
waits. `src/launch.rs` holds the policy and `src/remote.rs` drives it.

| Schist runs on | plug-in is | how it runs |
|---|---|---|
| Windows x86-64 | Windows x86-64 | helper, directly |
| Windows x86-64 | Windows x86 | 32-bit helper, on WOW64 |
| Linux x86-64 | either Windows | helper under **Wine** |
| Linux arm64 | either Windows | helper under **Wine** under **[FEX-Emu](https://github.com/FEX-Emu/FEX)** |
| macOS arm64 | Apple Silicon | helper, directly |
| macOS arm64 | Intel | Intel helper under **Rosetta** |
| macOS x86-64 | Intel | helper, directly |
| macOS x86-64 | Apple Silicon | not possible — Rosetta goes Intel to Arm, not back |
| anywhere | the other OS's plug-ins | not possible |

Three things follow from the process split, none of which can be had in
process:

* **A plug-in fault costs a filter, not a document.** The helper catches
  it and says what happened — "the plug-in read or wrote memory it does
  not own at 0x…, and was stopped" — rather than leaving Schist to infer
  a crash from a process that vanished. The image is left as it was.
* **A plug-in runs in a helper built for *its* architecture.** That is
  what lets an Intel filter run on an Apple Silicon Mac at all, and it is
  simply not expressible in one address space.
* **The emulator wraps a command line.** Wine, FEX and Rosetta are
  wrappers around the helper's argv and nothing else in Schist knows
  they exist.

What is missing gets reported as something to do rather than a failure:
a plug-in that needs Wine on a machine without it is listed with "needs
Wine installed" and a link, not hidden or broken.

### The wire

Pixels go in a file both processes map, so an image crosses once however
large it is. Everything else — the request, progress, the ending — is a
length-prefixed frame on a loopback TCP socket, chosen because it works
identically for a native helper, a Windows helper under Wine, and an
Intel helper under Rosetta. Schist listens and the helper connects back,
so the helper needs no address of its own; a random token sent first
keeps a stray local connection out.

Cancelling is killing the helper. That is both simpler and more reliable
than asking a plug-in to stop, because a plug-in stuck in its own loop
never reads a message.

Stage 1 is deliberately in-process even though the eventual design is
not. Running a twenty-year-old binary inside the editor is not something
to ship — it will segfault and take the document with it — but proving
the ABI is a prerequisite for the process split, and doing it in one
address space keeps the failures legible while the layout is still being
established.

## What stage 1 does

```
crates/plugin-host-8bf
├── pe.rs       PE/COFF resource walk — pure bytes, no OS calls
├── pipl.rs     property list parsing, both byte orders
├── abi.rs      FilterRecord and the selector/mode/case constants
├── suites.rs   the handle, buffer and PICA-basic callbacks
└── host.rs     the selector sequence, advanceState, pixel marshalling
```

A PiPL that overstates its property count is read as far as it goes
rather than refused: shipping plug-ins do it, and everything that
matters is in the part that parses.

Discovery is host-independent: `pe.rs` and `pipl.rs` are byte parsers, so
a Linux build can list a folder of Windows plug-ins, print what each one
declares, and say exactly why it cannot run it. Only `Filter::open`
needs the platform to match.

```sh
cargo run -p schist-plugin-host-8bf --example 8bf -- inspect ~/Plug-Ins
cargo run -p schist-plugin-host-8bf --example 8bf -- apply Twirl.8bf in.ppm out.ppm
```

```
Schist > Invert
  file      /tmp/Invert.8bf
  machine   x86-64
  interface 4.0
  code      [Win64X86]
  enable    in(PSHOP_ImageMode, RGBMode, GrayScaleMode)
  BLOCKED   this build cannot load Windows plug-ins; running them under Wine is stage 3
```

### The pixel loop

`advanceState` and the `Continue` loop are the same operation seen from
two sides — commit the last output, hand over the next input — so
`Session::advance` serves both. A plug-in that uses `advanceState` does
all its work inside `Start` and never sees a `Continue`; one that does
not leaves rectangles behind and the host services them between calls.
Either way the last output is committed after the loop, because the host
writes back a region only when the plug-in asks for a different one.

Images come in as 8-bit interleaved planes and go back the same way.
`inColumnBytes`/`inPlaneBytes` are set explicitly rather than left zero,
because the API Guide says a zero there means "the host has not set it".

### The dialog

Filters draw their own modal dialogs with raw Win32 and expect a live
native event loop. `platformData` does not carry the window handle: it
points at a `PlatformData` whose first member is the `HWND`. Passing the
handle itself makes a plug-in fault reading at the handle's own numeric
value, which is how that was pinned down.

This does work off Windows, which was a surprise. Cross-compiling the
host to `x86_64-pc-windows-gnu` and running it under Wine on a headless
Xvfb display gets a real plug-in's real dialog on screen, and `xdotool`
can drive it. `tools/verify-8bf.sh` does exactly that.

### Padding

A plug-in may ask for a region that overhangs the image, and says in
`inputPadding` what it wants there: Adobe documents 0..=255 as a literal
fill value and names three other modes without ever printing their
numbers. Rather than guess, this host fills for 0..=255 and replicates
the edge for anything else — which satisfies `plugInWantsEdgeReplication`
outright, is a valid answer to `plugInDoesNotWantPadding` ("leave the
data random"), and is more useful than the error the third mode asks for,
which exists only because older hosts could not serve the region at all.
So the constants are recorded and not depended on, and a mode the host
has never seen still comes back with real pixels.

### Tracing

`SCHIST_8BF_TRACE=1` logs every selector call, every host callback the
plug-in makes, and every rectangle the host serves, with arguments:

```
[8bf] -> selector 3
[8bf] pica.acquire_suite("Photoshop Handle Suite for Plug-ins", 2)
[8bf] handle.new(1)
[8bf] handle.lock(0x7ffffea99490)
[8bf] handle.set_size(0x7ffffea99490, 129)
```

This is the only way to see what an uncooperative plug-in is asking for.
`SCHIST_8BF_BUFPROBE=1` goes further and replaces the buffer suite with
one interchangeable probe per slot, which is how that suite's member
order was established — a wrong order shows up as a call whose arguments
make no sense for the slot it landed on.

## What stage 1 does not do

- **Documents past 32767 pixels.** `bigDocumentData` is null, so the
  16-bit rectangles are the limit and an oversized image is refused
  rather than silently wrapped into negative coordinates.
- **The descriptor / scripting suites.** `descriptorParameters` is null,
  so nothing records or plays back. `AcquireSuite` reports "not found"
  for everything, which is what makes a plug-in take its compatible path
  instead of misreading a zero.
- **Format, automation, selection and parser modules.** Filters only.
  `FormatRecord` is not in the API Guide — its contents run from Filter
  Modules straight to Selection Modules — so there is nothing to write
  one from.
- **Some callback suites.** PseudoResource, Image Services, Channel
  Ports and the descriptor sub-suites are still null. Nothing tested so
  far needs them; `docs/8bf-abi-provenance.md` tracks what does.
- **Crash isolation.** A plug-in fault kills the process.

### Previews

A filter with a preview pane builds a `PSPixelMap` over its own working
pixels and asks the *host* to draw them — the host owns colour
management, so it is the one that knows how. `crates/plugin-host-8bf/src/display.rs`
reads the map (honouring `rowBytes`/`colBytes`/`planeBytes`, so planar
and interleaved layouts both work, and undoing any matte so transparent
edges do not show the colour they were composited against) and blits it
with `StretchDIBits`. Modes it cannot draw are refused rather than drawn
wrong, which is what the API Guide means by "Nonsuccess is generally due
to unsupported color modes".

This is not optional. Every FilterMeister-built plug-in checks for it and
refuses to run without it — "This plug-in requires Adobe Photoshop 2.5.2
or later functionality" — and FilterMeister is what a great deal of the
freeware world is built with.

Unlike `FilterRecord`, `PSPixelMap` is naturally aligned.

### Depth

8-, 16- and 32-bit images all go through, as grayscale or RGB — six
modes in all, since Photoshop treats each depth as a different mode
rather than as an attribute of one.

**16-bit runs 0..=32768, not 0..=65535.** Photoshop's range spans 32769
values so that half-way is exactly representable, and a host that hands
over 65535-scaled data gives a plug-in colours twice as bright as
intended across the whole top half. `Depth::Sixteen` says so and the
fixture inverts about 32768 to prove it. Previews scale the same way,
and 32-bit float previews clamp at 1.0 because scene-referred values
above white have nowhere else to go.

One thing this taught: a plug-in that supports 16-bit may not say so in
its `'mode'` flags. G'MIC declares only Grayscale and RGB there and
handles depth through `'enbl'`'s `PSHOP_ImageDepth` test instead. So only
the *base* mode is grounds for refusal; a missing deep-mode flag is not.

### Layers and selections

A trailing plane is transparency, not colour: four planes is RGB plus
alpha, two is grayscale plus alpha. A layer is offered as the editable
transparency case, and if the plug-in says it cannot filter that, the
protected case, and failing that as a flat image — losing the
transparency but running, which is what Adobe describes and beats
refusing.

A selection arrives as one byte per pixel, 255 meaning fully selected,
and is handed to the plug-in as mask data for whatever rectangle it asks
for. `autoMask` is the host's job: the plug-in filters the whole
rectangle and the host blends the result back through the selection, so
a half-selected pixel moves half way rather than being switched. A
plug-in that wants to do its own masking turns `autoMask` off and the
host stops.

Adobe's table says of mask data "0=no mask (selected) and 255=masked
(not selected)", which contradicts the rest of the same page and what
Photoshop does. It is coverage: 255 is selected.

### Colour services

Plug-ins ask the host to convert between colour spaces, because in
Photoshop the host is the one holding the document's profile.
`crates/plugin-host-8bf/src/color.rs` converts between RGB, HSB, HSL,
CMYK, Lab, XYZ and greyscale in Adobe's component ranges — one of which
is a trap, since **CMYK is stored inverted**, 0 meaning 100% ink.

There is no colour management here yet, so RGB, HSB, HSL and greyscale
are exact and CMYK, Lab and XYZ are textbook sRGB/D65 approximations of
what a profile would give. Worth knowing before trusting a CMYK number
that came back through this.

The host also answers "what is the foreground colour" and "what is the
pixel at this point", and refuses to choose a colour, since that wants a
picker this crate has no UI for — which lets a plug-in fall back to its
own.

### Document properties

A plug-in asks the host about the document through the Property suite —
how many channels, what they are called, the ruler units, the grid. This
host answers what it honestly knows and returns
`errPlugInPropertyUndefined` for the rest, including the serial number,
which plug-ins ask for to implement copy protection: inventing one would
be answering a question about a Photoshop licence that does not exist.
A plug-in can act on "I don't know" and cannot act on a plausible lie.

### Loading

A plug-in is loaded with `LOAD_WITH_ALTERED_SEARCH_PATH` over a
canonicalised path, so DLLs sitting beside it resolve. Plug-ins ship
helper libraries in their own folder as a matter of course — an FFT
filter next to its FFTW build, say — and Windows does not search a
module's own directory when loading it. Without the flag those plug-ins
fail at `LoadLibraryExW` with nothing to explain why.

### Suites

`handleProcs` and `bufferProcs` are implemented; `sSPBasic` serves the
PICA handle and buffer suites by name and reports every other suite
absent, which is what makes a plug-in take its compatible path instead of
misreading a zero. Both PICA suites exist because a real plug-in asked
for them by name, not on spec. Everything else — PseudoResource, Property, Image Services,
Channel Ports, the descriptor sub-suites — is null, the documented way to
say "unavailable".

Member order inside a suite is the one thing Adobe never prints — and it
cannot be inferred from the order the prose introduces the routines in.
The Handle suite's narrative order happens to match its struct order; the
Buffer suite's does not, and assuming otherwise put a wrong order in this
host for a commit. Both are now established the same way, separately: by
handing a real plug-in one interchangeable probe per slot and reading
which slot received arguments shaped like which routine. See
`SCHIST_8BF_BUFPROBE` and the note in `docs/8bf-abi-provenance.md`.

### Packing

`FilterRecord` is `#[repr(C, packed(4))]`: 560 bytes, with a pointer
following an `int32` and no hole between them. This is not what a naive
reading of the declaration gives you. Natural alignment inserts 4-byte
holes before `inData` and before `outData`, and by `platformData` the
record is 8 bytes too long — far enough that a real plug-in reads a
pointer out of the middle of the monitor record and faults on whatever
happens to be there. That was the single most expensive thing to find and
the single most important thing to get right.

The callback suites are the opposite: **not** packed. Both plug-ins drove
a naturally aligned `HandleProcs` correctly, and packing it segfaults
immediately. Different headers, different pragmas.

## Testing

The fixture is a C plug-in, `tests/fixtures/plugin.c`, compiled at test
time. Writing it in C is the point: it declares `FilterRecord`
independently and exports its own `offsetof` table, so `tests/layout.rs`
can check that Rust and a C compiler agree on the same 560-byte record,
field for field. It also carries the packing asymmetry — `#pragma
pack(push, 4)` around the record and nothing else — so a regression in
either direction fails a test rather than a plug-in.

`tests/pipl.rs` links a real x86-64 Windows DLL with mingw-w64, carrying
a PiPL resource built by the same code path a plug-in author's would use,
and walks it back out. `tests/run.rs` drives both entry points — one
using `advanceState`, one using the `Continue` loop — over a gradient and
checks every byte, including the partial tiles at the right and bottom
edges.

None of that involves Adobe, so `tools/verify-8bf.sh` does: it downloads
Filter Foundry and G'MIC-Qt, cross-compiles the host to both Windows
targets, and runs them under Wine.

- G'MIC has to get through `Prepare`, use the handle suite and call
  `advanceState`.
- The about selector has to return without faulting.
- Filter Foundry has to open its dialog, accept `255-r` typed into all
  three channel fields, and come back with every output channel equal to
  255 minus the input's red — on the 64-bit host, and again with the
  32-bit host driving the 32-bit build of the plug-in.
- Serving the PICA handle suite has to make the plug-in acquire, use and
  release it.

- A plug-in that ships a helper DLL beside it has to load at all, which
  is what guards the search-path flag.
- Two FilterMeister builds have to get past their capability check and
  draw a preview, which is what guards `displayPixels`.
- Adobe's own Dissolve has to dissolve, its ColorMunger has to reach
  `colorServices`, and its Propetizer has to walk the property table
  without faulting.

Only the shipped binaries are used; no project's source is read. Three
families are covered: Filter Foundry, G'MIC-Qt, and a set of Fourier
transforms whose only job here is to depend on a sibling DLL.

Tests that need a toolchain skip with a printed reason rather than
failing, but a toolchain that is *present and broken* is a hard failure:
a silent skip that reads as a pass is worse than no test.

## Provenance

Everything was derived from Adobe's published prose documentation, not
from the Photoshop SDK headers, which are licensed and are not vendored
here. [`8bf-abi-provenance.md`](8bf-abi-provenance.md) lists every ABI
fact and where it came from.

Twelve of them the prose did not pin down. All but one are now settled —
by running two real plug-ins as black boxes and watching where they read,
what they called and where they faulted, and by reading the suite headers
in chapter 3 more carefully. That closed the packing question, the suite
member orders, the selector numbers, the image-mode ordinals, the
`'mode'` flag set's bit order (which was backwards, and was making this
host refuse plug-ins that were willing to run), the `platformData`
indirection, the `AboutRecord`, the 32-bit path, and the two-byte prelude
on a Windows PiPL resource. Two more were closed by making a wrong guess
harmless rather than by guessing better — see the padding note above.

What remains is `SPBasicSuite` past its first two members, which the
guide documents nowhere and neither plug-in called.
