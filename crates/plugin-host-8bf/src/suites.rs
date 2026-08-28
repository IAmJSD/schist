//! Host callback suites handed to the plug-in through `FilterRecord`.
//!
//! # Provenance and risk
//!
//! Adobe's API Guide never prints the suite structs, only each
//! routine's signature — but it documents the routines of a suite in a
//! fixed narrative order and heads each suite with its version and
//! routine count. For the Handle suite those are "version 1, routines 7"
//! and the seven appear as New, Dispose, GetSize, SetSize, Lock, Unlock,
//! RecoverSpace — which is exactly the order a real plug-in was observed
//! calling them in. That match is what licenses reading the same order
//! off the page for the Buffer suite, whose header reads "version 2,
//! routines 5" over Space, Allocate, Free, Lock, Unlock.
//!
//! Getting this wrong means the plug-in calls the wrong function
//! pointer, so every suite also carries its documented version/count
//! header: a plug-in that checks refuses rather than misbehaves.
//!
//! All allocation here is deliberately global-state-free. The ABI gives
//! callbacks no user-data parameter, so the usual trick is a global
//! registry; instead each block carries its own size in a header behind
//! the pointer the plug-in sees, which keeps the callbacks re-entrant
//! and thread-safe for free.

use crate::abi::{Handle, MacBoolean, OSErr, NO_ERR};
use std::alloc::{alloc, dealloc, Layout};
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set from `SCHIST_8BF_TRACE`. Every host callback logs its arguments,
/// which is how you find out what an uncooperative plug-in is actually
/// asking for — and the only way to check, from the outside, that a
/// suite's function pointers are in the order the plug-in expects.
static TRACE: AtomicBool = AtomicBool::new(false);

pub fn set_trace(on: bool) {
    TRACE.store(on, Ordering::Relaxed);
}

pub fn trace_enabled() -> bool {
    TRACE.load(Ordering::Relaxed)
}

/// Enable tracing if `SCHIST_8BF_TRACE` is set to anything but "0".
pub fn trace_from_env() {
    if let Ok(v) = std::env::var("SCHIST_8BF_TRACE") {
        set_trace(v != "0");
    }
}

macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::suites::trace_enabled() {
            eprintln!("[8bf] {}", format_args!($($arg)*));
        }
    };
}
pub(crate) use trace;

/// Bytes reserved ahead of every block for its own size. Sixteen keeps
/// the payload 16-byte aligned, which is at least as strict as anything
/// a plug-in can ask of a `Ptr`.
const HEADER: usize = 16;

fn layout_for(size: usize) -> Layout {
    Layout::from_size_align(size + HEADER, HEADER).expect("plausible allocation size")
}

/// Allocate `size` bytes, returning the payload pointer. The size is
/// stashed in the header so the matching free needs no bookkeeping.
unsafe fn block_alloc(size: usize) -> *mut u8 {
    let base = alloc(layout_for(size));
    if base.is_null() {
        return std::ptr::null_mut();
    }
    (base as *mut usize).write(size);
    base.add(HEADER)
}

unsafe fn block_size(payload: *mut u8) -> usize {
    (payload.sub(HEADER) as *mut usize).read()
}

unsafe fn block_free(payload: *mut u8) {
    if payload.is_null() {
        return;
    }
    let size = block_size(payload);
    dealloc(payload.sub(HEADER), layout_for(size));
}

// --- Handle suite -------------------------------------------------------
//
// A classic Mac `Handle` is a pointer to a *master pointer*, so `*h` is
// the block's data. Plug-ins written against the Mac idiom dereference
// directly instead of calling `lockProc`, so the master pointer has to
// be real and stable, not a token.

pub type NewPIHandleProc = unsafe extern "C" fn(size: i32) -> Handle;
pub type DisposePIHandleProc = unsafe extern "C" fn(h: Handle);
pub type GetPIHandleSizeProc = unsafe extern "C" fn(h: Handle) -> i32;
pub type SetPIHandleSizeProc = unsafe extern "C" fn(h: Handle, size: i32) -> OSErr;
pub type LockPIHandleProc = unsafe extern "C" fn(h: Handle, move_high: MacBoolean) -> *mut u8;
pub type UnlockPIHandleProc = unsafe extern "C" fn(h: Handle);
pub type RecoverSpaceProc = unsafe extern "C" fn(size: i32);

