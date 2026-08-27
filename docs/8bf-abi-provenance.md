# Where the 8BF ABI facts came from

`crates/plugin-host-8bf` was written **clean room**: from Adobe's
published prose documentation, not from the Photoshop SDK headers. No
SDK header was read or transcribed, and none is vendored here. That
keeps the crate free of the SDK licence, and it is why this file exists —
prose pins down most of the ABI but not all of it, and the gaps are
load-bearing.

## Sources

| Short name | Document |
|---|---|
| **API Guide** | *Adobe Photoshop API Guide*, version CS, October 2003 |
| **Resource Guide** | *Cross-Application Plug-in Development Resource Guide*, version 1.6, June 1999 |
| **PE spec** | Microsoft PE/COFF specification (for `src/pe.rs` only) |

## Documented — taken straight from the prose

| Fact | Source |
|---|---|
| `PIPropertyList` / `PIProperty` layout, four-byte property alignment, length excludes padding | Resource Guide tables 11-1, 11-2 |
| Integers stored in the platform's *native* byte order, so Windows PiPLs are little-endian and Mac ones big-endian | Resource Guide ch. 11 |
| Every Photoshop property carries vendor `'8BIM'` | Resource Guide table 11-2 |
| Property keys `kind` `vers` `prty` `mode` `enbl` `host` `catg` `name` `fici`, with their hex values | Resource Guide table 11-3 |
| `'kind'` values `8BFM` `8BIF` `8BAM` `8BEM` `8BSM` `8BYM` `8BXM` | Resource Guide table 11-3 |
| `PIWin32X86CodeDesc` is a bare null-terminated entry point name, null-padded to four bytes | Resource Guide table 11-8 |
| `FilterCaseInfo` is `char inputHandling, outputHandling, flags1, flags2`; seven entries | Resource Guide table 11-12 |
| The seven filter cases and their numbering, 1..7 | Resource Guide table 11-13 |
| `inputHandling` / `outputHandling` modes 0..11, each numbered explicitly | Resource Guide table 11-14 |
| `flags1` bit meanings, and that bit 0 is the least significant bit | Resource Guide table 11-15 |
| The image-mode *order* — bitmap, grayscale, indexed, RGB, CMYK, HSL, HSB, multichannel, duotone, Lab, gray 16, RGB 48 | Resource Guide table 11-3 |
| Every `FilterRecord` field, its C type, and its declaration order | API Guide table 63 |
| `PlugInMonitor` is ten `Fixed` values, gamma first; `gamma == 0` means invalid | API Guide table A-5 |
| `MACPASCAL OSErr (*AdvanceStateProc)(void)` and its contract | API Guide ch. 3 |
| Buffer suite routine signatures (`BufferSpaceProc`, `AllocateBufferProc`, `FreeBufferProc`, `LockBufferProc`, `UnlockBufferProc`) | API Guide ch. 3 |
| Error code ranges and `filterBadParameters` = -30100, `filterBadMode` = -30101 | API Guide table 2-4 |
| The selector calling sequence and the empty-rectangle termination rule | API Guide ch. 8 |
| Mask data is one byte per pixel; `maskRect` is ignored when there is no selection mask | API Guide table 63 |
| `AboutRecord`, not `FilterRecord`, is passed at `selectorAbout` | API Guide ch. 2 |
| `errorString` is a `Str255` the plug-in fills before returning `errReportString` | API Guide table 63 |

## Settled empirically, against shipping plug-ins

Adobe's prose leaves a dozen things open. Most were closed by running two
real, third-party, freely redistributable plug-ins under Wine — Filter
Foundry 1.7.0.25 (Telegraphics/ViaThinkSoft, GPL) and G'MIC-Qt 4.0.4
(0xC0000054) — and watching where they read, what they called, and where
they faulted. Only the shipped **binaries** were used; neither project's
source was read, so the clean-room line holds. `tools/verify-8bf.sh`
reproduces the whole run.

