//! User-controlled launch at Windows sign-in. No elevated or process-wide setup.
pub const SUPPORTED: bool = cfg!(target_os = "windows");

#[cfg(target_os = "windows")]
pub use native::{enabled, set_enabled};
#[cfg(not(target_os = "windows"))]
pub fn enabled() -> Result<bool, String> {
    Ok(false)
}
#[cfg(not(target_os = "windows"))]
pub fn set_enabled(_: bool) -> Result<bool, String> {
    Err("Launch at startup is unavailable on this platform.".into())
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod native {
    use std::{os::windows::ffi::OsStrExt, path::Path};
    use windows_sys::{
        Win32::{
            Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS},
            System::Registry::{
                HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
                RegSetKeyValueW,
            },
        },
        core::w,
    };
    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
    fn command(exe: &Path) -> Result<Vec<u16>, String> {
        let path: Vec<u16> = exe.as_os_str().encode_wide().collect();
        if !exe.is_absolute() || path.contains(&0) || path.contains(&34) || path.len() + 2 > 260 {
            return Err(
                "The FREYM executable path cannot be registered for Windows startup.".into(),
            );
        }
        Ok(std::iter::once(34).chain(path).chain([34, 0]).collect())
    }
    fn read(key: &str) -> Result<bool, String> {
        let key = wide(key);
        let mut size = 0;
        // SAFETY: terminated key/value strings; this query requests only REG_SZ size.
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                w!("FREYM"),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        match result {
            ERROR_SUCCESS => Ok(size > 2),
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(false),
            error => Err(format!(
                "Could not read launch at startup: {}",
                std::io::Error::from_raw_os_error(error as i32)
            )),
        }
    }
    fn write(key: &str, value: Option<&[u16]>) -> Result<bool, String> {
        let key_w = wide(key);
        // SAFETY: this owns its string storage and changes only the FREYM value.
        // RegSetKeyValue creates the subkey if necessary; no HKLM/admin write.
        let result = unsafe {
            if let Some(value) = value {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key_w.as_ptr(),
                    w!("FREYM"),
                    REG_SZ,
                    value.as_ptr().cast(),
                    (value.len() * 2) as u32,
                )
            } else {
                RegDeleteKeyValueW(HKEY_CURRENT_USER, key_w.as_ptr(), w!("FREYM"))
            }
        };
        match result {
            ERROR_SUCCESS => read(key),
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND if value.is_none() => Ok(false),
            error => Err(format!(
                "Could not change launch at startup: {}",
                std::io::Error::from_raw_os_error(error as i32)
            )),
        }
    }
    pub fn enabled() -> Result<bool, String> {
        read(KEY)
    }
    pub fn set_enabled(enabled: bool) -> Result<bool, String> {
        if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            write(KEY, Some(&command(&exe)?))
        } else {
            write(KEY, None)
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn startup_command_quotes_unicode_paths_and_rejects_ambiguous_values() {
            let cmd = command(Path::new(r"C:\Program Files\FREYM İ\FREYM.exe")).unwrap();
            assert_eq!(
                String::from_utf16_lossy(&cmd[..cmd.len() - 1]),
                "\"C:\\Program Files\\FREYM İ\\FREYM.exe\""
            );
            assert!(command(Path::new("freym.exe")).is_err());
            assert!(command(Path::new("C:\\bad\"path.exe")).is_err());
            assert!(command(Path::new(&format!("C:\\{}.exe", "x".repeat(260)))).is_err());
        }
        #[test]
        fn startup_registry_roundtrip_uses_only_an_isolated_test_key() {
            let key = format!(r"Software\FREYM\Tests\Startup-{}", std::process::id());
            let cmd = command(Path::new(r"C:\FREYM test\FREYM.exe")).unwrap();
            let result = (|| {
                assert!(!read(&key)?);
                assert!(write(&key, Some(&cmd))?);
                assert!(read(&key)?);
                assert!(!write(&key, None)?);
                assert!(!write(&key, None)?);
                Ok::<_, String>(())
            })();
            let _ = write(&key, None);
            // SAFETY: remove only this test's empty, exact subkey, never the real Run key.
            unsafe {
                windows_sys::Win32::System::Registry::RegDeleteKeyW(
                    HKEY_CURRENT_USER,
                    wide(&key).as_ptr(),
                );
            }
            result.unwrap();
        }
    }
}
