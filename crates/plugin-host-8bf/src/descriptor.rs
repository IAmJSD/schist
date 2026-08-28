//! Scripting: recording a filter's parameters and playing them back.
//!
//! This is what makes Last Filter remember its settings and what lets a
//! filter be recorded into an action. The plug-in writes its parameters
//! at `filterSelectorFinish` through the write suite, and reads them at
//! `filterSelectorStart` through the read suite; the host holds the
//! descriptor in between.
//!
//! # Provenance and risk
//!
//! Every routine's signature is from API Guide chapter 3, which prints
//! them. The **member order of the two suites is not printed anywhere**,
//! and here the guide lists the routines alphabetically after Open and
//! Close — which is a documentation convention, not necessarily a struct
//! layout. The Buffer suite already proved once that reading an order
//! off the page gets it wrong.
//!
//! So the order below is a starting point to be *checked*, with
//! `SCHIST_8BF_DESCPROBE` for checking it, and a plug-in that reaches
//! for a slot this host has in the wrong place is the thing to watch
//! for. Both suites carry the documented version and count, so a plug-in
//! that checks those refuses rather than misbehaves.

use crate::abi::{Handle, MacBoolean, OSErr, OSType, NO_ERR};
use std::ffi::c_void;

/// A key in a descriptor — an `OSType` naming one parameter.
pub type DescriptorKeyID = OSType;
/// The type of a value, also an `OSType`.
pub type DescType = OSType;
/// A unit for [`Value::UnitFloat`] — pixels, percent, angle and so on.
pub type DescriptorUnitID = OSType;

/// Classic Mac `paramErr`, which these return for a request that makes
/// no sense.
const PARAM_ERR: OSErr = -50;
/// What `GetKey` returns when there are no more keys.
const NO_MORE: MacBoolean = 0;

/// One parameter's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Integer(i32),
    Float(f64),
    UnitFloat {
        unit: DescriptorUnitID,
        value: f64,
    },
    Boolean(bool),
    /// A Pascal string, as `PutString` supplies it.
    Text(String),
    Enumerated {
        kind: DescType,
        value: DescType,
    },
    Class(DescType),
    Count(u32),
    Alias(Vec<u8>),
    /// A nested descriptor, which is how a parameter with structure is
    /// recorded.
    Object {
        kind: DescType,
        fields: Descriptor,
    },
    /// A reference to something in the document. Kept whole rather than
    /// interpreted; this host has nothing to resolve it against.
    Reference(Vec<u8>),
}

impl Value {
    /// The `DescType` a plug-in is told before it reads the value, so it
    /// knows which `Get` to call.
    pub fn desc_type(&self) -> DescType {
        use crate::abi::fourcc;
        match self {
            Value::Integer(_) => fourcc(b"long"),
            Value::Float(_) => fourcc(b"doub"),
            Value::UnitFloat { .. } => fourcc(b"UntF"),
            Value::Boolean(_) => fourcc(b"bool"),
            Value::Text(_) => fourcc(b"TEXT"),
            Value::Enumerated { .. } => fourcc(b"enum"),
            Value::Class(_) => fourcc(b"type"),
            Value::Count(_) => fourcc(b"long"),
            Value::Alias(_) => fourcc(b"alis"),
            Value::Object { .. } => fourcc(b"objc"),
            Value::Reference(_) => fourcc(b"obj "),
        }
    }
}

/// A filter's recorded parameters: keys in the order they were written.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Descriptor {
    pub entries: Vec<(DescriptorKeyID, Value)>,
}

