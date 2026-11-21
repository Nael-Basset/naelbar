//! DPAPI wrapper: data is bound to the current Windows user.

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::PCWSTR;

/// App-specific entropy: another program running as the same user cannot
/// decrypt the history without knowing it.
const ENTROPY: &[u8] = b"naelbar.clipboard.history.v1";

fn blob(b: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: b.len() as u32, pbData: b.as_ptr() as *mut u8 }
}

fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    unsafe {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
        v
    }
}

pub(crate) fn protect(data: &[u8]) -> Option<Vec<u8>> {
    let input = blob(data);
    let entropy = blob(ENTROPY);
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .ok()?;
    }
    Some(take(out))
}

pub(crate) fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() {
        return None;
    }
    let input = blob(data);
    let entropy = blob(ENTROPY);
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .ok()?;
    }
    Some(take(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_not_plain() {
        let data = b"secret clipboard text \xc3\xa9";
        let enc = protect(data).unwrap();
        assert!(!enc.windows(data.len()).any(|w| w == data));
        assert_eq!(unprotect(&enc).unwrap(), data);
        assert!(unprotect(b"not a blob").is_none());
        assert!(unprotect(&[]).is_none());
    }
}
