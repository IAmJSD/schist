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
| **1** | PiPL parse, filter selectors, `advanceState`, 8-bit RGB, the plug-in's own dialog, Windows only, in-process | **this crate**, verified against two shipping plug-ins on 32- and 64-bit |
| 2 | Out-of-process helper with shared-memory tiles, macOS, 16/32-bit, all modes, selections and transparency, buffer/handle/property suites | not started |
| 3 | Wine helper on Linux, 32-bit Windows helper, Rosetta helper on Apple Silicon, packaging | not started |
| 4 | ActionManager / descriptor recording, format plug-ins, big-document coordinates | not started |

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

- **Anything but 8-bit.** `depth` is reported as 8 and `imageMode` as RGB
  or grayscale. 16-bit Photoshop pixels run 0..32768, not 0..65535, which
  is its own trap for stage 2.
- **Selections, masks and transparency.** Only
  `filterCaseFlatImageNoSelection` is offered; a plug-in that declares it
  cannot filter that case is refused up front rather than run wrongly.
- **Documents past 32767 pixels.** `bigDocumentData` is null, so the
  16-bit rectangles are the limit and an oversized image is refused
  rather than silently wrapped into negative coordinates.
- **The descriptor / scripting suites.** `descriptorParameters` is null,
  so nothing records or plays back. `AcquireSuite` reports "not found"
  for everything, which is what makes a plug-in take its compatible path
  instead of misreading a zero.
- **Format, automation, selection and parser modules.** Filters only.
- **Most callback suites.** See above.
- **Crash isolation.** A plug-in fault kills the process.

### Loading

A plug-in is loaded with `LOAD_WITH_ALTERED_SEARCH_PATH` over a
canonicalised path, so DLLs sitting beside it resolve. Plug-ins ship
helper libraries in their own folder as a matter of course — an FFT
filter next to its FFTW build, say — and Windows does not search a
module's own directory when loading it. Without the flag those plug-ins
fail at `LoadLibraryExW` with nothing to explain why.

### Suites

`handleProcs` and `bufferProcs` are implemented; `sSPBasic` serves the
PICA handle suite by name and reports every other suite absent, which is
what makes a plug-in take its compatible path instead of misreading a
zero. Everything else — PseudoResource, Property, Image Services,
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
