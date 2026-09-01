//! Dragging photos out of the window on Windows: OLE's `DoDragDrop`.
//!
//! The data object is the shell's own — `IShellItemArray` bound to
//! `BHID_DataObject` hands back exactly what Explorer expects (CF_HDROP
//! and the rest), which is a great deal better than hand-rolling
//! `IDataObject`. Only the drop *source* is ours, and it exists to
//! answer two questions: whether the drag should continue, and what
//! cursor to show.
//!
//! `DoDragDrop` is modal — it runs its own message loop until the drop
//! lands, so this call does not return until the user lets go. That is
//! how every Windows drag source behaves.

use std::path::PathBuf;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{BOOL, POINT};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, DROPEFFECT, DROPEFFECT_COPY,
};
use windows::Win32::UI::Shell::{
    BHID_DataObject, IShellItem, IShellItemArray, SHCreateItemFromParsingName,
    SHCreateShellItemArrayFromShellItem,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetCursorPos, GetWindowThreadProcessId, WindowFromPoint, GA_ROOT,
};

/// Whether the window under the pointer belongs to another process.
/// Identity by process, so all of our own windows count as ours.
pub(super) fn over_foreign_window(_window: &gpui::Window) -> bool {
    unsafe {
        let mut at = POINT::default();
        if GetCursorPos(&mut at).is_err() {
            return false;
        }
        let under = WindowFromPoint(at);
        if under.is_invalid() {
            // Nothing but the desktop, which takes a file drop.
            return true;
        }
        let root = GetAncestor(under, GA_ROOT);
        let mut pid = 0u32;
        GetWindowThreadProcessId(if root.is_invalid() { under } else { root }, Some(&mut pid));
        pid != std::process::id()
    }
}

pub(super) fn start(paths: &[PathBuf], _window: &gpui::Window) -> bool {
    match unsafe { begin(paths) } {
        Ok(()) => true,
        Err(err) => {
            log::warn!("drag-out: the Windows drag did not start: {err}");
            false
        }
    }
}

unsafe fn begin(paths: &[PathBuf]) -> windows::core::Result<()> {
    let data = shell_data_object(paths)?;
    let source: IDropSource = DropSource.into();
    let mut effect = DROPEFFECT::default();
    // Copy only: the gallery watches these folders, and a drag that
    // moved the originals out of one would be a poor surprise.
    let _ = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
    Ok(())
}

/// The shell's data object for these files.
unsafe fn shell_data_object(paths: &[PathBuf]) -> windows::core::Result<IDataObject> {
    // The array is built from the first item and the rest are added by
    // the shell's own parsing; a single-item array is the common case
    // and multi-select rides on the same call.
    let mut items: Vec<Option<IShellItem>> = Vec::with_capacity(paths.len());
    for path in paths {
        let wide = HSTRING::from(path.as_os_str());
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)?;
        items.push(Some(item));
    }
    let first = items
        .first()
        .and_then(|i| i.clone())
        .ok_or_else(|| windows::core::Error::empty())?;
    let array: IShellItemArray = SHCreateShellItemArrayFromShellItem(&first)?;
    array.BindToHandler(None, &BHID_DataObject)
}

/// The drop source: continue while the button is held, stop when it is
/// released or Escape is pressed, and let Windows draw the cursors.
#[windows::core::implement(IDropSource)]
struct DropSource;

#[allow(non_snake_case)]
impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(
        &self,
        escape: BOOL,
        keys: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
    ) -> windows::core::HRESULT {
        use windows::Win32::Foundation::{DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, S_OK};
        use windows::Win32::System::SystemServices::MK_LBUTTON;
        if escape.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if !keys.contains(MK_LBUTTON) {
            return DRAGDROP_S_DROP;
        }
        S_OK
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> windows::core::HRESULT {
        windows::Win32::Foundation::DRAGDROP_S_USEDEFAULTCURSORS
    }
}
