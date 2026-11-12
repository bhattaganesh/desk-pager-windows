//! The pairing secret (ntfy topic), kept in Windows Credential Manager, never in a file or log.

use std::ffi::c_void;

use windows::{
    core::{w, Result, PCWSTR, PWSTR},
    Win32::{
        Foundation::ERROR_NOT_FOUND,
        Security::Credentials::{
            CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
        },
    },
};

const TARGET: PCWSTR = w!("DeskPager/ntfy-topic");
const TOPIC_PREFIX: &str = "deskpager-";
const TOPIC_RANDOM_BYTES: usize = 16;

/// The stored topic and whether it was created by this call (i.e. the phone isn't paired yet).
pub struct Topic {
    pub value: String,
    pub is_new: bool,
}

/// Returns the stored topic, creating and saving a random 128-bit one on first run.
pub fn load_or_create() -> Result<Topic> {
    if let Some(value) = read()? {
        return Ok(Topic { value, is_new: false });
    }
    let value = regenerate()?;
    Ok(Topic { value, is_new: true })
}

/// Generates and stores a fresh 128-bit random topic in Windows Credential Manager, replacing any old one.
pub fn regenerate() -> Result<String> {
    let mut random = [0u8; TOPIC_RANDOM_BYTES];
    getrandom::fill(&mut random).map_err(|e| windows::core::Error::from(std::io::Error::other(e.to_string())))?;
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let value = format!("{TOPIC_PREFIX}{hex}");
    write(&value)?;
    Ok(value)
}

fn read() -> Result<Option<String>> {
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: on success `cred` points at an OS-allocated CREDENTIALW that we free below.
    unsafe {
        if let Err(e) = CredReadW(TARGET, CRED_TYPE_GENERIC, None, &mut cred) {
            return if e.code() == ERROR_NOT_FOUND.to_hresult() { Ok(None) } else { Err(e) };
        }
        let blob = std::slice::from_raw_parts((*cred).CredentialBlob, (*cred).CredentialBlobSize as usize);
        let value = String::from_utf8_lossy(blob).into_owned();
        CredFree(cred as *const c_void);
        Ok(Some(value).filter(|v| is_valid(v)))
    }
}

fn write(value: &str) -> Result<()> {
    let mut blob = value.as_bytes().to_vec();
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(TARGET.as_ptr() as *mut u16),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    // SAFETY: `cred` and the buffers it points at outlive the call; the API only reads them.
    unsafe { CredWriteW(&cred, 0) }
}

/// A topic we generated: the prefix plus lowercase hex only, so it is always safe in a URL path.
fn is_valid(value: &str) -> bool {
    value.strip_prefix(TOPIC_PREFIX).is_some_and(|hex| {
        hex.len() == TOPIC_RANDOM_BYTES * 2 && hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}

#[cfg(test)]
mod tests {
    use super::is_valid;

    #[test]
    fn accepts_only_generated_topics() {
        assert!(is_valid("deskpager-00112233445566778899aabbccddeeff"));
        assert!(!is_valid("deskpager-00112233445566778899AABBCCDDEEFF"));
        assert!(!is_valid("deskpager-0011"));
        assert!(!is_valid("other-00112233445566778899aabbccddeeff"));
        assert!(!is_valid("deskpager-00112233445566778899aabbccddee/../x"));
    }
}
