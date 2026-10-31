//! Read-only current-process usage for the shell. Never sampled on a media or UI thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Tenths of one percent, normalized to the machine's logical CPU capacity.
    pub cpu: Option<u16>,
    /// Busiest GPU engine for this PID only; unavailable is never reported as zero.
    pub gpu: Option<u8>,
    /// Resident process working set, rounded down to MiB (Windows UI MB).
    pub ram_mb: Option<u64>,
    /// Current-process read + write transfer bytes per second, not disk utilization.
    pub io_bytes_per_sec: Option<u64>,
}

#[cfg(target_os = "windows")]
pub use native::Sampler;

#[cfg(not(target_os = "windows"))]
#[derive(Default)]
pub struct Sampler;
#[cfg(not(target_os = "windows"))]
impl Sampler {
    pub fn sample(&mut self) -> Usage {
        Usage::default()
    }
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)] // Win32 counter boundary; handles and buffers stay owned here.
mod native {
    use super::Usage;
    use std::{collections::BTreeMap, mem::size_of, ptr, time::Instant};
    use windows_sys::Win32::System::{
        Performance::*,
        ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::{
            GetActiveProcessorCount, GetCurrentProcess, GetProcessIoCounters, GetProcessTimes,
            IO_COUNTERS,
        },
    };