impl Descriptor {
    pub fn get(&self, key: DescriptorKeyID) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    pub fn put(&mut self, key: DescriptorKeyID, value: Value) {
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.entries.push((key, value)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// --- the handles a plug-in holds ----------------------------------------
//
// `PIDescriptorHandle` is a `Handle` whose block holds a pointer to a
// `Descriptor` this host owns. The plug-in only ever passes it back.

/// Wrap a descriptor in a handle to give to a plug-in.
///
/// # Safety
///
/// The handle must later be freed with [`take_handle`], and not by the
/// plug-in's own means.
pub unsafe fn make_handle(d: Descriptor) -> Handle {
    let boxed = Box::into_raw(Box::new(d));
    let h = crate::suites::new_handle(std::mem::size_of::<*mut Descriptor>() as i32);
    if h.is_null() {
        drop(Box::from_raw(boxed));
        return std::ptr::null_mut();
    }
    (h.read() as *mut *mut Descriptor).write(boxed);
    h
}

/// Read a descriptor back out of a handle without taking ownership.
///
/// # Safety
///
/// `h` is null or a handle from [`make_handle`].
pub unsafe fn borrow_handle(h: Handle) -> Option<&'static Descriptor> {
    let inner = (h.as_ref()?.cast::<*mut Descriptor>()).read();
    inner.as_ref()
}

/// Take the descriptor back and release the handle.
///
/// # Safety
///
/// `h` is null or a handle from [`make_handle`], not used afterwards.
pub unsafe fn take_handle(h: Handle) -> Option<Descriptor> {
    if h.is_null() {
        return None;
    }
    let inner = (h.read() as *mut *mut Descriptor).read();
    let out = (!inner.is_null()).then(|| *Box::from_raw(inner));
    crate::suites::dispose_handle(h);
    out
}

/// What a plug-in holds while reading: the descriptor and how far
/// through it `GetKey` has walked.
pub struct ReadState {
    descriptor: Descriptor,
    /// Index of the entry the next `Get` call reads — the one `GetKey`
    /// last handed over.
    at: usize,
}

/// What a plug-in holds while writing.
pub struct WriteState {
    descriptor: Descriptor,
}

/// What a plug-in holds while reading; opaque to it.
pub type PIReadDescriptor = *mut ReadState;
/// What a plug-in holds while writing; opaque to it.
pub type PIWriteDescriptor = *mut WriteState;

macro_rules! read_state {
    ($d:expr) => {
        match $d.as_mut() {
            Some(s) => s,
            None => return PARAM_ERR,
        }
    };
}

macro_rules! current {
    ($s:expr) => {
        match $s.descriptor.entries.get($s.at) {
            Some((_, v)) => v,
            None => return PARAM_ERR,
        }
    };
}

// --- the read suite ------------------------------------------------------

/// # Safety
///
/// `descriptor` is null or a handle from [`make_handle`].
pub(crate) unsafe extern "C" fn open_read(
    descriptor: Handle,
    _keys: *mut c_void,
) -> PIReadDescriptor {
    // The key array says which keys the plug-in cares about; walking
    // them all in order is answer enough and is what it does anyway.
    let d = borrow_handle(descriptor).cloned().unwrap_or_default();
    crate::suites::trace!("descriptor.open_read({} keys)", d.entries.len());
    Box::into_raw(Box::new(ReadState {
        descriptor: d,
        at: usize::MAX,
    }))
}

/// # Safety
///
/// From [`open_read`], not used afterwards.
pub(crate) unsafe extern "C" fn close_read(d: PIReadDescriptor) -> OSErr {
    if !d.is_null() {
        drop(Box::from_raw(d));
    }
    NO_ERR
}

/// # Safety
///
/// `d` is from [`open_read`]; the out pointers are writable.
pub(crate) unsafe extern "C" fn get_key(
    d: PIReadDescriptor,
    key: *mut DescriptorKeyID,
    kind: *mut DescType,
    flags: *mut i16,
) -> MacBoolean {
    crate::suites::trace!("descriptor.get_key");
    let Some(state) = d.as_mut() else {
        return NO_MORE;
    };
    // `at` starts at usize::MAX so the first step lands on zero.
    state.at = state.at.wrapping_add(1);
    let Some((k, v)) = state.descriptor.entries.get(state.at) else {
        return NO_MORE;
    };
    if !key.is_null() {
        key.write(*k);
    }
    if !kind.is_null() {
        kind.write(v.desc_type());
    }
    if !flags.is_null() {
        flags.write(0);
    }
    1
}

/// # Safety
///
/// `d` is from [`open_read`]; `out` is writable.
pub(crate) unsafe extern "C" fn get_integer(d: PIReadDescriptor, out: *mut i32) -> OSErr {
    crate::suites::trace!("descriptor.get_integer");
    let s = read_state!(d);
    match current!(s) {
        Value::Integer(v) => write_out(out, *v),
        Value::Count(v) => write_out(out, *v as i32),
        _ => PARAM_ERR,
    }
}

/// # Safety
///
/// As [`get_integer`].
pub(crate) unsafe extern "C" fn get_count(d: PIReadDescriptor, out: *mut u32) -> OSErr {
    crate::suites::trace!("descriptor.get_count");
    let s = read_state!(d);
    match current!(s) {
        Value::Count(v) => write_out(out, *v),
        Value::Integer(v) => write_out(out, *v as u32),
        _ => PARAM_ERR,
    }
}

/// # Safety
///
/// As [`get_integer`].
pub(crate) unsafe extern "C" fn get_float(d: PIReadDescriptor, out: *mut f64) -> OSErr {
    crate::suites::trace!("descriptor.get_float");
    let s = read_state!(d);
    match current!(s) {
        Value::Float(v) => write_out(out, *v),
        Value::UnitFloat { value, .. } => write_out(out, *value),
        Value::Integer(v) => write_out(out, *v as f64),
        _ => PARAM_ERR,
    }
}

/// # Safety
///
/// As [`get_integer`].
pub(crate) unsafe extern "C" fn get_boolean(d: PIReadDescriptor, out: *mut MacBoolean) -> OSErr {
    crate::suites::trace!("descriptor.get_boolean");
    let s = read_state!(d);
    match current!(s) {
        Value::Boolean(v) => write_out(out, u8::from(*v)),
        _ => PARAM_ERR,
    }
}

/// # Safety
///
/// As [`get_integer`], and `out` points at a `Str255`.
pub(crate) unsafe extern "C" fn get_string(d: PIReadDescriptor, out: *mut u8) -> OSErr {
    crate::suites::trace!("descriptor.get_string");
    let s = read_state!(d);
    let Value::Text(text) = current!(s) else {
        return PARAM_ERR;
    };
    if out.is_null() {
        return PARAM_ERR;
    }
    let bytes = text.as_bytes();
    let len = bytes.len().min(255);
    out.write(len as u8);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.add(1), len);
    NO_ERR
}

/// # Safety
///
/// As [`get_integer`]; `out` receives a handle the plug-in disposes.
pub(crate) unsafe extern "C" fn get_text(d: PIReadDescriptor, out: *mut Handle) -> OSErr {
    crate::suites::trace!("descriptor.get_text");
    let s = read_state!(d);
    let Value::Text(text) = current!(s) else {
        return PARAM_ERR;
    };
    if out.is_null() {
        return PARAM_ERR;
    }
    let bytes = text.as_bytes();
    let h = crate::suites::new_handle(bytes.len() as i32);
    if h.is_null() {
        return PARAM_ERR;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), h.read(), bytes.len());
    out.write(h);
    NO_ERR
}

