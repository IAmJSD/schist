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

Adobe's prose leaves a dozen things open. Most were closed by running
real, third-party, freely redistributable plug-ins under Wine and
watching where they read, what they called, and where they faulted.
Three families, fifteen binaries:

- **Filter Foundry 1.7.0.25** (Telegraphics/ViaThinkSoft), 32- and 64-bit
- **G'MIC-Qt 4.0.4** (0xC0000054), whose one binary declares two filters
- **Ft/Fourier 08oct2019** (rechmbrs), twelve x86-64 transforms — the
  first to declare interface version **4.1** rather than 4.0
- **Adobe's own SDK samples** (Dissolve with and without AppleScript,
  ColorMunger, Propetizer), 32-bit, recovered from an archive repository
- **hayabuzo/Graphic-Filters**, eight 32-bit FilterMeister builds

Only the shipped **binaries** were used; no project's source was read, so
the clean-room line holds. GitHub code search was used once, purely as an
index — to find out *which* projects call `AllocateBlock` and so whose
binary was worth downloading — and not to read anything.
`tools/verify-8bf.sh` reproduces the whole run.

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

### A second pass closed the rest

The first pass left twelve gaps. A second — 32-bit Wine, the about
selector, and a closer reading of the API Guide's suite headers — closed
all but one.

| Fact | How it was settled |
|---|---|
| **`BufferProcs` version and routine count** — 2 and 5 | Chapter 3 heads each suite with both: "Buffer suite. Current version: 2; Adobe Photoshop: 5.0; Routines: 5" |
| **`BufferProcs` member order** — Allocate, Lock, Unlock, Free, Space | Read off a real plug-in. G'MIC was handed five interchangeable probes, one per slot, each logging the arguments it received; slot 0 arrived with `(3072, <stack pointer>)`, and 3072 is exactly one plane of the image it was filtering — unmistakably `AllocateBufferProc(size, &buffer)`. With the order restored, G'MIC allocates, locks, writes three planes, unlocks and frees, and its output is correct |
| **`handleProcsVersion` = 1, `numHandleProcs` = 7** | The same suite headers, printed in the prose. The host had been claiming 8 |
| **The API Guide's narrative order is *not* generally the struct order** | The Handle suite's prose order — New, Dispose, GetSize, SetSize, Lock, Unlock, RecoverSpace — happens to match what a plug-in was seen calling. Treating that single match as a rule and applying it to the Buffer suite put a **wrong order in this host for one commit**; the prose there runs Space, Allocate, Free, Lock, Unlock and the struct does not. Each suite's order has to be established on its own evidence. The Handle suite's is; the Buffer suite's now is too; neither licenses the other |
| **The padding constants stopped mattering** | Rather than guess three numbers the prose never prints, the host fills for the documented 0..=255 and replicates the edge for anything else. Replication satisfies `plugInWantsEdgeReplication` outright, is a valid answer to `plugInDoesNotWantPadding` ("leave the data random"), and beats the error `plugInWantsErrorOnBoundsException` asks for, which exists only because older hosts could not serve the region. A fixture requests a rectangle overhanging every edge under a mode the host has never heard of and still gets usable pixels |
| **`errReportString` stopped mattering** | The host reports whatever the plug-in wrote into `errorString` whatever result code came with it. A non-empty `Str255` only happens because the plug-in filled it, so the string is the signal and the code need not be known |
| **The `AboutRecord` layout** | Filter Foundry's about box renders correctly and the selector returns 0, so `platformData` is at offset 0 and indirect there too |
| **The 32-bit path** | The host cross-compiled to `i686-pc-windows-gnu` loads the 32-bit Filter Foundry through its `'wx86'` descriptor and produces byte-identical pixels to the 64-bit run. `packed(4)` is right on both: on 32-bit it simply coincides with natural alignment |
| **`SPBasicSuite.ReleaseSuite` is the second member** | Serving the PICA handle suite made Filter Foundry call `ReleaseSuite` — which it never did while every `AcquireSuite` failed. Nothing else would have exercised that slot |
| **The PICA handle suite layout** | Chapter 4: "Suite PEA Handle suite. Current version: 1; Routines: 6" over New, Dispose, SetLock, GetSize, SetSize, RecoverSpace. Filter Foundry acquires it by name, calls `SetLock` with `lock=1` and later `lock=0`, and its pixels stay correct |
| `bigDocumentData` and `descriptorParameters` are not *required* | Explicitly nulling either changed nothing for both plug-ins. They are provided anyway because Photoshop always does, and because stage 4 needs the descriptor block regardless |
| **`PSPixelMap` is naturally aligned**, unlike `FilterRecord` | Filter Foundry's 64-bit preview draws correctly through it, and a 32-bit FilterMeister build does too. If the map were packed like the record, `base_addr` would be read four bytes early on 64-bit and the preview would be noise or a fault |
| **An overhanging *output* rectangle has to be served, not refused** | Adobe says the output rectangle must be a subset of `filterRect`. Propetizer asks for a row above the top edge anyway. Refusing leaves `outData` null, and Propetizer does not check — it writes through it and faults. Serving a buffer of the size asked for and clipping on commit is what survives real plug-ins |
| **Plug-ins really do write the negative padding modes** | FilterMeister sets `inputPadding`, `outputPadding` and `maskPadding` all to -2. Which named mode that is remains unknown and, by design, does not matter: the host replicates the edge for any value outside 0..=255 |
| **A plug-in must be loaded with its own directory on the DLL search path** | Not an ABI fact but a loading one, and it fails just as hard. The Fourier family ships FFTW beside it, and Windows does not search a module's own directory: all twelve failed at `LoadLibraryExW` with nothing to say why. `LOAD_WITH_ALTERED_SEARCH_PATH` over a canonicalised path fixes the lot |