/// Member order and header values from API Guide chapter 3: "Handle
/// suite. Current version: 1; Adobe Photoshop: 5.0; Routines: 7".
///
/// `dispose_regular_handle_proc` is an eighth slot past the documented
/// seven, so the count stays at 7 and a plug-in that trusts it will
/// never reach the extra one. It is populated anyway: harmless if the
/// real struct has it, invisible if not.
#[repr(C)]
pub struct HandleProcs {
    pub handle_procs_version: i16,
    pub num_handle_procs: i16,
    pub new_proc: Option<NewPIHandleProc>,
    pub dispose_proc: Option<DisposePIHandleProc>,
    pub get_size_proc: Option<GetPIHandleSizeProc>,
    pub set_size_proc: Option<SetPIHandleSizeProc>,
    pub lock_proc: Option<LockPIHandleProc>,
    pub unlock_proc: Option<UnlockPIHandleProc>,
    pub recover_space_proc: Option<RecoverSpaceProc>,
    pub dispose_regular_handle_proc: Option<DisposePIHandleProc>,
}

/// Allocate a handle whose master pointer the plug-in may dereference.
pub(crate) unsafe extern "C" fn new_handle(size: i32) -> Handle {
    trace!("handle.new({size})");
    let Ok(size) = usize::try_from(size) else {
        return std::ptr::null_mut();
    };
    let data = block_alloc(size);
    if data.is_null() {
        return std::ptr::null_mut();
    }
    // The master pointer cell is itself a one-pointer block, so it can
    // be freed by the same path.
    let cell = block_alloc(std::mem::size_of::<*mut u8>()) as *mut *mut u8;
    if cell.is_null() {
        block_free(data);
        return std::ptr::null_mut();
    }
    cell.write(data);
    cell
}

pub(crate) unsafe extern "C" fn dispose_handle(h: Handle) {
    trace!("handle.dispose({h:p})");
    if h.is_null() {
        return;
    }
    block_free(h.read());
    block_free(h as *mut u8);
}

pub(crate) unsafe extern "C" fn get_handle_size(h: Handle) -> i32 {
    trace!("handle.get_size({h:p})");
    if h.is_null() {
        return 0;
    }
    i32::try_from(block_size(h.read())).unwrap_or(i32::MAX)
}

pub(crate) unsafe extern "C" fn set_handle_size(h: Handle, size: i32) -> OSErr {
    const MEM_FULL_ERR: OSErr = -108;
    trace!("handle.set_size({h:p}, {size})");
    if h.is_null() {
        return MEM_FULL_ERR;
    }
    let Ok(size) = usize::try_from(size) else {
        return MEM_FULL_ERR;
    };
    let old = h.read();
    let old_size = block_size(old);
    if size == old_size {
        return NO_ERR;
    }
    let new = block_alloc(size);
    if new.is_null() {
        return MEM_FULL_ERR;
    }
    std::ptr::copy_nonoverlapping(old, new, size.min(old_size));
    block_free(old);
    h.write(new);
    NO_ERR
}

/// Locking is a no-op: nothing here relocates. The Mac `moveHigh` flag
/// has no meaning off Mac OS, which the API Guide says explicitly.
pub(crate) unsafe extern "C" fn lock_handle(h: Handle, _move_high: MacBoolean) -> *mut u8 {
    trace!("handle.lock({h:p})");
    if h.is_null() {
        std::ptr::null_mut()
    } else {
        h.read()
    }
}

pub(crate) unsafe extern "C" fn unlock_handle(h: Handle) {
    trace!("handle.unlock({h:p})");
}

pub(crate) unsafe extern "C" fn recover_space(size: i32) {
    trace!("handle.recover_space({size})");
}

pub fn handle_procs() -> HandleProcs {
    HandleProcs {
        handle_procs_version: 1,
        num_handle_procs: 7,
        new_proc: Some(new_handle),
        dispose_proc: Some(dispose_handle),
        get_size_proc: Some(get_handle_size),
        set_size_proc: Some(set_handle_size),
        lock_proc: Some(lock_handle),
        unlock_proc: Some(unlock_handle),
        recover_space_proc: Some(recover_space),
        dispose_regular_handle_proc: Some(dispose_handle),
    }
}

// --- Buffer suite -------------------------------------------------------

/// "Buffers are identified by pointers to an opaque type called
/// `BufferID`" — API Guide chapter 3.
pub type BufferID = *mut c_void;