/// # Safety
///
/// As [`get_integer`].
pub(crate) unsafe extern "C" fn get_enumerated(
    d: PIReadDescriptor,
    kind: *mut DescType,
    value: *mut DescType,
) -> OSErr {
    crate::suites::trace!("descriptor.get_enumerated");
    let s = read_state!(d);
    let Value::Enumerated { kind: k, value: v } = current!(s) else {
        return PARAM_ERR;
    };
    if !kind.is_null() {
        kind.write(*k);
    }
    write_out(value, *v)
}

/// # Safety
///
/// As [`get_integer`].
pub(crate) unsafe extern "C" fn get_class(d: PIReadDescriptor, out: *mut DescType) -> OSErr {
    crate::suites::trace!("descriptor.get_class");
    let s = read_state!(d);
    match current!(s) {
        Value::Class(v) => write_out(out, *v),
        Value::Object { kind, .. } => write_out(out, *kind),
        _ => PARAM_ERR,
    }
}

/// # Safety
///
/// As [`get_integer`].
pub(crate) unsafe extern "C" fn get_unit_float(
    d: PIReadDescriptor,
    units: *mut DescriptorUnitID,
    value: *mut f64,
) -> OSErr {
    crate::suites::trace!("descriptor.get_unit_float");
    let s = read_state!(d);
    let Value::UnitFloat { unit, value: v } = current!(s) else {
        return PARAM_ERR;
    };
    if !units.is_null() {
        units.write(*unit);
    }
    write_out(value, *v)
}

/// # Safety
///
/// As [`get_integer`]; `min` and `max` are readable.
pub(crate) unsafe extern "C" fn get_pinned_float(
    d: PIReadDescriptor,
    min: *const f64,
    max: *const f64,
    out: *mut f64,
) -> OSErr {
    crate::suites::trace!("descriptor.get_pinned_float");
    let err = get_float(d, out);
    if err == NO_ERR {
        pin_f64(min, max, out);
    }
    err
}

/// # Safety
///
/// As [`get_integer`].
pub(crate) unsafe extern "C" fn get_pinned_integer(
    d: PIReadDescriptor,
    min: i32,
    max: i32,
    out: *mut i32,
) -> OSErr {
    crate::suites::trace!("descriptor.get_pinned_integer");
    let err = get_integer(d, out);
    if err == NO_ERR && !out.is_null() {
        out.write(out.read().clamp(min, max));
    }
    err
}

/// # Safety
///
/// As [`get_unit_float`].
pub(crate) unsafe extern "C" fn get_pinned_unit_float(
    d: PIReadDescriptor,
    min: *const f64,
    max: *const f64,
    units: *mut DescriptorUnitID,
    out: *mut f64,
) -> OSErr {
    crate::suites::trace!("descriptor.get_pinned_unit_float");
    let err = get_unit_float(d, units, out);
    if err == NO_ERR {
        pin_f64(min, max, out);
    }
    err
}