| Fact | How it was settled |
|---|---|
| **`FilterRecord` is packed to four bytes**, not naturally aligned | A plug-in read an 8-byte pointer at record offset 224. Poking distinct values there moved the fault address to match exactly, so the read was confirmed rather than inferred. Natural alignment puts nothing meaningful at 224 — it is the tail of `monitor` — because two 4-byte holes before `inData` and `outData` push everything after them 8 bytes late. `packed(4)` puts `bufferProcs` at 224 exactly, and both plug-ins then ran |
| **The callback suites are *not* packed** | The opposite of the record, and not a guess: both plug-ins drove a naturally aligned `HandleProcs` correctly, and packing it to four bytes segfaults the fixture immediately |
| **`HandleProcs` member order** — new, dispose, getSize, setSize, lock, unlock | Filter Foundry's call sequence is coherent only in this order: `new(1)`, `lock`, `get_size`, `unlock`, `set_size(129)`, `lock`, `unlock`, `set_size(53)`, and finally `dispose` of the same handle. A wrong order calls a different function with the wrong argument shape |
| **`SPBasicSuite.AcquireSuite` is the first member** | Filter Foundry called it with `("Photoshop Handle Suite for Plug-ins", 2)` and then `(…, 1)` — a legible name and a version pair, so the slot and the signature are both right |
| **`filterSelector` values 0..5** | Prepare (2), Start (3), Continue (4) and Finish (5) each behaved as the API Guide describes, in that order, for both plug-ins; Parameters (1) is what raises the dialog |
| **`plugInMode` ordinals**, extended to 17 | Each plug-in's `'mode'` flag set matches, bit for bit, the modes its `'enbl'` string names in prose. That pins the ordinals *and* recovers the six the 1999 guide predates |
| **The `'mode'` flag set runs most-significant bit first** | The same cross-check. Reading it the other way round claimed Filter Foundry supports Bitmap and Indexed, which its `'enbl'` denies, and claimed G'MIC supports CMYK and HSB rather than Grayscale and RGB — which is what made this host refuse a plug-in that was perfectly willing to run |
| **`platformData` is a pointer to a struct holding the window handle**, not the handle | Passing the `HWND` directly made both plug-ins fault reading at the handle's own numeric value, `0x10020`. Passing `&PlatformData { hwnd }` fixed both |
| **The `'8664'` PiPL key for the Win64 entry point** | Both plug-ins carry it, and the names it yields — `PluginMain`, `GmicPlugin` — resolve as exports |
| **A Windows PiPL resource has a two-byte prelude**, `01 00`, before `version` | Present in both, from different authors and toolchains. The four-byte property alignment is relative to the start of the *list*, so with the prelude the properties are not four-byte aligned in the file. The tolerant offset scan in `Pipl::parse` exists for this and is load-bearing |
| **`userCanceledErr` is -128** | G'MIC returns it when it cannot raise its UI, and cleans up afterwards rather than erroring — the behaviour of a cancel, not a failure |
| `FilterColor` is four bytes | Corroborated rather than proven: any other size shifts the whole tail, and the 224 anchor lands exactly with four |

The end-to-end proof is Filter Foundry running its own Win32 dialog,
parented to the handle this host passes, with `255-r` typed into all
three channel fields: every output channel comes back as 255 minus the
input's red. Dialog, parameters, `Start`, the host-driven `Continue`
loop, and `Finish` — all of it, through a plug-in that has never heard
of Schist.

## Still unverified

| # | Fact | Why it is still open | Failure mode if wrong |
|---|---|---|---|
| 1 | **`BufferProcs` member order** | Neither plug-in tested allocates through the buffer suite, so nothing exercised it. The suite carries its documented `version`/`count` header, so a plug-in that checks refuses rather than misbehaves | Wrong function pointer called |
| 2 | Padding constants `-1` / `-2` / `-3` | Nothing requested a region outside the image | Out-of-bounds requests padded wrongly, or refused when they should not be |
| 3 | `errReportString` = -30902 | Neither plug-in reported a string | An error is reported as a different error |
| 4 | The `AboutRecord` layout | The about box was never raised | Misbehaves; affects only that one selector |
| 5 | `HostProc` signature | Passed as null, so unused | None while it stays null |
| 6 | `handleProcsVersion` / `numHandleProcs` values | Both plug-ins used the suite without complaint, which means they did not check — so the values are untested, not confirmed | A plug-in that version-checks refuses to run |
| 7 | Whether `bigDocumentData` and `descriptorParameters` must be non-null | This host provides both because Photoshop always does. Explicitly nulling either changed nothing for the plug-ins tested, so their necessity is unproven either way | A plug-in that dereferences them without checking would fault |
| 8 | Anything past 8-bit, or with a selection or transparency | Out of scope for stage 1 | — |

## Reproducing

`tools/verify-8bf.sh` downloads both plug-ins, cross-compiles the host to
`x86_64-pc-windows-gnu`, and runs discovery and a filter under Wine. It
needs `wine`, `Xvfb` and `xdotool`; it prints what it skips.