pub type AllocateBufferProc = unsafe extern "C" fn(size: i32, buffer: *mut BufferID) -> OSErr;
pub type LockBufferProc = unsafe extern "C" fn(b: BufferID, move_high: MacBoolean) -> *mut u8;
pub type UnlockBufferProc = unsafe extern "C" fn(b: BufferID);
pub type FreeBufferProc = unsafe extern "C" fn(b: BufferID);
pub type BufferSpaceProc = unsafe extern "C" fn() -> i32;

/// Version and routine count from API Guide chapter 3: "Buffer suite.
/// Current version: 2; Adobe Photoshop: 5.0; Routines: 5".
///
/// The member *order*, however, is **not** the order the guide
/// documents the routines in. The prose runs Space, Allocate, Free,
/// Lock, Unlock; the struct runs Allocate, Lock, Unlock, Free, Space.
/// That was settled by handing a real plug-in five interchangeable
/// probes, one per slot, and reading the argument registers: slot 0
/// arrived with `(3072, <stack pointer>)`, and 3072 is exactly one plane
/// of the image it was filtering — unmistakably
/// `AllocateBufferProc(size, &buffer)`.
///
/// This is worth dwelling on, because the Handle suite's narrative order
/// *does* match its struct order. One suite matching is not a rule, and
/// treating it as one put a wrong order in this file for a while. Only
/// the Handle suite's order is licensed by observation; this one is too,
/// separately, and neither licenses the other.
#[repr(C)]
pub struct BufferProcs {
    pub buffer_procs_version: i16,
    pub num_buffer_procs: i16,
    pub allocate_proc: Option<AllocateBufferProc>,
    pub lock_proc: Option<LockBufferProc>,
    pub unlock_proc: Option<UnlockBufferProc>,
    pub free_proc: Option<FreeBufferProc>,
    pub space_proc: Option<BufferSpaceProc>,
}

/// What `BufferSpaceProc` reports. The suite exists so a plug-in can
/// stop asking the host to account for its memory; there is no useful
/// number to give it, and quoting a large one is what lets filters that
/// scale their work to available space behave normally.
const REPORTED_BUFFER_SPACE: i32 = 256 * 1024 * 1024;

pub(crate) unsafe extern "C" fn allocate_buffer(size: i32, buffer: *mut BufferID) -> OSErr {
    const MEM_FULL_ERR: OSErr = -108;
    trace!("buffer.allocate({size})");
    if buffer.is_null() {
        return MEM_FULL_ERR;
    }
    let Ok(size) = usize::try_from(size) else {
        return MEM_FULL_ERR;
    };
    let p = block_alloc(size);
    if p.is_null() {
        buffer.write(std::ptr::null_mut());
        return MEM_FULL_ERR;
    }
    buffer.write(p as BufferID);
    NO_ERR
}

pub(crate) unsafe extern "C" fn lock_buffer(b: BufferID, _move_high: MacBoolean) -> *mut u8 {
    trace!("buffer.lock({b:p})");
    b as *mut u8
}

pub(crate) unsafe extern "C" fn unlock_buffer(b: BufferID) {
    trace!("buffer.unlock({b:p})");
}

pub(crate) unsafe extern "C" fn free_buffer(b: BufferID) {
    trace!("buffer.free({b:p})");
    block_free(b as *mut u8);
}

pub(crate) unsafe extern "C" fn buffer_space() -> i32 {
    trace!("buffer.space()");
    REPORTED_BUFFER_SPACE
}

/// Five interchangeable probes, one per slot of [`BufferProcs`], each
/// logging the arguments it was handed. Enabled with
/// `SCHIST_8BF_BUFPROBE`.
///
/// This is how the member order of a suite gets settled, and it is worth
/// keeping. Every routine in a suite is `extern "C"` and takes its
/// arguments in the same registers, so one probe can stand in for any
/// slot; the argument *shape* then says which routine the plug-in
/// thought it was calling. `(small int, pointer)` is Allocate,
/// `(pointer, 0|1)` is Lock, a bare pointer is Free or Unlock, and no
/// meaningful arguments at all is Space.
///
/// It earned its place: it caught this file claiming an order taken from
/// the order the API Guide's prose introduces the routines in, which for
/// this suite is not the order of the struct.
unsafe extern "C" fn probe_slot(slot: u64, a: u64, b: u64, c: u64) -> u64 {
    trace!("bufslot {slot}: arg1={a:#x} arg2={b:#x} arg3={c:#x}");
    0
}