/// # Safety
///
/// As [`get_integer`]; `out` receives a handle from [`make_handle`].
pub(crate) unsafe extern "C" fn get_object(
    d: PIReadDescriptor,
    kind: *mut DescType,
    out: *mut Handle,
) -> OSErr {
    crate::suites::trace!("descriptor.get_object");
    let s = read_state!(d);
    let Value::Object { kind: k, fields } = current!(s) else {
        return PARAM_ERR;
    };
    if !kind.is_null() {
        kind.write(*k);
    }
    if out.is_null() {
        return PARAM_ERR;
    }
    out.write(make_handle(fields.clone()));
    NO_ERR
}

/// # Safety
///
/// As [`get_integer`].
pub(crate) unsafe extern "C" fn get_alias(d: PIReadDescriptor, out: *mut Handle) -> OSErr {
    crate::suites::trace!("descriptor.get_alias");
    let s = read_state!(d);
    let Value::Alias(bytes) = current!(s) else {
        return PARAM_ERR;
    };
    if out.is_null() {
        return PARAM_ERR;
    }
    let h = crate::suites::new_handle(bytes.len() as i32);
    if h.is_null() {
        return PARAM_ERR;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), h.read(), bytes.len());
    out.write(h);
    NO_ERR
}

/// References are not resolved against anything here, so this reports
/// that there is nothing to give rather than inventing one.
///
/// # Safety
///
/// As [`get_integer`].
pub(crate) unsafe extern "C" fn get_simple_reference(
    _d: PIReadDescriptor,
    _out: *mut c_void,
) -> OSErr {
    crate::suites::trace!("descriptor.get_simple_reference");
    PARAM_ERR
}

// --- the write suite -----------------------------------------------------

/// # Safety
///
/// Trivially safe; the result is freed by [`close_write`].
pub(crate) unsafe extern "C" fn open_write() -> PIWriteDescriptor {
    crate::suites::trace!("descriptor.open_write()");
    Box::into_raw(Box::new(WriteState {
        descriptor: Descriptor::default(),
    }))
}

/// # Safety
///
/// `d` is from [`open_write`] and not used afterwards; `out` is writable.
pub(crate) unsafe extern "C" fn close_write(d: PIWriteDescriptor, out: *mut Handle) -> OSErr {
    if d.is_null() {
        return PARAM_ERR;
    }
    let state = Box::from_raw(d);
    crate::suites::trace!(
        "descriptor.close_write({} keys)",
        state.descriptor.entries.len()
    );
    if !out.is_null() {
        out.write(make_handle(state.descriptor));
    }
    NO_ERR
}

macro_rules! write_state {
    ($d:expr) => {
        match $d.as_mut() {
            Some(s) => s,
            None => return PARAM_ERR,
        }
    };
}

/// # Safety
///
/// `d` is from [`open_write`].
pub(crate) unsafe extern "C" fn put_integer(
    d: PIWriteDescriptor,
    key: DescriptorKeyID,
    value: i32,
) -> OSErr {
    write_state!(d).descriptor.put(key, Value::Integer(value));
    NO_ERR
}

/// # Safety
///
/// As [`put_integer`]; `value` is readable.
pub(crate) unsafe extern "C" fn put_float(
    d: PIWriteDescriptor,
    key: DescriptorKeyID,
    value: *const f64,
) -> OSErr {
    let Some(v) = value.as_ref() else {
        return PARAM_ERR;
    };
    write_state!(d).descriptor.put(key, Value::Float(*v));
    NO_ERR
}

/// # Safety
///
/// As [`put_float`].
/// # Safety
///
/// As [`put_integer`].
pub(crate) unsafe extern "C" fn put_boolean(
    d: PIWriteDescriptor,
    key: DescriptorKeyID,
    value: MacBoolean,
) -> OSErr {
    write_state!(d)
        .descriptor
        .put(key, Value::Boolean(value != 0));
    NO_ERR
}

/// # Safety
///
/// As [`put_integer`]; `value` is a `Str255`.
pub(crate) unsafe extern "C" fn put_string(
    d: PIWriteDescriptor,
    key: DescriptorKeyID,
    value: *const u8,
) -> OSErr {
    let s = write_state!(d);
    let Some(len) = value.as_ref().map(|l| *l as usize) else {
        return PARAM_ERR;
    };
    let bytes = std::slice::from_raw_parts(value.add(1), len);
    s.descriptor
        .put(key, Value::Text(bytes.iter().map(|&b| b as char).collect()));
    NO_ERR
}

