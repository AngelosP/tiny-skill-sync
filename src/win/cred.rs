//! The GitHub token in Windows Credential Manager: a generic credential of the current user, kept on this machine only.

use std::io;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
use windows_sys::Win32::Security::Credentials::{CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW};

use crate::secret::Secret;
use crate::win::wide;

pub const TOKEN_TARGET: &str = "tiny-skill-sync:github-token";

/// Credential Manager's own dialog and `cmdkey` store text as UTF-16; other tools store UTF-8. Accept both.
/// (A token is plain ASCII, so a blob whose every second byte is zero is UTF-16.)
pub fn decode_blob(bytes: &[u8]) -> Option<String> {
    let utf16 = bytes.len() >= 2 && bytes.len() % 2 == 0 && bytes.chunks_exact(2).all(|c| c[1] == 0);
    let text = if utf16 {
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16(&units).ok()?
    } else {
        String::from_utf8(bytes.to_vec()).ok()?
    };
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

fn read(target: &str) -> Option<Secret> {
    let target = wide(target);
    let mut found: *mut CREDENTIALW = null_mut();
    if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut found) } == 0 {
        return None;
    }
    // SAFETY: CredReadW succeeded, so `found` points to a credential that stays valid until CredFree.
    let cred = unsafe { &*found };
    let blob = if cred.CredentialBlob.is_null() || cred.CredentialBlobSize == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize) }
    };
    let secret = decode_blob(blob).map(Secret::new);
    unsafe { CredFree(found.cast()) };
    secret
}

fn write(target: &str, secret: &str) -> io::Result<()> {
    let target = wide(target);
    let user = wide("tiny-skill-sync");
    let mut blob: Vec<u8> = secret.encode_utf16().flat_map(u16::to_le_bytes).collect();
    // SAFETY: CREDENTIALW is plain data; all-zero means "no optional fields".
    let mut cred: CREDENTIALW = unsafe { std::mem::zeroed() };
    cred.Type = CRED_TYPE_GENERIC;
    cred.TargetName = target.as_ptr() as *mut u16;
    cred.UserName = user.as_ptr() as *mut u16;
    cred.CredentialBlobSize = blob.len() as u32;
    cred.CredentialBlob = blob.as_mut_ptr();
    cred.Persist = CRED_PERSIST_LOCAL_MACHINE;
    let ok = unsafe { CredWriteW(&cred, 0) };
    let result = if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) };
    blob.fill(0);
    result
}

fn delete(target: &str) -> io::Result<bool> {
    let target = wide(target);
    if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } != 0 {
        return Ok(true);
    }
    if unsafe { GetLastError() } == ERROR_NOT_FOUND { Ok(false) } else { Err(io::Error::last_os_error()) }
}

pub fn read_token() -> Option<Secret> {
    // Live validation may use the GitHub CLI sign-in, but must never read the owner's saved app credential.
    if std::env::var_os("TSS_NO_SAVED_TOKEN").is_some() {
        return None;
    }
    read(TOKEN_TARGET)
}

pub fn has_token() -> bool {
    read_token().is_some()
}

pub fn write_token(token: &str) -> io::Result<()> {
    let token = token.trim();
    if token.is_empty() || token.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a token has no spaces or line breaks"));
    }
    write(TOKEN_TARGET, token)
}

/// Returns true when a token was stored and is now gone.
pub fn delete_token() -> io::Result<bool> {
    delete(TOKEN_TARGET)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_are_decoded_from_utf16_or_utf8() {
        let utf16: Vec<u8> = "ghp_abc123".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_blob(&utf16).as_deref(), Some("ghp_abc123"));
        assert_eq!(decode_blob(b"ghp_abc123\r\n").as_deref(), Some("ghp_abc123"));
        assert_eq!(decode_blob(b"ab").as_deref(), Some("ab"));
        assert_eq!(decode_blob(b""), None);
        assert_eq!(decode_blob(b"  \0 "), None);
        assert_eq!(decode_blob(&[0xff, 0xfe, 0xfd]), None);
    }

    #[test]
    fn a_credential_round_trips_and_deletes_cleanly() {
        let target = format!("tiny-skill-sync:test-{}", crate::fsx::unique_suffix());
        assert!(read(&target).is_none());
        write(&target, "ghp_roundtrip_value").unwrap();
        assert_eq!(read(&target).unwrap().expose(), "ghp_roundtrip_value");
        write(&target, "ghp_second").unwrap();
        assert_eq!(read(&target).unwrap().expose(), "ghp_second");
        assert!(delete(&target).unwrap());
        assert!(read(&target).is_none());
        assert!(!delete(&target).unwrap(), "deleting what is not there is not an error");
    }

    #[test]
    fn tokens_with_whitespace_are_refused_before_anything_is_stored() {
        assert!(write_token("two words").is_err());
        assert!(write_token("   ").is_err());
        assert!(write_token("line\nbreak").is_err());
    }
}
