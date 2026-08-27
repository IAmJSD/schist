/* A minimal Photoshop filter plug-in, used as a test fixture.
 *
 * The FilterRecord below is declared independently of the Rust one, from
 * the same Adobe prose (API Guide table 63). Compiling it and comparing
 * `offsetof` against Rust's `offset_of!` is what pins down the layout
 * assumption the host rests on: that the record uses natural alignment
 * with no packing pragma.
 *
 * The filter itself inverts every plane. It exports two entry points so
 * both host drivers get exercised: `entry_advance` does its work inside
 * filterSelectorStart via AdvanceState, and `entry_continue` leaves
 * rectangles behind for the host to service between Continue calls.
 */

#include <stddef.h>
#include <stdint.h>
#include <string.h>

#ifdef _WIN32
#define EXPORT __declspec(dllexport)
#else
#define EXPORT __attribute__((visibility("default")))
#endif

typedef int16_t OSErr;
typedef uint32_t OSType;
typedef unsigned char MacBoolean;
typedef int32_t Fixed;
typedef unsigned char **Handle;

typedef struct { int16_t v, h; } Point;
typedef struct { int16_t top, left, bottom, right; } Rect;
typedef struct { uint16_t red, green, blue; } RGBColor;
typedef unsigned char FilterColor[4];
typedef struct {
    Fixed gamma, redX, redY, greenX, greenY, blueX, blueY, whiteX, whiteY, ambient;
} PlugInMonitor;

/* Buffer suite. Order and header values from API Guide chapter 3:
 * "Current version: 2; Routines: 5", over BufferSpace, AllocateBuffer,
 * FreeBuffer, LockBuffer, UnlockBuffer. Declared here independently so
 * the host's own ordering is checked against the documentation rather
 * than against itself. */
typedef void *BufferID;
typedef struct BufferProcs {
    int16_t bufferProcsVersion;
    int16_t numBufferProcs;
    int32_t (*spaceProc)(void);
    OSErr (*allocateProc)(int32_t size, BufferID *buffer);
    void (*freeProc)(BufferID buffer);
    void *(*lockProc)(BufferID buffer, MacBoolean moveHigh);
    void (*unlockProc)(BufferID buffer);
} BufferProcs;

typedef struct PlatformData {
    void *hwnd;
} PlatformData;

typedef struct HandleProcs {
    int16_t handleProcsVersion;
    int16_t numHandleProcs;
    Handle (*newProc)(int32_t size);
    void (*disposeProc)(Handle h);
    int32_t (*getSizeProc)(Handle h);
    OSErr (*setSizeProc)(Handle h, int32_t size);
    void *(*lockProc)(Handle h, MacBoolean moveHigh);
    void (*unlockProc)(Handle h);
    void (*recoverSpaceProc)(int32_t size);
    void (*disposeRegularHandleProc)(Handle h);
} HandleProcs;

/* FilterRecord — and only FilterRecord — is packed to four bytes, so a
 * pointer follows an int32 with no hole. Natural alignment makes the
 * record eight bytes longer by `platformData`, far enough that a real
 * plug-in reads a pointer out of the middle of the monitor record. The
 * callback suites below are *not* packed. Both halves of that were
 * established against shipping plug-ins; see docs/8bf-abi-provenance.md. */