macro_rules! slot_probe {
    ($name:ident, $n:literal) => {
        unsafe extern "C" fn $name(a: u64, b: u64, c: u64) -> u64 {
            probe_slot($n, a, b, c)
        }
    };
}
slot_probe!(probe0, 0);
slot_probe!(probe1, 1);
slot_probe!(probe2, 2);
slot_probe!(probe3, 3);
slot_probe!(probe4, 4);

fn probing_buffer_procs() -> BufferProcs {
    // SAFETY: every one of these fn types is extern "C" and passes its
    // arguments in the same registers, so reading them through a wider
    // signature is sound on the targets this host runs on. The probes
    // return 0, so a plug-in that actually uses the suite while probing
    // will misbehave — which is the point, and why this is off unless
    // asked for.
    unsafe {
        BufferProcs {
            buffer_procs_version: 2,
            num_buffer_procs: 5,
            allocate_proc: Some(std::mem::transmute::<*const (), AllocateBufferProc>(
                probe0 as *const (),
            )),
            lock_proc: Some(std::mem::transmute::<*const (), LockBufferProc>(
                probe1 as *const (),
            )),
            unlock_proc: Some(std::mem::transmute::<*const (), UnlockBufferProc>(
                probe2 as *const (),
            )),
            free_proc: Some(std::mem::transmute::<*const (), FreeBufferProc>(
                probe3 as *const (),
            )),
            space_proc: Some(std::mem::transmute::<*const (), BufferSpaceProc>(
                probe4 as *const (),
            )),
        }
    }
}

pub fn buffer_procs() -> BufferProcs {
    if std::env::var("SCHIST_8BF_BUFPROBE").is_ok() {
        return probing_buffer_procs();
    }
    BufferProcs {
        buffer_procs_version: 2,
        num_buffer_procs: 5,
        allocate_proc: Some(allocate_buffer),
        lock_proc: Some(lock_buffer),
        unlock_proc: Some(unlock_buffer),
        free_proc: Some(free_buffer),
        space_proc: Some(buffer_space),
    }
}

// --- PICA handle suite --------------------------------------------------
//
// The PICA twin of `HandleProcs`, acquired by name rather than reached
// through the record. Order and count from API Guide chapter 4: "Suite
// PEA Handle suite. Current version: 1; Adobe Photoshop: 5.0; Routines:
// 6", over New, Dispose, SetLock, GetSize, SetSize, RecoverSpace.
//
// PICA suites carry no version/count header of their own — the version
// is the one the plug-in passed to `AcquireSuite`.

/// `MACPASCAL Ptr (*SetPIHandleLockProc)(Handle h, Boolean lock,
/// Ptr *address, Boolean *oldLock)`.
pub type SetPIHandleLockProc = unsafe extern "C" fn(
    h: Handle,
    lock: MacBoolean,
    address: *mut *mut u8,
    old_lock: *mut MacBoolean,
) -> *mut u8;

#[repr(C)]
pub struct PicaHandleSuite {
    pub new_proc: Option<NewPIHandleProc>,
    pub dispose_proc: Option<DisposePIHandleProc>,
    pub set_lock_proc: Option<SetPIHandleLockProc>,
    pub get_size_proc: Option<GetPIHandleSizeProc>,
    pub set_size_proc: Option<SetPIHandleSizeProc>,
    pub recover_space_proc: Option<RecoverSpaceProc>,
}

// SAFETY: a struct of plain function pointers, immutable for the life of
// the process.
unsafe impl Sync for PicaHandleSuite {}

/// Nothing here relocates, so locking only has to report the address.
pub(crate) unsafe extern "C" fn set_handle_lock(
    h: Handle,
    lock: MacBoolean,
    address: *mut *mut u8,
    old_lock: *mut MacBoolean,
) -> *mut u8 {
    trace!("pica.handle.set_lock({h:p}, lock={lock})");
    let p = if h.is_null() {
        std::ptr::null_mut()
    } else {
        h.read()
    };
    if !address.is_null() {
        address.write(p);
    }
    if !old_lock.is_null() {
        // Always locked, because nothing here ever moves.
        old_lock.write(1);
    }
    p
}

static PICA_HANDLE_SUITE: PicaHandleSuite = PicaHandleSuite {
    new_proc: Some(new_handle),
    dispose_proc: Some(dispose_handle),
    set_lock_proc: Some(set_handle_lock),
    get_size_proc: Some(get_handle_size),
    set_size_proc: Some(set_handle_size),
    recover_space_proc: Some(recover_space),
};

