//! Bounded read-only process-tree and endpoint identity evidence. No handles escape.
use frame_contracts::*;
use std::collections::{BTreeMap, BTreeSet};
use windows::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
fn creation(pid: u32) -> Option<u64> {
    // SAFETY: read-only handle, four valid output values, closed once by the guard.
    let handle =
        OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe { GetProcessTimes(handle.0, &mut c, &mut e, &mut k, &mut u) }.ok()?;
    Some((u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime))
}

pub(crate) fn enrich(sources: &mut [AudioSource]) -> Result<(), RecorderError> {
    let mut parents = BTreeMap::new();
    if sources
        .iter()
        .any(|s| s.kind == AudioSourceKind::Application)
    {
        // A failed ancestry read remains unknown evidence; process capture separately
        // requires its retained root handle and session lifetime revalidation.
        if let Ok(h) = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
            let h = OwnedHandle(h);
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut next = unsafe { Process32FirstW(h.0, &mut entry) }.is_ok();
            while next && parents.len() < 65536 {
                parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                next = unsafe { Process32NextW(h.0, &mut entry) }.is_ok();
            }
        }
    }
    let mut times = BTreeMap::new();
    for source in sources {
        let endpoint = source
            .id
            .0
            .strip_prefix("wasapi-mic:")
            .or_else(|| source.id.0.strip_prefix(crate::system_audio::PREFIX))
            .map(str::to_lowercase);
        let mut identity = AudioCaptureIdentity {
            endpoint,
            process_instance: None,
            ancestor_instances: Vec::new(),
        };
        if source.kind == AudioSourceKind::Application
            && let Some(pid) = source.process_id.filter(|p| *p != 0)
        {
            let root_time = *times.entry(pid).or_insert_with(|| creation(pid));
            if let Some(mut child_time) = root_time {
                identity.process_instance = Some(format!("{pid}:{child_time}"));
                let mut cursor = pid;
                let mut seen = BTreeSet::from([pid]);
                for _ in 0..64 {
                    let Some(parent) = parents
                        .get(&cursor)
                        .copied()
                        .filter(|p| *p != 0 && seen.insert(*p))
                    else {
                        break;
                    };
                    let Some(parent_time) =
                        *times.entry(parent).or_insert_with(|| creation(parent))
                    else {
                        break;
                    };
                    if parent_time > child_time {
                        break;
                    } // recycled parent PID
                    identity
                        .ancestor_instances
                        .push(format!("{parent}:{parent_time}"));
                    child_time = parent_time;
                    cursor = parent;
                }
            }
        }
        source.capture_identity = Some(identity);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[test]
    fn native_process_lifetimes_prove_owned_parent_child_overlap() {
        let child = OwnedChild(
            std::process::Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Start-Sleep -Seconds 30",
                ])
                .creation_flags(0x08000000)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("owned read-only child"),
        );
        let source = |id: &str, pid| AudioSource {
            controls: Default::default(),
            capture_identity: None,
            id: AudioSourceId(format!("wasapi-session:{id}")),
            kind: AudioSourceKind::Application,
            display_name: id.into(),
            process_id: Some(pid),
            app_identifier: None,
            is_active: true,
            route: AudioRoute::MainMix,
            muted: false,
        };
        let mut sources = [
            source("parent", std::process::id()),
            source("child", child.0.id()),
        ];
        enrich(&mut sources).unwrap();
        let parent = sources[0]
            .capture_identity
            .as_ref()
            .unwrap()
            .process_instance
            .as_ref()
            .unwrap();
        assert!(
            sources[1]
                .capture_identity
                .as_ref()
                .unwrap()
                .ancestor_instances
                .contains(parent)
        );
        assert!(frame_settings::audio_admission::overlaps(
            &sources[0],
            &sources[1]
        ));
        let routes: Vec<_> = sources
            .iter()
            .map(|s| AudioRouteSelection {
                source_id: s.id.clone(),
                route: s.route,
                muted: false,
                controls: Default::default(),
            })
            .collect();
        assert!(
            frame_settings::audio_admission::validate_audio_selection(
                &routes, &sources, true, true
            )
            .unwrap_err()
            .message
            .contains("overlapping")
        );
    }
}