## Still unverified

| # | Fact | Why it is still open | Failure mode if wrong |
|---|---|---|---|
| 0 | **`SPBasicSuite` members past the first two** — `IsEqual`, `AllocateBlock`, `FreeBlock`, `ReallocateBlock`, `Undefined` | `AcquireSuite` and `ReleaseSuite` are confirmed by position; the API Guide documents no `SPBasicSuite` struct anywhere, only usage examples of those two. **Twenty-seven plug-in binaries across five families have now been run and not one calls the rest.** Filter Foundry only ever asks for the handle suite; G'MIC asks for one ADM suite by GUID and falls back to Qt when refused; the Fourier family uses no host suite at all; Adobe's own samples use the direct callbacks. G'MIC's source *does* call `AllocateBlock` — GitHub code search says so, which is all it was used for — but nothing reachable from a filter run gets there, including its second entry point and its settings dialog. Adobe's published prose for **three** products (Photoshop, Premiere Pro, After Effects) documents only `AcquireSuite` and `ReleaseSuite`, and never the struct, so the documentation route is exhausted too | A plug-in that allocates through PICA calls the wrong slot. This is the last gap, and given how the Buffer suite turned out it should be assumed wrong until a plug-in proves otherwise rather than treated as probably fine. The probe technique below is what would settle it, the moment a plug-in that exercises it turns up |
| 2 | `HostProc`'s signature | Named but never printed, and passed as null | None while it stays null |
| 3 | The suites this host does not implement — PseudoResource, Property, Image Services, Channel Ports, and the descriptor sub-suites | All passed as null, which is the documented way to say "unavailable" | None; a plug-in that needs one declines |
| 4 | Anything past 8-bit, or with a selection or transparency | Out of scope for stage 1 | — |

## Gaps that block real plug-ins

Not ABI questions — things this host did not implement, each found by a
plug-in refusing to run. All three are now closed; the table is kept
because the evidence is the interesting part.

| Missing | What it blocks | Evidence |
|---|---|---|
| ~~`displayPixels`~~ — **now implemented** | Was blocking **FilterMeister**, and so a large slice of the freeware world | All eight Graphic-Filters plug-ins refused with "This plug-in requires Adobe Photoshop 2.5.2 or later functionality", and `displayPixels` alone was the field they checked. All eight now draw previews, and Filter Foundry's own preview pane — blank until this landed — shows the image on both architectures |
| ~~`colorServices`~~ — **now implemented** | Was hanging ColorMunger | ColorMunger is Adobe's colour-space conversion tester, so it doubles as an oracle. Fed pure green it now reports HSB 120/255/255, CMYK 0/255/0/255, Lab 224/42/211, Gray 150, HSL 120/255/128 and XYZ 91/182/30 — every one matching the textbook value, including CMYK's inverted storage |
| ~~`propertyProcs`~~ — **now implemented** | Was faulting Propetizer | Propetizer is Adobe's property tester, so it doubles as an oracle too: it now reads ruler units "pixels", big nudge 10.0/10.0, ruler origin 0.0/0.0 and grid 1.0/4 — the 16.16 fixed-point values decoding exactly — and refuses the serial number and title it cannot know, without faulting |

`displayPixels` and `colorServices` are done and described in
`docs/8bf-host.md`. `propertyProcs` remains.

## Settling a suite's member order

The technique, since it had to be invented twice and will be needed again
for stages 2 to 4:

Fill every slot of the suite with an interchangeable probe that logs the
arguments it was handed. Each routine in a suite is `extern "C"` and
passes its arguments in the same registers, so one probe can stand in for
any slot, and the argument *shape* says which routine the plug-in thought
it was calling — `(small int, pointer)` is an allocate, `(pointer, 0|1)`
is a lock, a bare pointer is a free or an unlock, nothing meaningful is a
space query. Then run a plug-in that uses the suite and read the log.

`SCHIST_8BF_BUFPROBE=1` does this for `BufferProcs`; `suites.rs` has the
machinery to point it at another.

Finding a plug-in that exercises the suite at all is the hard part. Both
plug-ins here had to be driven through their own UI before either touched
the buffer suite — a headless run never gets that far.

## Reproducing

`tools/verify-8bf.sh` downloads both plug-ins, cross-compiles the host to
both Windows targets, and runs discovery, the about box, a headless
G'MIC run, the Filter Foundry dialog on 64- and 32-bit, and a full G'MIC
round trip through its Qt UI — which is the only thing here that
exercises the buffer suite end to end. It needs `wine`, `Xvfb` and
`xdotool`, and prints whatever it skips.