#pragma pack(push, 4)
typedef struct FilterRecord {
    int32_t serialNumber;
    MacBoolean (*abortProc)(void);
    void (*progressProc)(int32_t done, int32_t total);
    Handle parameters;
    Point imageSize;
    int16_t planes;
    Rect filterRect;
    RGBColor background;
    RGBColor foreground;
    int32_t maxSpace;
    int32_t bufferSpace;
    Rect inRect;
    int16_t inLoPlane;
    int16_t inHiPlane;
    Rect outRect;
    int16_t outLoPlane;
    int16_t outHiPlane;
    void *inData;
    int32_t inRowBytes;
    void *outData;
    int32_t outRowBytes;
    MacBoolean isFloating;
    MacBoolean haveMask;
    MacBoolean autoMask;
    Rect maskRect;
    void *maskData;
    int32_t maskRowBytes;
    FilterColor backColor;
    FilterColor foreColor;
    OSType hostSig;
    void (*hostProc)(int16_t selector, void *data);
    int16_t imageMode;
    Fixed imageHRes;
    Fixed imageVRes;
    Point floatCoord;
    Point wholeSize;
    PlugInMonitor monitor;
    PlatformData *platformData;
    BufferProcs *bufferProcs;
    void *resourceProcs;
    void *processEvent;
    void *displayPixels;
    HandleProcs *handleProcs;

    /* new in 3.0 */
    MacBoolean supportsDummyPlanes;
    MacBoolean supportsAlternateLayouts;
    int16_t wantLayout;
    int16_t filterCase;
    int16_t dummyPlaneValue;
    void *premiereHook;
    OSErr (*advanceState)(void);
    MacBoolean supportsAbsolute;
    MacBoolean wantsAbsolute;
    void *getProperty;
    MacBoolean cannotUndo;
    MacBoolean supportsPadding;
    int16_t inputPadding;
    int16_t outputPadding;
    int16_t maskPadding;
    char samplingSupport;
    char reservedByte;
    Fixed inputRate;
    Fixed maskRate;
    void *colorServices;
    int16_t inLayerPlanes;
    int16_t inTransparencyMask;
    int16_t inLayerMasks;
    int16_t inInvertedLayerMasks;
    int16_t inNonLayerPlanes;
    int16_t outLayerPlanes;
    int16_t outTransparencyMask;
    int16_t outLayerMasks;
    int16_t outInvertedLayerMasks;
    int16_t outNonLayerPlanes;
    int16_t absLayerPlanes;
    int16_t absTransparencyMask;
    int16_t absLayerMasks;
    int16_t absInvertedLayerMasks;
    int16_t absNonLayerPlanes;
    int16_t inPreDummyPlanes;
    int16_t inPostDummyPlanes;
    int16_t outPreDummyPlanes;
    int16_t outPostDummyPlanes;
    int32_t inColumnBytes;
    int32_t inPlaneBytes;
    int32_t outColumnBytes;
    int32_t outPlaneBytes;

    /* new in 3.0.4 */
    void *imageServicesProcs;
    void *propertyProcs;
    int16_t inTileHeight;
    int16_t inTileWidth;
    Point inTileOrigin;
    int16_t absTileHeight;
    int16_t absTileWidth;
    Point absTileOrigin;
    int16_t outTileHeight;
    int16_t outTileWidth;
    Point outTileOrigin;
    int16_t maskTileHeight;
    int16_t maskTileWidth;
    Point maskTileOrigin;

    /* new in 4.0 */
    void *descriptorParameters;
    unsigned char *errorString;
    void *channelPortProcs;
    void *documentInfo;

    /* new in 5.0 */
    void *sSPBasic;
    void *plugInRef;
    int32_t depth;

    /* new in 6.0 */
    Handle iCCprofileData;
    int32_t iCCprofileSize;
    int32_t canUseICCProfiles;

    /* new in 7.0 */
    int32_t hasImageScrap;

    /* new in CS */
    void *bigDocumentData;
    char reserved[46];
} FilterRecord;
#pragma pack(pop)

/* ---- layout probe ---------------------------------------------------- */

EXPORT size_t probe_sizeof(void) { return sizeof(FilterRecord); }