/// The name a plug-in passes to `AcquireSuite` to ask for it.
pub const PICA_HANDLE_SUITE_NAME: &str = "Photoshop Handle Suite for Plug-ins";
/// The only version documented, and so the only one served.
pub const PICA_HANDLE_SUITE_VERSION: i32 = 1;

// --- PICA buffer suite --------------------------------------------------
//
// The PICA twin of `BufferProcs`, and a different shape: it hands back
// raw pointers rather than opaque ids, and asks for a size range rather
// than a size. Order and count from API Guide chapter 4: "Suite PEA
// Buffer suite. Current version: 1; Adobe Photoshop: 5.0; Routines: 4",
// over New, Dispose, GetSize, GetSpace.
//
// Serving this is not speculative: a real plug-in asks for it by name.

/// `Ptr (*BufferNewProc)(size_t *pRequestedSize, size_t minimumSize)`.
pub type BufferNewProc = unsafe extern "C" fn(requested: *mut usize, minimum: usize) -> *mut u8;
/// `void (*BufferDisposeProc)(Ptr *ppBuffer)`.
pub type BufferDisposeProc = unsafe extern "C" fn(buffer: *mut *mut u8);
/// `size_t (*BufferGetSizeProc)(Ptr pBuffer)`.
pub type BufferGetSizeProc = unsafe extern "C" fn(buffer: *mut u8) -> usize;
/// `size_t (*BufferGetSpaceProc)(void)`.
pub type BufferGetSpaceProc = unsafe extern "C" fn() -> usize;

#[repr(C)]
pub struct PicaBufferSuite {
    pub new_proc: Option<BufferNewProc>,
    pub dispose_proc: Option<BufferDisposeProc>,
    pub get_size_proc: Option<BufferGetSizeProc>,
    pub get_space_proc: Option<BufferGetSpaceProc>,
}

// SAFETY: plain function pointers, immutable for the life of the process.
unsafe impl Sync for PicaBufferSuite {}

/// Allocate `*requested` bytes, or the largest amount above `minimum`
/// that will fit, writing back what was actually taken. A null
/// `requested` means "exactly `minimum`, or fail".
pub(crate) unsafe extern "C" fn pica_buffer_new(requested: *mut usize, minimum: usize) -> *mut u8 {
    let want = if requested.is_null() {
        minimum
    } else {
        requested.read().max(minimum)
    };
    trace!("pica.buffer.new(requested={want}, minimum={minimum})");
    let p = block_alloc(want);
    if p.is_null() {
        // The contract is to fall back to anything at or above the
        // minimum before giving up.
        if want > minimum {
            let p = block_alloc(minimum);
            if !p.is_null() {
                if !requested.is_null() {
                    requested.write(minimum);
                }
                return p;
            }
        }
        return std::ptr::null_mut();
    }
    if !requested.is_null() {
        requested.write(want);
    }
    p
}

pub(crate) unsafe extern "C" fn pica_buffer_dispose(buffer: *mut *mut u8) {
    if buffer.is_null() {
        return;
    }
    let p = buffer.read();
    trace!("pica.buffer.dispose({p:p})");
    // "Does nothing if the buffer pointer is already NULL", and sets the
    // caller's variable to NULL afterwards.
    if !p.is_null() {
        block_free(p);
        buffer.write(std::ptr::null_mut());
    }
}

pub(crate) unsafe extern "C" fn pica_buffer_get_size(buffer: *mut u8) -> usize {
    if buffer.is_null() {
        return 0;
    }
    block_size(buffer)
}

pub(crate) unsafe extern "C" fn pica_buffer_get_space() -> usize {
    REPORTED_BUFFER_SPACE as usize
}

static PICA_BUFFER_SUITE: PicaBufferSuite = PicaBufferSuite {
    new_proc: Some(pica_buffer_new),
    dispose_proc: Some(pica_buffer_dispose),
    get_size_proc: Some(pica_buffer_get_size),
    get_space_proc: Some(pica_buffer_get_space),
};

pub const PICA_BUFFER_SUITE_NAME: &str = "Photoshop Buffer Suite for Plug-ins";
pub const PICA_BUFFER_SUITE_VERSION: i32 = 1;

// --- PICA basic suite ---------------------------------------------------

pub type AcquireSuiteProc =
    unsafe extern "C" fn(name: *const c_char, version: i32, suite: *mut *const c_void) -> i32;
