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
| **1** | PiPL parse, filter selectors, `advanceState`, 8-bit RGB, the plug-in's own dialog, Windows only, in-process | **this crate** |
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

Filters draw their own modal dialogs with raw Win32 against the window
handle in `platformData`, and expect a live native event loop. Stage 1
passes `RunOptions::parent_window` straight through; a null handle gives
an unparented dialog, which still works. Nothing about this is testable
off Windows.

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
- **Crash isolation.** A plug-in fault kills the process.

## Testing

The fixture is a C plug-in, `tests/fixtures/plugin.c`, compiled at test
time. Writing it in C is the point: it declares `FilterRecord`
independently and exports its own `offsetof` table, so `tests/layout.rs`
can check that Rust's `repr(C)` and a C compiler's natural alignment
produce the same 592-byte record, field for field. That is the one
layout assumption the whole host rests on.

`tests/pipl.rs` links a real x86-64 Windows DLL with mingw-w64, carrying
a PiPL resource built by the same code path a plug-in author's would use,
and walks it back out. `tests/run.rs` drives both entry points — one
using `advanceState`, one using the `Continue` loop — over a gradient and
checks every byte, including the partial tiles at the right and bottom
edges.

Tests that need a toolchain skip with a printed reason rather than
failing, but a toolchain that is *present and broken* is a hard failure:
a silent skip that reads as a pass is worse than no test.

## Provenance

Everything was derived from Adobe's published prose documentation, not
from the Photoshop SDK headers, which are licensed and are not vendored
here. [`8bf-abi-provenance.md`](8bf-abi-provenance.md) lists every ABI
fact, its source, and — importantly — the twelve that the prose does not
pin down, what a wrong guess costs, and how to settle each one against a
real plug-in.

The largest of those is the member order inside the callback suites: the
API Guide prints every routine's signature and none of the structs that
hold them. Until a known-good plug-in has been run on Windows, this crate
is unproven against anything but its own fixture.
