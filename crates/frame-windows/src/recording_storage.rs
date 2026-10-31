//! Read-only recording-volume observation. Call on a background worker: a network
//! filesystem can block. The result is bytes available to this caller, including quotas.
use std::{io, os::windows::ffi::OsStrExt, path::Path};
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

pub fn available_bytes(directory: &Path) -> io::Result<u64> {
    if !directory.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Use an absolute recording directory.",
        ));
    }
    let mut wide: Vec<u16> = directory.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The recording path contains a NUL.",
        ));
    }
    // GetDiskFreeSpaceEx requires a trailing slash for a UNC share root. It also
    // accepts one on local and nested directories, and follows junction targets.
    if !matches!(wide.last(), Some(47 | 92)) {
        wide.push(92);
    }
    wide.push(0);
    let mut available = 0_u64;
    // SAFETY: wide is NUL terminated and lives across the call; available is a
    // writable u64. The unused outputs are explicitly optional null pointers.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(available)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recording_volume_observation_reads_real_bytes_and_refuses_invalid_paths() {
        assert!(available_bytes(Path::new("relative")).is_err());
        assert!(available_bytes(Path::new("C:/invalid\0path")).is_err());
        let folder = std::env::temp_dir();
        assert!(available_bytes(&folder).is_ok());
        assert!(available_bytes(&folder.join("freym-missing-volume-probe-directory")).is_err());
    }
}