/// # Safety
///
/// As [`put_integer`]; `value` is a handle of text.
pub(crate) unsafe extern "C" fn put_text(
    d: PIWriteDescriptor,
    key: DescriptorKeyID,
    value: Handle,
) -> OSErr {
    let s = write_state!(d);
    let text = if value.is_null() {
        String::new()
    } else {
        let len = crate::suites::get_handle_size(value).max(0) as usize;
        let bytes = std::slice::from_raw_parts(value.read(), len);
        bytes.iter().map(|&b| b as char).collect()
    };
    s.descriptor.put(key, Value::Text(text));
    NO_ERR
}

/// # Safety
///
/// As [`put_integer`].
pub(crate) unsafe extern "C" fn put_enumerated(
    d: PIWriteDescriptor,
    key: DescriptorKeyID,
    kind: DescType,
    value: DescType,
) -> OSErr {
    write_state!(d)
        .descriptor
        .put(key, Value::Enumerated { kind, value });
    NO_ERR
}

/// # Safety
///
/// As [`put_integer`].
pub(crate) unsafe extern "C" fn put_class(
    d: PIWriteDescriptor,
    key: DescriptorKeyID,
    kind: DescType,
) -> OSErr {
    write_state!(d).descriptor.put(key, Value::Class(kind));
    NO_ERR
}

/// # Safety
///
/// As [`put_integer`].
pub(crate) unsafe extern "C" fn put_count(
    d: PIWriteDescriptor,
    key: DescriptorKeyID,
    value: u32,
) -> OSErr {
    write_state!(d).descriptor.put(key, Value::Count(value));
    NO_ERR
}

/// # Safety
///
/// As [`put_integer`]; `value` is a handle from [`make_handle`].
pub(crate) unsafe extern "C" fn put_object(
    d: PIWriteDescriptor,
    key: DescriptorKeyID,
    kind: DescType,
    value: Handle,
) -> OSErr {
    let s = write_state!(d);
    let fields = borrow_handle(value).cloned().unwrap_or_default();
    s.descriptor.put(key, Value::Object { kind, fields });
    NO_ERR
}

/// # Safety
///
/// As [`put_integer`]; `value` is a handle of alias bytes.
pub(crate) unsafe extern "C" fn put_alias(
    d: PIWriteDescriptor,
    key: DescriptorKeyID,
    value: Handle,
) -> OSErr {
    let s = write_state!(d);
    let bytes = if value.is_null() {
        Vec::new()
    } else {
        let len = crate::suites::get_handle_size(value).max(0) as usize;
        std::slice::from_raw_parts(value.read(), len).to_vec()
    };
    s.descriptor.put(key, Value::Alias(bytes));
    NO_ERR
}

/// # Safety
///
/// As [`put_integer`].
pub(crate) unsafe extern "C" fn put_simple_reference(
    _d: PIWriteDescriptor,
    _key: DescriptorKeyID,
    _value: *const c_void,
) -> OSErr {
    // Nothing here can resolve a reference, so recording one would
    // record something that cannot be played back.
    PARAM_ERR
}

// --- the suites ----------------------------------------------------------