pub type ReleaseSuiteProc = unsafe extern "C" fn(name: *const c_char, version: i32) -> i32;
pub type IsEqualProc = unsafe extern "C" fn(a: *const c_char, b: *const c_char) -> u8;
pub type AllocateBlockProc = unsafe extern "C" fn(size: usize, block: *mut *mut c_void) -> i32;
pub type FreeBlockProc = unsafe extern "C" fn(block: *mut c_void) -> i32;
pub type ReallocateBlockProc =
    unsafe extern "C" fn(block: *mut c_void, size: usize, out: *mut *mut c_void) -> i32;
pub type UndefinedProc = unsafe extern "C" fn() -> i32;

#[repr(C)]
pub struct SPBasicSuite {
    pub acquire_suite: Option<AcquireSuiteProc>,
    pub release_suite: Option<ReleaseSuiteProc>,
    pub is_equal: Option<IsEqualProc>,
    pub allocate_block: Option<AllocateBlockProc>,
    pub free_block: Option<FreeBlockProc>,
    pub reallocate_block: Option<ReallocateBlockProc>,
    pub undefined: Option<UndefinedProc>,
}

/// PICA's "no such suite". A plug-in that gets this is expected to fall
/// back to the direct `FilterRecord` callbacks, which is exactly what
/// this host serves.
const SP_SUITE_NOT_FOUND: i32 = -1;

pub(crate) unsafe extern "C" fn acquire_suite(
    name: *const c_char,
    version: i32,
    suite: *mut *const c_void,
) -> i32 {
    let wanted = cstr(name);
    trace!("pica.acquire_suite({wanted:?}, {version})");
    if suite.is_null() {
        return SP_SUITE_NOT_FOUND;
    }
    if wanted == PICA_HANDLE_SUITE_NAME && version == PICA_HANDLE_SUITE_VERSION {
        suite.write(&PICA_HANDLE_SUITE as *const _ as *const c_void);
        trace!("   -> served the handle suite");
        return NO_ERR as i32;
    }
    if wanted == PICA_BUFFER_SUITE_NAME && version == PICA_BUFFER_SUITE_VERSION {
        suite.write(&PICA_BUFFER_SUITE as *const _ as *const c_void);
        trace!("   -> served the buffer suite");
        return NO_ERR as i32;
    }
    // Everything else is genuinely absent, and saying so is what makes a
    // plug-in take its compatible path instead of misreading a zero.
    suite.write(std::ptr::null());
    SP_SUITE_NOT_FOUND
}

pub(crate) unsafe extern "C" fn release_suite(name: *const c_char, version: i32) -> i32 {
    trace!("pica.release_suite({:?}, {version})", cstr(name));
    NO_ERR as i32
}

/// Read a plug-in-supplied C string for tracing, tolerating null.
unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return "<null>".into();
    }
    CStr::from_ptr(p).to_string_lossy().into_owned()
}

pub(crate) unsafe extern "C" fn is_equal(a: *const c_char, b: *const c_char) -> u8 {
    // Logged as raw pointers rather than as strings on purpose. If the
    // member order of this suite is ever wrong, this slot receives
    // something that is not a string, and a trace that dereferenced it
    // would turn a diagnosable mistake into a crash.
    trace!("pica.is_equal({a:p}, {b:p})");
    if a.is_null() || b.is_null() {
        return u8::from(a == b);
    }
    u8::from(CStr::from_ptr(a) == CStr::from_ptr(b))
}

pub(crate) unsafe extern "C" fn allocate_block(size: usize, block: *mut *mut c_void) -> i32 {
    const SP_OUT_OF_MEMORY: i32 = -2;
    // Every argument, raw: if the slot order is wrong this is where it
    // shows, as a "size" that is obviously a pointer or vice versa.
    trace!("pica.allocate_block(size={size} out={block:p})");
    if block.is_null() {
        return SP_OUT_OF_MEMORY;
    }
    let p = block_alloc(size);
    block.write(p as *mut c_void);
    if p.is_null() {
        SP_OUT_OF_MEMORY
    } else {
        NO_ERR as i32
    }
}

pub(crate) unsafe extern "C" fn free_block(block: *mut c_void) -> i32 {
    trace!("pica.free_block({block:p})");
    block_free(block as *mut u8);
    NO_ERR as i32
}