/* Keep in step with FIELDS in tests/layout.rs. */
EXPORT size_t probe_offsets(size_t *out, size_t n) {
    static const size_t offs[] = {
        offsetof(FilterRecord, serialNumber),
        offsetof(FilterRecord, abortProc),
        offsetof(FilterRecord, parameters),
        offsetof(FilterRecord, imageSize),
        offsetof(FilterRecord, planes),
        offsetof(FilterRecord, filterRect),
        offsetof(FilterRecord, background),
        offsetof(FilterRecord, maxSpace),
        offsetof(FilterRecord, inRect),
        offsetof(FilterRecord, inData),
        offsetof(FilterRecord, outData),
        offsetof(FilterRecord, isFloating),
        offsetof(FilterRecord, maskRect),
        offsetof(FilterRecord, maskData),
        offsetof(FilterRecord, backColor),
        offsetof(FilterRecord, hostSig),
        offsetof(FilterRecord, imageMode),
        offsetof(FilterRecord, monitor),
        offsetof(FilterRecord, platformData),
        offsetof(FilterRecord, handleProcs),
        offsetof(FilterRecord, filterCase),
        offsetof(FilterRecord, advanceState),
        offsetof(FilterRecord, samplingSupport),
        offsetof(FilterRecord, inputRate),
        offsetof(FilterRecord, inLayerPlanes),
        offsetof(FilterRecord, inColumnBytes),
        offsetof(FilterRecord, imageServicesProcs),
        offsetof(FilterRecord, maskTileOrigin),
        offsetof(FilterRecord, descriptorParameters),
        offsetof(FilterRecord, errorString),
        offsetof(FilterRecord, sSPBasic),
        offsetof(FilterRecord, depth),
        offsetof(FilterRecord, iCCprofileData),
        offsetof(FilterRecord, hasImageScrap),
        offsetof(FilterRecord, bigDocumentData),
        offsetof(FilterRecord, reserved),
    };
    size_t count = sizeof(offs) / sizeof(offs[0]);
    if (n < count) return 0;
    memcpy(out, offs, sizeof(offs));
    return count;
}

/* ---- the filter ------------------------------------------------------ */

#define selectorAbout       0
#define selectorParameters  1
#define selectorPrepare     2
#define selectorStart       3
#define selectorContinue    4
#define selectorFinish      5

#define filterBadParameters (-30100)
#define filterBadMode       (-30101)

#define PARAM_SIG 0x53434831u  /* 'SCH1' */
#define TILE 32

typedef struct { uint32_t sig; int32_t amount; } Params;

/* Iteration state, kept in the host-provided `data` slot. */
typedef struct { int16_t nextTop, nextLeft; } Progress;

static void invert_tile(FilterRecord *fr, int32_t amount) {
    int planes = fr->inHiPlane - fr->inLoPlane + 1;
    int w = fr->inRect.right - fr->inRect.left;
    int h = fr->inRect.bottom - fr->inRect.top;
    for (int y = 0; y < h; y++) {
        const unsigned char *src = (const unsigned char *)fr->inData + (size_t)y * fr->inRowBytes;
        unsigned char *dst = (unsigned char *)fr->outData + (size_t)y * fr->outRowBytes;
        for (int i = 0; i < w * planes; i++) {
            int v = amount - src[i];
            dst[i] = (unsigned char)(v < 0 ? 0 : (v > 255 ? 255 : v));
        }
    }
}

/* Point the record at the next tile, or empty the rectangles when the
 * whole filterRect has been covered. Returns 1 while there is work. */
static int next_tile(FilterRecord *fr, Progress *p) {
    if (p->nextTop >= fr->filterRect.bottom) {
        fr->inRect.top = fr->inRect.left = fr->inRect.bottom = fr->inRect.right = 0;
        fr->outRect = fr->inRect;
        fr->maskRect = fr->inRect;
        return 0;
    }
    int16_t bottom = p->nextTop + TILE;
    int16_t right = p->nextLeft + TILE;
    if (bottom > fr->filterRect.bottom) bottom = fr->filterRect.bottom;
    if (right > fr->filterRect.right) right = fr->filterRect.right;

    fr->inRect.top = p->nextTop;
    fr->inRect.left = p->nextLeft;
    fr->inRect.bottom = bottom;
    fr->inRect.right = right;
    fr->outRect = fr->inRect;
    fr->inLoPlane = fr->outLoPlane = 0;
    fr->inHiPlane = fr->outHiPlane = (int16_t)(fr->planes - 1);

    p->nextLeft = right;
    if (p->nextLeft >= fr->filterRect.right) {
        p->nextLeft = fr->filterRect.left;
        p->nextTop = bottom;
    }
    return 1;
}

static OSErr ensure_params(FilterRecord *fr) {
    if (fr->parameters == NULL) {
        if (fr->handleProcs == NULL || fr->handleProcs->newProc == NULL)
            return filterBadParameters;
        fr->parameters = fr->handleProcs->newProc((int32_t)sizeof(Params));
        if (fr->parameters == NULL) return filterBadParameters;
        Params *p = (Params *)*fr->parameters;
        p->sig = PARAM_SIG;
        p->amount = 255;
    }
    return 0;
}

