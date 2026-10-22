// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
//! Finite synthetic renderer for process-isolation evidence. Never captures audio.
#![cfg_attr(target_os = "windows", allow(unsafe_code))]
#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("The renderer requires Windows.");
    std::process::exit(1);
}
#[cfg(target_os = "windows")]
fn main() {
    if let Err(e) = render::run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
#[cfg(target_os = "windows")]
mod render {
    use std::{
        io::Write,
        time::{Duration, Instant},
    };
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;
    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    struct Stream(IAudioClient);
    impl Drop for Stream {
        fn drop(&mut self) {
            unsafe {
                let _ = self.0.Stop();
            }
        }
    }
    pub(super) fn run() -> Result<()> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        if args.len() != 4 || args[0] != "--tone-hz" || args[2] != "--seconds" {
            return Err("application_tone_fixture --tone-hz 100..4000 --seconds 1..60".into());
        }
        let hz: u32 = args[1].parse()?;
        let seconds: u32 = args[3].parse()?;
        if !(100..=4000).contains(&hz) || !(1..=60).contains(&seconds) {
            return Err("Tone or duration outside bounds".into());
        }
        // SAFETY: this finite renderer owns COM and all native calls on this thread.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        let _apartment = Apartment;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 192_000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                1_000_000,
                0,
                &format,
                None,
            )?;
        }
        let capacity = unsafe { client.GetBufferSize()? };
        if capacity == 0 || capacity > 48_000 {
            return Err("Unexpected render capacity".into());
        }
        let renderer: IAudioRenderClient = unsafe { client.GetService()? };
        let mut frames = 0u64;
        fill(&renderer, capacity, hz, &mut frames)?;
        unsafe {
            client.Start()?;
        }
        let stream = Stream(client);
        println!(
            "{}",
            serde_json::json!({"event":"started","pid":std::process::id(),"toneHz":hz,"sampleRate":48000,"amplitude":0.08,"seconds":seconds})
        );
        std::io::stdout().flush()?;
        let begin = Instant::now();
        let mut successful_writes = 1u64;
        while begin.elapsed() < Duration::from_secs(u64::from(seconds)) {
            let padding = unsafe { stream.0.GetCurrentPadding()? };
            let available = capacity
                .checked_sub(padding)
                .ok_or("Invalid render padding")?;
            if available > 0 {
                fill(&renderer, available, hz, &mut frames)?;
                successful_writes += 1;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        unsafe {
            stream.0.Stop()?;
        }
        println!(
            "{}",
            serde_json::json!({"event":"finished","pid":std::process::id(),"toneHz":hz,
            "framesSubmitted":frames,"successfulWrites":successful_writes,"elapsedSeconds":begin.elapsed().as_secs_f64()})
        );
        Ok(())
    }
    fn fill(renderer: &IAudioRenderClient, frames: u32, hz: u32, position: &mut u64) -> Result<()> {
        // SAFETY: the caller has bounded frames by current available buffer capacity;
        // every successful acquisition is released before any other operation.
        let data = unsafe { renderer.GetBuffer(frames)? };
        if data.is_null() {
            unsafe {
                renderer.ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            }
            return Err("Render GetBuffer returned null".into());
        }
        let samples =
            unsafe { std::slice::from_raw_parts_mut(data.cast::<i16>(), frames as usize * 2) };
        for (index, pair) in samples.chunks_exact_mut(2).enumerate() {
            let phase =
                2.0 * std::f64::consts::PI * f64::from(hz) * (*position + index as u64) as f64
                    / 48_000.0;
            let value = (phase.sin() * 0.08 * 32767.0) as i16;
            pair.fill(value);
        }
        unsafe {
            renderer.ReleaseBuffer(frames, 0)?;
        }
        *position += u64::from(frames);
        Ok(())
    }
}