pub(crate) unsafe extern "C" fn reallocate_block(
    block: *mut c_void,
    size: usize,
    out: *mut *mut c_void,
) -> i32 {
    const SP_OUT_OF_MEMORY: i32 = -2;
    trace!("pica.reallocate_block({block:p}, {size})");
    if out.is_null() {
        return SP_OUT_OF_MEMORY;
    }
    let fresh = block_alloc(size);
    if fresh.is_null() {
        out.write(std::ptr::null_mut());
        return SP_OUT_OF_MEMORY;
    }
    if !block.is_null() {
        let old = block as *mut u8;
        let n = block_size(old).min(size);
        std::ptr::copy_nonoverlapping(old, fresh, n);
        block_free(old);
    }
    out.write(fresh as *mut c_void);
    NO_ERR as i32
}

pub(crate) unsafe extern "C" fn undefined() -> i32 {
    trace!("pica.undefined()");
    SP_SUITE_NOT_FOUND
}

/// Five interchangeable probes for the slots of [`SPBasicSuite`] past
/// the two that are confirmed, enabled with `SCHIST_8BF_SPPROBE`.
///
/// `AcquireSuite` and `ReleaseSuite` stay real: both are confirmed by
/// position — a plug-in was observed acquiring a suite by name through
/// the first and releasing it through the second — and leaving them
/// working means a plug-in still gets far enough to reach the rest.
///
/// The argument shape says which routine the plug-in thought it was
/// calling: two pointers is `IsEqual`, `(small int, pointer)` is
/// `AllocateBlock`, a bare pointer is `FreeBlock`,
/// `(pointer, small int, pointer)` is `ReallocateBlock`, and nothing
/// meaningful is `Undefined`.
///
/// This exists because the member order past slot 1 is the last
/// unverified thing in the ABI, and the Buffer suite has already shown
/// once that guessing it from the documentation's prose order is wrong.
unsafe extern "C" fn sp_probe(slot: u64, a: u64, b: u64, c: u64) -> u64 {
    trace!("spslot {slot}: arg1={a:#x} arg2={b:#x} arg3={c:#x}");
    0
}

macro_rules! sp_slot_probe {
    ($name:ident, $n:literal) => {
        unsafe extern "C" fn $name(a: u64, b: u64, c: u64) -> u64 {
            sp_probe($n, a, b, c)
        }
    };
}
sp_slot_probe!(sp_probe2, 2);
sp_slot_probe!(sp_probe3, 3);
sp_slot_probe!(sp_probe4, 4);
sp_slot_probe!(sp_probe5, 5);
sp_slot_probe!(sp_probe6, 6);

fn probing_sp_basic_suite() -> SPBasicSuite {
    // SAFETY: as for the buffer probes — every one of these fn types is
    // extern "C" and passes its arguments in the same registers.
    unsafe {
        SPBasicSuite {
            acquire_suite: Some(acquire_suite),
            release_suite: Some(release_suite),
            is_equal: Some(std::mem::transmute::<*const (), IsEqualProc>(
                sp_probe2 as *const (),
            )),
            allocate_block: Some(std::mem::transmute::<*const (), AllocateBlockProc>(
                sp_probe3 as *const (),
            )),
            free_block: Some(std::mem::transmute::<*const (), FreeBlockProc>(
                sp_probe4 as *const (),
            )),
            reallocate_block: Some(std::mem::transmute::<*const (), ReallocateBlockProc>(
                sp_probe5 as *const (),
            )),
            undefined: Some(std::mem::transmute::<*const (), UndefinedProc>(
                sp_probe6 as *const (),
            )),
        }
    }
}

