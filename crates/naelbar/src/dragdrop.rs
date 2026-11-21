//! OLE drag and drop for the shelf: files in through `IDropTarget`, files out
//! through the shell's own data object and our `IDropSource`.

use std::path::PathBuf;

use windows::Win32::Foundation::{DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HWND, POINTL, S_OK};
use windows::Win32::System::Com::*;
use windows::Win32::System::Ole::*;
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::core::{BOOL, HRESULT, HSTRING, Ref, Result};
use windows_core::implement;

use crate::app::with_app;

const CF_HDROP: u16 = 15;

fn hdrop_format() -> FORMATETC {
    FORMATETC { cfFormat: CF_HDROP, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 }
}

fn has_files(data: Option<&IDataObject>) -> bool {
    data.is_some_and(|d| unsafe { d.QueryGetData(&hdrop_format()) } == S_OK)
}

fn read_files(data: &IDataObject) -> Vec<PathBuf> {
    let mut out = Vec::new();
    unsafe {
        let Ok(mut medium) = data.GetData(&hdrop_format()) else { return out };
        let hdrop = HDROP(medium.u.hGlobal.0);
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
            out.push(PathBuf::from(String::from_utf16_lossy(&buf[..got])));
        }
        ReleaseStgMedium(&mut medium);
    }
    out
}

#[implement(IDropTarget)]
struct DropTarget;

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(&self, data: Ref<IDataObject>, _keys: MODIFIERKEYS_FLAGS, _pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let ok = has_files(data.as_ref());
        unsafe { *effect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
        if ok {
            with_app(|a| a.on_drag_enter());
        }
        Ok(())
    }

    fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, _pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        unsafe { *effect = DROPEFFECT_COPY };
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        with_app(|a| a.on_drag_leave());
        Ok(())
    }

    fn Drop(&self, data: Ref<IDataObject>, _keys: MODIFIERKEYS_FLAGS, _pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let files = data.as_ref().map(read_files).unwrap_or_default();
        unsafe { *effect = DROPEFFECT_COPY };
        with_app(|a| a.on_drop(files));
        Ok(())
    }
}

pub fn register(hwnd: HWND) {
    let target: IDropTarget = DropTarget.into();
    unsafe {
        let _ = RegisterDragDrop(hwnd, &target);
    }
}

pub fn revoke(hwnd: HWND) {
    unsafe {
        let _ = RevokeDragDrop(hwnd);
    }
}

#[implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// What leaves the island when an item is dragged out.
pub enum Payload {
    Files(Vec<PathBuf>),
    Text(String),
}

/// Runs the modal OLE drag loop. Files are copied, never moved.
pub fn drag_out(payload: &Payload) {
    let data = match payload {
        Payload::Files(paths) => files_object(paths),
        Payload::Text(text) => text_object(text),
    };
    let Some(data) = data else { return };
    unsafe {
        let source: IDropSource = DropSource.into();
        let mut effect = DROPEFFECT_NONE;
        let _ = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
    }
}

/// The shell's own data object, so Explorer, mail and chat apps all take the files.
fn files_object(paths: &[PathBuf]) -> Option<IDataObject> {
    unsafe {
        let pidls: Vec<*mut ITEMIDLIST> = paths
            .iter()
            .filter(|p| p.exists())
            .map(|p| ILCreateFromPathW(&HSTRING::from(p.as_os_str())))
            .filter(|p| !p.is_null())
            .collect();
        if pidls.is_empty() {
            return None;
        }
        let list: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| *p as *const _).collect();
        let array = SHCreateShellItemArrayFromIDLists(&list);
        for p in pidls {
            ILFree(Some(p as *const _));
        }
        array.ok()?.BindToHandler::<_, IDataObject>(None, &BHID_DataObject).ok()
    }
}

/// Plain Unicode text, which editors, browsers and chat boxes accept.
fn text_object(text: &str) -> Option<IDataObject> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u16 = 13;
    unsafe {
        let data: IDataObject = SHCreateDataObject(None, None, None).ok()?;
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).ok()?;
        let ptr = GlobalLock(mem) as *mut u16;
        if ptr.is_null() {
            return None;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(mem);
        let format = FORMATETC { cfFormat: CF_UNICODETEXT, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
        let medium = STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: mem }, pUnkForRelease: std::mem::ManuallyDrop::new(None) };
        // The data object owns the memory from here on.
        data.SetData(&format, &medium, true).ok()?;
        Some(data)
    }
}

/// Writes a copied image where it can be dragged as a file. Only the latest
/// one is kept, in the naelbar folder of LOCALAPPDATA.
pub fn image_file(id: u64, png: &[u8]) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("naelbar").join("drag");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("image-{id}.png"));
    std::fs::write(&path, png).ok()?;
    Some(path)
}