static int32_t param_amount(FilterRecord *fr) {
    if (fr->parameters == NULL) return 255;
    Params *p = (Params *)*fr->parameters;
    return p->sig == PARAM_SIG ? p->amount : 255;
}

/* Modes for `run`. */
#define RUN_ADVANCE  1
#define RUN_CONTINUE 0
#define RUN_FAIL     2

static void run(int16_t selector, FilterRecord *fr, intptr_t *data,
                int16_t *result, int use_advance) {
    *result = 0;
    switch (selector) {
    case selectorAbout:
        return;
    case selectorParameters:
        *result = ensure_params(fr);
        return;
    case selectorPrepare:
        fr->bufferSpace = 0;
        fr->maxSpace = 0;
        return;
    case selectorStart: {
        if (fr->imageMode != 3 /* RGBColor */ && fr->imageMode != 1 /* GrayScale */) {
            *result = filterBadMode;
            return;
        }
        if (fr->depth != 8) { *result = filterBadMode; return; }
        /* platformData points at a PlatformData, and is never the raw
         * window handle. Following it must be safe even when the host
         * has no window to offer. */
        if (fr->platformData == NULL) { *result = filterBadParameters; return; }
        (void)fr->platformData->hwnd;
        if (fr->parameters != NULL && ((Params *)*fr->parameters)->sig != PARAM_SIG) {
            *result = filterBadParameters;
            return;
        }
        Progress *p = (Progress *)data;
        p->nextTop = fr->filterRect.top;
        p->nextLeft = fr->filterRect.left;
        int32_t amount = param_amount(fr);

        if (use_advance == RUN_FAIL) {
            /* Filter two tiles, then give up — so the host has really
             * committed something by the time the run fails. */
            for (int i = 0; i < 2 && next_tile(fr, p); i++) {
                OSErr e = fr->advanceState();
                if (e != 0) { *result = e; return; }
                invert_tile(fr, amount);
            }
            *result = filterBadParameters;
            return;
        }
        if (use_advance) {
            if (fr->advanceState == NULL) { *result = filterBadParameters; return; }
            int32_t total = fr->filterRect.bottom - fr->filterRect.top;
            while (next_tile(fr, p)) {
                if (fr->abortProc && fr->abortProc()) { *result = -128; return; }
                OSErr e = fr->advanceState();
                if (e != 0) { *result = e; return; }
                invert_tile(fr, amount);
                if (fr->progressProc)
                    fr->progressProc(p->nextTop - fr->filterRect.top, total);
            }
        } else {
            next_tile(fr, p);
        }
        return;
    }
    case selectorContinue: {
        Progress *p = (Progress *)data;
        invert_tile(fr, param_amount(fr));
        next_tile(fr, p);
        return;
    }
    case selectorFinish:
        return;
    default:
        return;
    }
}

EXPORT void entry_advance(int16_t selector, void *pb, intptr_t *data, int16_t *result) {
    run(selector, (FilterRecord *)pb, data, result, RUN_ADVANCE);
}

EXPORT void entry_continue(int16_t selector, void *pb, intptr_t *data, int16_t *result) {
    run(selector, (FilterRecord *)pb, data, result, RUN_CONTINUE);
}

/* ---- padding ---------------------------------------------------------
 *
 * Ask for a rectangle that overhangs the image on every side, then copy
 * the padded buffer straight through. Whatever the host put in the
 * margin ends up in the output, where the test can check it.
 */
#define PAD 8

