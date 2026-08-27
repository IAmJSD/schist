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

## Unverified — best-known values, tagged `UNVERIFIED` in the source

Ranked by how much damage a wrong guess does. Each is validated the same
way: run a real, known-good plug-in and see.

| # | Fact | Why it is a guess | Failure mode if wrong |
|---|---|---|---|
| 1 | **Member order inside `HandleProcs`, `BufferProcs` and `SPBasicSuite`** | The API Guide prints each routine's signature but never the suite structs | The plug-in calls the wrong function pointer. Loud crash if the signatures differ, silent corruption if they happen to match. Every suite carries its documented `version`/`count` header so a plug-in that checks refuses instead |
| 2 | `FilterRecord` uses **natural alignment**, no packing pragma | The prose states no packing rule; the explicit `reservedByte` "(for alignment)" is consistent with it | Every field past the first mismatch is garbage. Mitigated: `tests/layout.rs` cross-checks Rust's `repr(C)` against a C compiler's `offsetof` over the same declaration, so at least the two sides here agree |
| 3 | `filterSelector*` numeric values 0..5 | The guide names them and fixes their order, prints no numbers | Wrong selector runs, or none does |
| 4 | `plugInMode*` numeric values start at 0 | Only the ordering is documented | Plug-in rejects the mode, or misreads the pixels |
| 5 | Padding constants `-1` / `-2` / `-3` | The guide names all four options and says the error case is the default; 0..255 is documented as a literal fill, so the named modes must be negative | Out-of-bounds requests padded wrongly, or refused when they should not be |
| 6 | `FilterColor` is four bytes | Described only as "in the color space native to the image" | Shifts every field after `back_color`. Covered by test #2's cross-check only in the sense that both sides guess alike |
| 7 | `errReportString` = -30902, `userCanceledErr` = -128 | Named, not numbered, in the guide | An error is reported as a different error |
| 8 | The `'8664'` PiPL key for the Win64 entry point | The 1999 Resource Guide predates x86-64 and documents only `'wx86'` | 64-bit plug-ins look like they carry no code |
| 9 | The `AboutRecord` layout | Referred to as "declared in `PIAbout.h`", not printed | About box misbehaves; it is the only selector this affects |
| 10 | `HostProc` signature | Named, not printed. Passed as null, so unused | None while it stays null |
| 11 | Whether a Windows PiPL resource has a prelude before `version` | Not stated; `CNVTPIPL.EXE` is documented to "handle padding and byte-ordering issues for you" without saying what it emits | Parse fails. Mitigated: `Pipl::parse` tries offsets 0, 2 and 4 and accepts the framing whose first property carries `'8BIM'` |
| 12 | `handleProcsVersion` / `numHandleProcs` values | Not printed | A plug-in that version-checks refuses to run |

## Validating

None of the above can be settled on Linux. The check is:

1. On Windows, point `examples/8bf.rs` at a known-good freeware `.8bf`.
2. `inspect` first — that exercises facts 8 and 11 with no code running.
3. `apply` with `--no-dialog` — that exercises 3, 4, 5 and, if the
   plug-in stores parameters, 1.
4. A plug-in that shows a dialog exercises the rest.

Until that has happened this crate should be treated as unproven against
anything but its own test fixture.