/// Version and count from API Guide chapter 3: "ReadDescriptorProcs
/// suite. Current version: 0; Adobe Photoshop: 5.0; Routines: 18."
///
/// **The member order is not known and this suite is not served.** The
/// guide lists the eighteen routines alphabetically after Open and
/// Close, and that is not the layout: handed this suite in the
/// documented order, Filter Foundry opened a read descriptor and then
/// called slot 2 one and a half million times without stopping. It was
/// calling `GetKey` — you open a descriptor and iterate its keys — so
/// **`GetKey` is the third member**, and the alphabetical listing is
/// documentation order, exactly as it turned out to be for the Buffer
/// suite.
///
/// One slot of eighteen is not enough to serve on. A wrong getter does
/// not fail politely: it hands a plug-in a value of the wrong type or
/// spins, and plug-ins that work today would stop. So
/// `PIDescriptorParameters` carries null sub-suites, which is the
/// documented way to say scripting is unavailable, and plug-ins keep
/// their parameters in the `parameters` handle instead — which they
/// already do, and which works.
///
/// What would settle it: a plug-in whose recorded keys are known, so the
/// getter it reaches for can be identified the same way `GetKey` was.
/// `SCHIST_8BF_TRACE` names whichever slot gets called.
#[repr(C)]
pub struct ReadDescriptorProcs {
    pub read_descriptor_procs_version: i16,
    pub num_read_descriptor_procs: i16,
    pub open_read_descriptor: Option<unsafe extern "C" fn(Handle, *mut c_void) -> PIReadDescriptor>,
    pub close_read_descriptor: Option<unsafe extern "C" fn(PIReadDescriptor) -> OSErr>,
    /// Third, which is the one member position actually established.
    pub get_key: Option<
        unsafe extern "C" fn(
            PIReadDescriptor,
            *mut DescriptorKeyID,
            *mut DescType,
            *mut i16,
        ) -> MacBoolean,
    >,
    pub get_alias: Option<unsafe extern "C" fn(PIReadDescriptor, *mut Handle) -> OSErr>,
    pub get_boolean: Option<unsafe extern "C" fn(PIReadDescriptor, *mut MacBoolean) -> OSErr>,
    pub get_class: Option<unsafe extern "C" fn(PIReadDescriptor, *mut DescType) -> OSErr>,
    pub get_count: Option<unsafe extern "C" fn(PIReadDescriptor, *mut u32) -> OSErr>,
    pub get_enumerated:
        Option<unsafe extern "C" fn(PIReadDescriptor, *mut DescType, *mut DescType) -> OSErr>,
    pub get_float: Option<unsafe extern "C" fn(PIReadDescriptor, *mut f64) -> OSErr>,
    pub get_integer: Option<unsafe extern "C" fn(PIReadDescriptor, *mut i32) -> OSErr>,
    pub get_simple_reference: Option<unsafe extern "C" fn(PIReadDescriptor, *mut c_void) -> OSErr>,
    pub get_object:
        Option<unsafe extern "C" fn(PIReadDescriptor, *mut DescType, *mut Handle) -> OSErr>,
    pub get_pinned_float:
        Option<unsafe extern "C" fn(PIReadDescriptor, *const f64, *const f64, *mut f64) -> OSErr>,
    pub get_pinned_integer:
        Option<unsafe extern "C" fn(PIReadDescriptor, i32, i32, *mut i32) -> OSErr>,
    pub get_pinned_unit_float: Option<
        unsafe extern "C" fn(
            PIReadDescriptor,
            *const f64,
            *const f64,
            *mut DescriptorUnitID,
            *mut f64,
        ) -> OSErr,
    >,
    pub get_string: Option<unsafe extern "C" fn(PIReadDescriptor, *mut u8) -> OSErr>,
    pub get_text: Option<unsafe extern "C" fn(PIReadDescriptor, *mut Handle) -> OSErr>,
    pub get_unit_float:
        Option<unsafe extern "C" fn(PIReadDescriptor, *mut DescriptorUnitID, *mut f64) -> OSErr>,
}

/// Version and count from the same chapter: "WriteDescriptorProc suite.
/// Current version: 0; Adobe Photoshop 5.0; Routines: 16".
///
/// **Sixteen routines, thirteen documented, and the order unknown for
/// the same reason as the read suite.** Not served either. Three slots
/// the guide counts and never names are left at the end, which is where
/// a suite usually grows — also a guess.
#[repr(C)]
pub struct WriteDescriptorProcs {
    pub write_descriptor_procs_version: i16,
    pub num_write_descriptor_procs: i16,
    pub open_write_descriptor: Option<unsafe extern "C" fn() -> PIWriteDescriptor>,
    pub close_write_descriptor:
        Option<unsafe extern "C" fn(PIWriteDescriptor, *mut Handle) -> OSErr>,
    pub put_alias:
        Option<unsafe extern "C" fn(PIWriteDescriptor, DescriptorKeyID, Handle) -> OSErr>,
    pub put_boolean:
        Option<unsafe extern "C" fn(PIWriteDescriptor, DescriptorKeyID, MacBoolean) -> OSErr>,
    pub put_class:
        Option<unsafe extern "C" fn(PIWriteDescriptor, DescriptorKeyID, DescType) -> OSErr>,
    pub put_count: Option<unsafe extern "C" fn(PIWriteDescriptor, DescriptorKeyID, u32) -> OSErr>,
    pub put_enumerated: Option<
        unsafe extern "C" fn(PIWriteDescriptor, DescriptorKeyID, DescType, DescType) -> OSErr,
    >,
    pub put_float:
        Option<unsafe extern "C" fn(PIWriteDescriptor, DescriptorKeyID, *const f64) -> OSErr>,
    pub put_integer: Option<unsafe extern "C" fn(PIWriteDescriptor, DescriptorKeyID, i32) -> OSErr>,
    pub put_simple_reference:
        Option<unsafe extern "C" fn(PIWriteDescriptor, DescriptorKeyID, *const c_void) -> OSErr>,
    pub put_object:
        Option<unsafe extern "C" fn(PIWriteDescriptor, DescriptorKeyID, DescType, Handle) -> OSErr>,
    pub put_string:
        Option<unsafe extern "C" fn(PIWriteDescriptor, DescriptorKeyID, *const u8) -> OSErr>,
    pub put_text: Option<unsafe extern "C" fn(PIWriteDescriptor, DescriptorKeyID, Handle) -> OSErr>,
    /// The three the guide counts and does not name. See the note above.
    pub undocumented: [Option<unsafe extern "C" fn() -> OSErr>; 3],
}