static void run_padding(int16_t selector, FilterRecord *fr, intptr_t *data,
                        int16_t *result, int16_t padval) {
    (void)data;
    *result = 0;
    if (selector != selectorStart) {
        if (selector == selectorContinue) {
            fr->inRect.top = fr->inRect.left = fr->inRect.bottom = fr->inRect.right = 0;
            fr->outRect = fr->inRect;
        }
        return;
    }
    if (fr->advanceState == NULL) { *result = filterBadParameters; return; }
    fr->inputPadding = padval;
    fr->inRect.top = (int16_t)(fr->filterRect.top - PAD);
    fr->inRect.left = (int16_t)(fr->filterRect.left - PAD);
    fr->inRect.bottom = (int16_t)(fr->filterRect.bottom + PAD);
    fr->inRect.right = (int16_t)(fr->filterRect.right + PAD);
    fr->outRect = fr->filterRect;
    fr->inLoPlane = fr->outLoPlane = 0;
    fr->inHiPlane = fr->outHiPlane = (int16_t)(fr->planes - 1);
    OSErr e = fr->advanceState();
    if (e != 0) { *result = e; return; }

    int planes = fr->planes;
    int w = fr->outRect.right - fr->outRect.left;
    int h = fr->outRect.bottom - fr->outRect.top;
    for (int y = 0; y < h; y++) {
        const unsigned char *src = (const unsigned char *)fr->inData + (size_t)y * fr->inRowBytes;
        unsigned char *dst = (unsigned char *)fr->outData + (size_t)y * fr->outRowBytes;
        memcpy(dst, src, (size_t)w * planes);
    }
    fr->inRect.top = fr->inRect.left = fr->inRect.bottom = fr->inRect.right = 0;
    fr->outRect = fr->inRect;
    fr->maskRect = fr->inRect;
}

EXPORT void entry_pad_replicate(int16_t selector, void *pb, intptr_t *data, int16_t *result) {
    run_padding(selector, (FilterRecord *)pb, data, result, -1);
}

EXPORT void entry_pad_fill(int16_t selector, void *pb, intptr_t *data, int16_t *result) {
    run_padding(selector, (FilterRecord *)pb, data, result, 200);
}

/* An undocumented negative: the host must still return usable pixels
 * rather than whatever the buffer happened to contain. */
EXPORT void entry_pad_unknown(int16_t selector, void *pb, intptr_t *data, int16_t *result) {
    run_padding(selector, (FilterRecord *)pb, data, result, -77);
}

/* ---- buffer suite ---------------------------------------------------- */

#define bufferBadVersion (-30110)
#define bufferBadRoutine (-30111)
#define bufferBadData    (-30112)

EXPORT void entry_buffers(int16_t selector, void *pb, intptr_t *data, int16_t *result) {
    FilterRecord *fr = (FilterRecord *)pb;
    (void)data;
    *result = 0;
    if (selector != selectorStart) return;

    BufferProcs *bp = fr->bufferProcs;
    if (bp == NULL) { *result = bufferBadRoutine; return; }
    if (bp->bufferProcsVersion != 2) { *result = bufferBadVersion; return; }
    if (bp->numBufferProcs < 5) { *result = bufferBadVersion; return; }
    if (!bp->spaceProc || !bp->allocateProc || !bp->freeProc ||
        !bp->lockProc || !bp->unlockProc) { *result = bufferBadRoutine; return; }

    if (bp->spaceProc() <= 0) { *result = bufferBadData; return; }

    BufferID b = NULL;
    if (bp->allocateProc(4096, &b) != 0 || b == NULL) { *result = bufferBadData; return; }
    unsigned char *p = (unsigned char *)bp->lockProc(b, 0);
    if (p == NULL) { *result = bufferBadData; return; }
    memset(p, 0x5a, 4096);
    for (int i = 0; i < 4096; i++)
        if (p[i] != 0x5a) { *result = bufferBadData; return; }
    bp->unlockProc(b);
    bp->freeProc(b);

    fr->inRect.top = fr->inRect.left = fr->inRect.bottom = fr->inRect.right = 0;
    fr->outRect = fr->inRect;
    fr->maskRect = fr->inRect;
}

/* ---- error reporting -------------------------------------------------- */

EXPORT void entry_error_string(int16_t selector, void *pb, intptr_t *data, int16_t *result) {
    FilterRecord *fr = (FilterRecord *)pb;
    (void)data;
    *result = 0;
    if (selector != selectorStart) return;
    if (fr->errorString == NULL) { *result = filterBadParameters; return; }
    static const char msg[] = "the fixture declined on purpose";
    unsigned char len = (unsigned char)(sizeof(msg) - 1);
    fr->errorString[0] = len;
    memcpy(&fr->errorString[1], msg, len);
    *result = -30902; /* errReportString, whatever its real value */
}

EXPORT void entry_fail_midway(int16_t selector, void *pb, intptr_t *data, int16_t *result) {
    run(selector, (FilterRecord *)pb, data, result, RUN_FAIL);
}
