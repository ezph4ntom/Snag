//! The Streamable login, kept in Windows Credential Manager ("Windows
//! Credentials" > "Snag/Streamable"), never in settings files or logs.

use anyhow::{Context, Result};
use windows::Win32::Security::Credentials::{
    CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
};
use windows::core::{PCWSTR, PWSTR};

const TARGET: &str = "Snag/Streamable";

pub struct Login {
    pub email: String,
    pub password: String,
}

/// Saves (or replaces) the login.
pub fn save(email: &str, password: &str) -> Result<()> {
    let mut target = crate::win::to_wide(TARGET);
    let mut user = crate::win::to_wide(email);
    let blob: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let cred = CREDENTIALW {
        Flags: CRED_FLAGS(0),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_ptr() as *mut u8,
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user.as_mut_ptr()),
        ..Default::default()
    };
    // SAFETY: every pointer in `cred` points into buffers alive for the call.
    unsafe { CredWriteW(&cred, 0) }.context("Windows Credential Manager refused to store the login")
}

/// The saved login, if any.
pub fn load() -> Option<Login> {
    let target = crate::win::to_wide(TARGET);
    let mut p: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: CredReadW allocates `p`; it is read, then freed with CredFree.
    unsafe {
        CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut p).ok()?;
        let c = &*p;
        let blob = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
        let units: Vec<u16> = blob.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect();
        let password = String::from_utf16_lossy(&units);
        let email = if c.UserName.is_null() { String::new() } else { c.UserName.to_string().unwrap_or_default() };
        CredFree(p as *const _);
        Some(Login { email, password })
    }
}

/// Just the email of the saved login (for showing who is connected).
pub fn email() -> Option<String> {
    load().map(|l| l.email)
}

pub fn delete() {
    let target = crate::win::to_wide(TARGET);
    // SAFETY: plain call with a valid string.
    let _ = unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) };
}

#[cfg(test)]
mod tests {
    #[test]
    fn saves_reads_and_deletes() {
        if super::load().is_some() {
            return; // a real login is saved: leave it alone
        }
        super::save("test@example.com", "pä55wörd ✓").unwrap();
        let l = super::load().unwrap();
        assert_eq!((l.email.as_str(), l.password.as_str()), ("test@example.com", "pä55wörd ✓"));
        super::delete();
        assert!(super::load().is_none());
    }
}