    #[derive(Default)]
    pub struct Sampler {
        query: PDH_HQUERY,
        previous_cpu: Option<(u64, Instant)>,
        previous_io: Option<(u64, Instant)>,
        gpu: PDH_HCOUNTER,
    }
    // SAFETY: PDH queries have no thread affinity. This owner is neither Clone
    // nor Sync; mutable sampling and Drop are serialized even across workers.
    unsafe impl Send for Sampler {}
    impl Drop for Sampler {
        fn drop(&mut self) {
            if !self.query.is_null() {
                // SAFETY: uniquely owned query; closes its counters too.
                unsafe {
                    PdhCloseQuery(self.query);
                }
            }
        }
    }
    fn percent(value: f64) -> Option<u8> {
        (value.is_finite() && value >= 0.).then(|| value.min(100.).round() as u8)
    }
    fn valid(status: u32) -> bool {
        matches!(status, PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA)
    }
    fn add(query: PDH_HQUERY, path: &str) -> PDH_HCOUNTER {
        let path = path.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let mut counter = ptr::null_mut();
        // SAFETY: NUL-terminated path and writable counter; query is live.
        if unsafe { PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) } == 0 {
            counter
        } else {
            ptr::null_mut()
        }
    }
    impl Sampler {
        pub fn sample(&mut self) -> Usage {
            // SAFETY: borrowed current-process pseudo handle; no CloseHandle needed.
            let process = unsafe { GetCurrentProcess() };
            let mut io = IO_COUNTERS::default();
            // Windows includes cached/device/network transfers in process I/O;
            // this is throughput, never physical disk busy time.
            let io_bytes_per_sec = if unsafe { GetProcessIoCounters(process, &mut io) } != 0 {
                let current = (
                    io.ReadTransferCount.saturating_add(io.WriteTransferCount),
                    Instant::now(),
                );
                self.previous_io.replace(current).and_then(|previous| {
                    io_rate(
                        current.0,
                        previous.0,
                        current.1.duration_since(previous.1).as_secs_f64(),
                    )
                })
            } else {
                self.previous_io = None;
                None
            };
            let mut memory = PROCESS_MEMORY_COUNTERS {
                cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
            let ram_mb = (unsafe {
                K32GetProcessMemoryInfo(
                    process,
                    &mut memory,
                    size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                )
            } != 0)
                .then_some(memory.WorkingSetSize as u64 / (1024 * 1024));
            let mut times = [windows_sys::Win32::Foundation::FILETIME::default(); 4];
            let [created, exited, kernel, user] = &mut times;
            let cpu = if unsafe { GetProcessTimes(process, created, exited, kernel, user) } != 0 {
                let ticks = |t: &windows_sys::Win32::Foundation::FILETIME| {
                    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
                };
                let current = (ticks(kernel).saturating_add(ticks(user)), Instant::now());
                let previous = self.previous_cpu.replace(current);
                let processors = unsafe { GetActiveProcessorCount(u16::MAX) };
                previous.and_then(|(ticks, time)| {
                    cpu_load(
                        current.0.checked_sub(ticks)?,
                        current.1.duration_since(time).as_secs_f64(),
                        processors,
                    )
                })
            } else {
                self.previous_cpu = None;
                None
            };
            if self.query.is_null() {
                // SAFETY: null selects real-time local counters; output is owned.
                if unsafe { PdhOpenQueryW(ptr::null(), 0, &mut self.query) } != 0 {
                    self.query = ptr::null_mut();
                    return Usage {
                        cpu,
                        ram_mb,
                        io_bytes_per_sec,
                        ..Default::default()
                    };
                }
                self.gpu = add(self.query, r"\GPU Engine(*)\Utilization Percentage");
                // Two observations are required for rate counters. No invented first value.
                unsafe {
                    PdhCollectQueryData(self.query);
                }
                return Usage {
                    cpu,
                    ram_mb,
                    io_bytes_per_sec,
                    ..Default::default()
                };
            }
            // SAFETY: query is live, sampling is sequential on a background worker.
            if unsafe { PdhCollectQueryData(self.query) } != 0 {
                return Usage {
                    cpu,
                    ram_mb,
                    io_bytes_per_sec,
                    ..Default::default()
                };
            }
            Usage {
                cpu,
                gpu: self.gpu_usage(),
                ram_mb,
                io_bytes_per_sec,
            }
        }
        fn gpu_usage(&self) -> Option<u8> {
            if self.gpu.is_null() {
                return None;
            }
            // Instance sets may change between the sizing and read calls. Retry
            // once from a fresh size; cap allocation rather than growing forever.
            for _ in 0..2 {
                let (mut bytes, mut count) = (0, 0);
                let status = unsafe {
                    PdhGetFormattedCounterArrayW(
                        self.gpu,
                        PDH_FMT_DOUBLE,
                        &mut bytes,
                        &mut count,
                        ptr::null_mut(),
                    )
                };
                if status != PDH_MORE_DATA || bytes == 0 || bytes > 1024 * 1024 {
                    return None;
                }
                // u64 allocation provides alignment for both structs and doubles.
                let mut buffer = vec![0u64; (bytes as usize).div_ceil(size_of::<u64>())];
                let items = buffer.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
                // SAFETY: aligned buffer of at least `bytes`; read only on success.
                let status = unsafe {
                    PdhGetFormattedCounterArrayW(
                        self.gpu,
                        PDH_FMT_DOUBLE,
                        &mut bytes,
                        &mut count,
                        items,
                    )
                };
                if status != 0 {
                    continue;
                }
                if count as usize
                    > buffer.len() * size_of::<u64>() / size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>()
                {
                    return None;
                }
                let mut engines = BTreeMap::<String, f64>::new();
                let own_pid = format!("pid_{}_", std::process::id());
                for item in unsafe { std::slice::from_raw_parts(items, count as usize) } {
                    if !valid(item.FmtValue.CStatus) {
                        continue;
                    }
                    // The API's strings reside in the same returned buffer. Bound
                    // every scan to that allocation, including the terminating NUL.
                    let start = item.szName as usize;
                    let begin = buffer.as_ptr() as usize;
                    let end = begin + buffer.len() * size_of::<u64>();
                    if start < begin || start >= end || !start.is_multiple_of(2) {
                        continue;
                    }
                    let name =
                        unsafe { std::slice::from_raw_parts(item.szName, (end - start) / 2) };
                    let Some(len) = name.iter().position(|c| *c == 0) else {
                        continue;
                    };
                    let name = String::from_utf16_lossy(&name[..len]);
                    accumulate(&mut engines, &own_pid, &name, unsafe {
                        item.FmtValue.Anonymous.doubleValue
                    });
                }
                return engines.into_values().reduce(f64::max).and_then(percent);
            }
            None
        }
    }
    fn cpu_load(ticks: u64, elapsed: f64, processors: u32) -> Option<u16> {
        if elapsed <= 0. || !elapsed.is_finite() || processors == 0 {
            return None;
        }
        Some(
            (ticks as f64 / 10_000_000. / elapsed / processors as f64 * 1000.)
                .clamp(0., 1000.)
                .round() as u16,
        )
    }
    fn io_rate(current: u64, previous: u64, elapsed: f64) -> Option<u64> {
        if elapsed <= 0. || !elapsed.is_finite() {
            return None;
        }
        Some((current.checked_sub(previous)? as f64 / elapsed).round() as u64)
    }
    fn accumulate(engines: &mut BTreeMap<String, f64>, own_pid: &str, instance: &str, value: f64) {
        if !instance.starts_with(own_pid) || !value.is_finite() || value < 0. {
            return;
        }
        // Strip PID, retain adapter LUID + physical GPU + engine identity.
        let Some(start) = instance.find("_luid_") else {
            return;
        };
        let key = instance[start + 1..].split('#').next().unwrap_or_default();
        if !key.contains("_phys_") || !key.contains("_eng_") {
            return;
        }
        *engines.entry(key.to_owned()).or_default() += value;
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn gpu_ignores_other_pids_and_does_not_sum_parallel_engines() {
            let mut engines = BTreeMap::new();
            accumulate(
                &mut engines,
                "pid_1_",
                "pid_1_luid_0x0_0x12_phys_0_eng_0_engtype_3D",
                25.,
            );
            accumulate(
                &mut engines,
                "pid_1_",
                "pid_2_luid_0x0_0x12_phys_0_eng_0_engtype_3D",
                30.,
            );
            accumulate(
                &mut engines,
                "pid_1_",
                "pid_1_luid_0x0_0x12_phys_0_eng_1_engtype_Copy",
                40.,
            );
            accumulate(
                &mut engines,
                "pid_1_",
                "pid_1_luid_0x0_0x99_phys_0_eng_0_engtype_3D",
                10.,
            );
            accumulate(&mut engines, "pid_1_", "unknown", 100.);
            accumulate(
                &mut engines,
                "pid_1_",
                "pid_1_luid_0x0_0x12_phys_0_eng_0_engtype_3D",
                f64::NAN,
            );
            assert_eq!(engines.len(), 3);
            assert_eq!(
                engines.into_values().reduce(f64::max).and_then(percent),
                Some(40)
            );
            assert_eq!(percent(f64::NAN), None);
            assert_eq!(percent(-1.), None);
            assert_eq!(percent(103.), Some(100));
        }
        #[test]
        fn process_cpu_uses_elapsed_time_and_logical_capacity() {
            assert_eq!(cpu_load(10_000_000, 1., 8), Some(125));
            assert_eq!(cpu_load(0, 1., 8), Some(0));
            assert_eq!(cpu_load(100, 0., 8), None);
            assert_eq!(cpu_load(100, 1., 0), None);
        }
        #[test]
        fn process_io_uses_transfer_delta_and_never_invents_first_or_reset_rates() {
            assert_eq!(io_rate(8192, 4096, 2.), Some(2048));
            assert_eq!(io_rate(4096, 4096, 1.), Some(0));
            assert_eq!(io_rate(1, 4096, 1.), None);
            assert_eq!(io_rate(8192, 4096, 0.), None);
        }
        #[test]
        fn native_usage_probe() {
            let mut sampler = Sampler::default();
            println!("initial: {:?}", sampler.sample());
            std::thread::sleep(std::time::Duration::from_secs(1));
            let usage = sampler.sample();
            println!("measured: {usage:?}");
            assert!(usage.cpu.is_some(), "Windows CPU counter unavailable");
            assert!(
                usage.io_bytes_per_sec.is_some(),
                "Process I/O counters unavailable"
            );
            assert!(
                usage.ram_mb.is_some(),
                "Process working-set data unavailable"
            );
            // GPU may be unavailable on unsupported drivers/remote sessions.
            assert!(usage.cpu.is_none_or(|v| v <= 1000) && usage.gpu.is_none_or(|v| v <= 100));
        }
    }
}