// SAFETY: structs of plain function pointers, immutable once built.
unsafe impl Sync for ReadDescriptorProcs {}
unsafe impl Sync for WriteDescriptorProcs {}

unsafe extern "C" fn undocumented_slot() -> OSErr {
    crate::suites::trace!("descriptor: a routine this host does not know was called");
    PARAM_ERR
}

pub static READ_PROCS: ReadDescriptorProcs = ReadDescriptorProcs {
    read_descriptor_procs_version: 0,
    num_read_descriptor_procs: 18,
    open_read_descriptor: Some(open_read),
    close_read_descriptor: Some(close_read),
    get_key: Some(get_key),
    get_alias: Some(get_alias),
    get_boolean: Some(get_boolean),
    get_class: Some(get_class),
    get_count: Some(get_count),
    get_enumerated: Some(get_enumerated),
    get_float: Some(get_float),
    get_integer: Some(get_integer),
    get_simple_reference: Some(get_simple_reference),
    get_object: Some(get_object),
    get_pinned_float: Some(get_pinned_float),
    get_pinned_integer: Some(get_pinned_integer),
    get_pinned_unit_float: Some(get_pinned_unit_float),
    get_string: Some(get_string),
    get_text: Some(get_text),
    get_unit_float: Some(get_unit_float),
};

pub static WRITE_PROCS: WriteDescriptorProcs = WriteDescriptorProcs {
    write_descriptor_procs_version: 0,
    num_write_descriptor_procs: 16,
    open_write_descriptor: Some(open_write),
    close_write_descriptor: Some(close_write),
    put_alias: Some(put_alias),
    put_boolean: Some(put_boolean),
    put_class: Some(put_class),
    put_count: Some(put_count),
    put_enumerated: Some(put_enumerated),
    put_float: Some(put_float),
    put_integer: Some(put_integer),
    put_simple_reference: Some(put_simple_reference),
    put_object: Some(put_object),
    put_string: Some(put_string),
    put_text: Some(put_text),
    undocumented: [Some(undocumented_slot); 3],
};

// --- helpers -------------------------------------------------------------

unsafe fn write_out<T>(out: *mut T, v: T) -> OSErr {
    if out.is_null() {
        return PARAM_ERR;
    }
    out.write(v);
    NO_ERR
}