pub fn sp_basic_suite() -> SPBasicSuite {
    if std::env::var("SCHIST_8BF_SPPROBE").is_ok() {
        return probing_sp_basic_suite();
    }
    SPBasicSuite {
        acquire_suite: Some(acquire_suite),
        release_suite: Some(release_suite),
        is_equal: Some(is_equal),
        allocate_block: Some(allocate_block),
        free_block: Some(free_block),
        reallocate_block: Some(reallocate_block),
        undefined: Some(undefined),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_round_trip_through_the_master_pointer() {
        unsafe {
            let h = new_handle(64);
            assert!(!h.is_null());
            assert_eq!(get_handle_size(h), 64);
            // What a Mac-idiom plug-in does: dereference, don't lock.
            let data = h.read();
            assert_eq!(lock_handle(h, 0), data);
            data.write_bytes(0xab, 64);

            assert_eq!(set_handle_size(h, 128), NO_ERR);
            assert_eq!(get_handle_size(h), 128);
            let grown = h.read();
            assert_eq!(std::slice::from_raw_parts(grown, 64), [0xab; 64]);

            dispose_handle(h);
        }
    }

    #[test]
    fn buffers_allocate_lock_and_free() {
        unsafe {
            let mut id: BufferID = std::ptr::null_mut();
            assert_eq!(allocate_buffer(1024, &mut id), NO_ERR);
            assert!(!id.is_null());
            let p = lock_buffer(id, 0);
            p.write_bytes(7, 1024);
            assert_eq!(*p.add(1023), 7);
            unlock_buffer(id);
            free_buffer(id);
        }
    }

    #[test]
    fn pica_blocks_grow_without_losing_their_contents() {
        unsafe {
            let mut b: *mut c_void = std::ptr::null_mut();
            assert_eq!(allocate_block(16, &mut b), 0);
            (b as *mut u8).write_bytes(0x5a, 16);
            let mut grown: *mut c_void = std::ptr::null_mut();
            assert_eq!(reallocate_block(b, 64, &mut grown), 0);
            assert_eq!(std::slice::from_raw_parts(grown as *mut u8, 16), [0x5a; 16]);
            assert_eq!(free_block(grown), 0);
        }
    }

    #[test]
    fn acquire_suite_reports_not_found_and_nulls_the_out_pointer() {
        unsafe {
            let mut suite: *const c_void = std::ptr::dangling::<c_void>();
            let name = c"Photoshop Action Descriptor Suite";
            assert_eq!(acquire_suite(name.as_ptr(), 2, &mut suite), -1);
            assert!(suite.is_null());
        }
    }

    #[test]
    fn the_pica_buffer_suite_is_served_and_reports_what_it_gave() {
        unsafe {
            let name = c"Photoshop Buffer Suite for Plug-ins";
            let mut suite: *const c_void = std::ptr::null();
            assert_eq!(acquire_suite(name.as_ptr(), 1, &mut suite), 0);
            let s = &*(suite as *const PicaBufferSuite);

            // A null requested-size means "exactly the minimum".
            let p = (s.new_proc.unwrap())(std::ptr::null_mut(), 512);
            assert!(!p.is_null());
            assert_eq!((s.get_size_proc.unwrap())(p), 512);
            p.write_bytes(0x33, 512);
            let mut held = p;
            (s.dispose_proc.unwrap())(&mut held);
            assert!(held.is_null(), "dispose must null the caller's pointer");

            // Otherwise it takes what was asked for and says so.
            let mut want = 4096usize;
            let p = (s.new_proc.unwrap())(&mut want, 64);
            assert!(!p.is_null());
            assert_eq!(want, 4096);
            assert_eq!((s.get_size_proc.unwrap())(p), 4096);
            let mut held = p;
            (s.dispose_proc.unwrap())(&mut held);

            // Disposing an already-null pointer "does nothing".
            let mut none: *mut u8 = std::ptr::null_mut();
            (s.dispose_proc.unwrap())(&mut none);
            assert_eq!((s.get_size_proc.unwrap())(std::ptr::null_mut()), 0);
            assert!((s.get_space_proc.unwrap())() > 0);
        }
    }

    #[test]
    fn the_pica_handle_suite_is_served_by_name_and_version() {
        unsafe {
            let name = c"Photoshop Handle Suite for Plug-ins";
            let mut suite: *const c_void = std::ptr::null();
            assert_eq!(acquire_suite(name.as_ptr(), 1, &mut suite), 0);
            assert!(!suite.is_null());

            // A version this host does not implement is refused rather
            // than served something of the wrong shape.
            let mut other: *const c_void = std::ptr::dangling::<c_void>();
            assert_eq!(acquire_suite(name.as_ptr(), 2, &mut other), -1);
            assert!(other.is_null());

            let s = &*(suite as *const PicaHandleSuite);
            let h = (s.new_proc.unwrap())(32);
            assert!(!h.is_null());
            assert_eq!((s.get_size_proc.unwrap())(h), 32);

            let mut address: *mut u8 = std::ptr::null_mut();
            let mut old_lock: MacBoolean = 0;
            let p = (s.set_lock_proc.unwrap())(h, 1, &mut address, &mut old_lock);
            assert_eq!(p, h.read());
            assert_eq!(address, h.read());

            (s.dispose_proc.unwrap())(h);
        }
    }
}
