//! Default-device switching through the **undocumented** `IPolicyConfig`
//! COM interface (the one the Windows Sound control panel uses). It is not
//! part of the SDK and may change or disappear; every failure is returned as
//! an ordinary error so callers degrade gracefully.

#![allow(non_snake_case, dead_code)]

use std::ffi::c_void;

use windows::Win32::Media::Audio::{ERole, eCommunications, eConsole, eMultimedia};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, PCWSTR, interface};

use crate::Error;

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

// Vtable layout of the Vista..Win11 `IPolicyConfig`. Unused slots keep raw
// pointer arguments; only their position matters.
#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: IUnknown {
    unsafe fn GetMixFormat(&self, id: PCWSTR, fmt: *mut *mut c_void) -> HRESULT;
    unsafe fn GetDeviceFormat(&self, id: PCWSTR, default: i32, fmt: *mut *mut c_void) -> HRESULT;
    unsafe fn ResetDeviceFormat(&self, id: PCWSTR) -> HRESULT;
    unsafe fn SetDeviceFormat(&self, id: PCWSTR, a: *const c_void, b: *const c_void) -> HRESULT;
    unsafe fn GetProcessingPeriod(&self, id: PCWSTR, default: i32, a: *mut i64, b: *mut i64) -> HRESULT;
    unsafe fn SetProcessingPeriod(&self, id: PCWSTR, a: *const i64) -> HRESULT;
    unsafe fn GetShareMode(&self, id: PCWSTR, mode: *mut c_void) -> HRESULT;
    unsafe fn SetShareMode(&self, id: PCWSTR, mode: *mut c_void) -> HRESULT;
    unsafe fn GetPropertyValue(&self, id: PCWSTR, fx: i32, key: *const c_void, v: *mut c_void) -> HRESULT;
    unsafe fn SetPropertyValue(&self, id: PCWSTR, fx: i32, key: *const c_void, v: *mut c_void) -> HRESULT;
    unsafe fn SetDefaultEndpoint(&self, id: PCWSTR, role: ERole) -> HRESULT;
    unsafe fn SetEndpointVisibility(&self, id: PCWSTR, visible: i32) -> HRESULT;
}

/// Make the endpoint the default for console, multimedia and communications.
pub fn set_default(device_id: &str) -> Result<(), Error> {
    let policy: IPolicyConfig = unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }
        .map_err(|e| Error::Unsupported(format!("IPolicyConfig unavailable: {e}")))?;
    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
    for role in [eConsole, eMultimedia, eCommunications] {
        unsafe { policy.SetDefaultEndpoint(PCWSTR(wide.as_ptr()), role) }
            .ok()
            .map_err(|e| Error::Unsupported(format!("IPolicyConfig::SetDefaultEndpoint failed: {e}")))?;
    }
    Ok(())
}