unsafe fn pin_f64(min: *const f64, max: *const f64, out: *mut f64) {
    if out.is_null() {
        return;
    }
    let mut v = out.read();
    if let Some(lo) = min.as_ref() {
        v = v.max(*lo);
    }
    if let Some(hi) = max.as_ref() {
        v = v.min(*hi);
    }
    out.write(v);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::fourcc;

    #[test]
    fn a_descriptor_keeps_one_value_per_key_in_order() {
        let mut d = Descriptor::default();
        d.put(fourcc(b"Rds "), Value::Integer(10));
        d.put(fourcc(b"Amnt"), Value::Boolean(true));
        d.put(fourcc(b"Rds "), Value::Integer(25));
        assert_eq!(d.entries.len(), 2, "a repeated key replaces, not appends");
        assert_eq!(d.get(fourcc(b"Rds ")), Some(&Value::Integer(25)));
        assert_eq!(d.entries[0].0, fourcc(b"Rds "), "order is the write order");
    }

    #[test]
    fn a_plug_in_writes_parameters_and_reads_them_back() {
        unsafe {
            // Write, exactly as a plug-in does at Finish.
            let w = open_write();
            assert_eq!(put_integer(w, fourcc(b"Rds "), 25), NO_ERR);
            assert_eq!(put_boolean(w, fourcc(b"Mnch"), 1), NO_ERR);
            let amount = 0.5f64;
            assert_eq!(put_float(w, fourcc(b"Amnt"), &amount), NO_ERR);
            let mut handle: Handle = std::ptr::null_mut();
            assert_eq!(close_write(w, &mut handle), NO_ERR);
            assert!(!handle.is_null());

            // Read, exactly as it does at Start the next time.
            let r = open_read(handle, std::ptr::null_mut());
            let mut seen = Vec::new();
            loop {
                let (mut key, mut kind, mut flags) = (0u32, 0u32, 0i16);
                if get_key(r, &mut key, &mut kind, &mut flags) == NO_MORE {
                    break;
                }
                match kind {
                    k if k == fourcc(b"long") => {
                        let mut v = 0i32;
                        assert_eq!(get_integer(r, &mut v), NO_ERR);
                        seen.push(format!("{}={v}", crate::abi::fourcc_str(key)));
                    }
                    k if k == fourcc(b"bool") => {
                        let mut v: MacBoolean = 0;
                        assert_eq!(get_boolean(r, &mut v), NO_ERR);
                        seen.push(format!("{}={v}", crate::abi::fourcc_str(key)));
                    }
                    k if k == fourcc(b"doub") => {
                        let mut v = 0f64;
                        assert_eq!(get_float(r, &mut v), NO_ERR);
                        seen.push(format!("{}={v}", crate::abi::fourcc_str(key)));
                    }
                    other => panic!("unexpected type {}", crate::abi::fourcc_str(other)),
                }
            }
            assert_eq!(close_read(r), NO_ERR);
            assert_eq!(seen, vec!["Rds =25", "Mnch=1", "Amnt=0.5"]);
            take_handle(handle);
        }
    }

    #[test]
    fn reading_the_wrong_type_is_refused_rather_than_reinterpreted() {
        unsafe {
            let w = open_write();
            put_boolean(w, fourcc(b"Mnch"), 1);
            let mut h: Handle = std::ptr::null_mut();
            close_write(w, &mut h);

            let r = open_read(h, std::ptr::null_mut());
            let (mut key, mut kind, mut flags) = (0u32, 0u32, 0i16);
            assert_eq!(get_key(r, &mut key, &mut kind, &mut flags), 1);
            let mut text: Handle = std::ptr::null_mut();
            assert_eq!(get_text(r, &mut text), PARAM_ERR);
            close_read(r);
            take_handle(h);
        }
    }

    #[test]
    fn pinned_reads_clamp_to_the_range_asked_for() {
        unsafe {
            let w = open_write();
            put_integer(w, fourcc(b"Rds "), 500);
            let big = 9.0f64;
            put_float(w, fourcc(b"Amnt"), &big);
            let mut h: Handle = std::ptr::null_mut();
            close_write(w, &mut h);

            let r = open_read(h, std::ptr::null_mut());
            let (mut k, mut t, mut f) = (0u32, 0u32, 0i16);
            get_key(r, &mut k, &mut t, &mut f);
            let mut i = 0i32;
            assert_eq!(get_pinned_integer(r, 1, 100, &mut i), NO_ERR);
            assert_eq!(i, 100);

            get_key(r, &mut k, &mut t, &mut f);
            let (lo, hi) = (0.0f64, 1.0f64);
            let mut d = 0f64;
            assert_eq!(get_pinned_float(r, &lo, &hi, &mut d), NO_ERR);
            assert_eq!(d, 1.0);
            close_read(r);
            take_handle(h);
        }
    }

    #[test]
    fn a_nested_object_survives_the_round_trip() {
        unsafe {
            let inner = open_write();
            put_integer(inner, fourcc(b"Hrzn"), 7);
            let mut inner_handle: Handle = std::ptr::null_mut();
            close_write(inner, &mut inner_handle);

            let outer = open_write();
            put_object(outer, fourcc(b"Ofst"), fourcc(b"Pnt "), inner_handle);
            let mut outer_handle: Handle = std::ptr::null_mut();
            close_write(outer, &mut outer_handle);

            let d = borrow_handle(outer_handle).unwrap();
            match d.get(fourcc(b"Ofst")) {
                Some(Value::Object { kind, fields }) => {
                    assert_eq!(*kind, fourcc(b"Pnt "));
                    assert_eq!(fields.get(fourcc(b"Hrzn")), Some(&Value::Integer(7)));
                }
                other => panic!("expected a nested object, got {other:?}"),
            }
            take_handle(inner_handle);
            take_handle(outer_handle);
        }
    }

    #[test]
    fn text_goes_out_as_a_pascal_string_and_comes_back_whole() {
        unsafe {
            let w = open_write();
            let mut pascal = vec![5u8];
            pascal.extend_from_slice(b"hello");
            put_string(w, fourcc(b"Nm  "), pascal.as_ptr());
            let mut h: Handle = std::ptr::null_mut();
            close_write(w, &mut h);
            assert_eq!(
                borrow_handle(h).unwrap().get(fourcc(b"Nm  ")),
                Some(&Value::Text("hello".into()))
            );
            take_handle(h);
        }
    }
}
