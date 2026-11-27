//! Ctrl+Alt+Del: only a service may send it (SendSAS), and only when the
//! SoftwareSASGeneration policy lets services (1) or services and ease of access (3) do.
//! When it says otherwise the service lets services for the one call and puts it back, as
//! RustDesk does.

use windows_sys::Win32::{
    Foundation::ERROR_SUCCESS,
    Security::Authentication::Identity::SendSAS,
    System::Registry::{
        HKEY_LOCAL_MACHINE, REG_DWORD, RRF_RT_REG_DWORD, RegDeleteKeyValueW, RegGetValueW,
        RegSetKeyValueW,
    },
};

use super::wide;

pub const POLICY_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
pub const POLICY_VALUE: &str = "SoftwareSASGeneration";

/// The DWORD at HKLM\`key`\`value`, if there is one.
pub fn read_dword(key: &str, value: &str) -> Option<u32> {
    let (key, value) = (wide(key), wide(value));
    let mut data = 0u32;
    let mut size = 4u32;
    // SAFETY: NUL-terminated names; a 4-byte buffer for a DWORD.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (r == ERROR_SUCCESS).then_some(data)
}

/// Sets HKLM\`key`\`value` to a DWORD, creating the key if need be.
pub fn write_dword(key: &str, value: &str, data: u32) -> Result<(), String> {
    let (k, v) = (wide(key), wide(value));
    // SAFETY: NUL-terminated names; 4 bytes of data.
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            v.as_ptr(),
            REG_DWORD,
            (&data as *const u32).cast(),
            4,
        )
    };
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!(
            "{key}\\{value}: {}",
            std::io::Error::from_raw_os_error(r as i32)
        ))
    }
}

/// Removes HKLM\`key`\`value`.
pub fn delete_value(key: &str, value: &str) {
    let (k, v) = (wide(key), wide(value));
    // SAFETY: NUL-terminated names.
    unsafe { RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), v.as_ptr()) };
}

/// Sends Ctrl+Alt+Del to the console session.
pub fn send() -> Result<(), String> {
    let before = read_dword(POLICY_KEY, POLICY_VALUE);
    let allowed = matches!(before, Some(1 | 3));
    if !allowed {
        write_dword(POLICY_KEY, POLICY_VALUE, 1)?;
    }
    // SAFETY: no pointers; as a service (FALSE).
    unsafe { SendSAS(0) };
    if !allowed {
        match before {
            Some(v) => write_dword(POLICY_KEY, POLICY_VALUE, v)?,
            None => delete_value(POLICY_KEY, POLICY_VALUE),
        }
    }
    Ok(())
}
