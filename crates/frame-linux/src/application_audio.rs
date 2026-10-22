// SPDX-License-Identifier: GPL-3.0-or-later
//! Isolated application streams via the optional PulseAudio protocol adapter.
#![allow(unsafe_code)]
use frame_audio::{AudioContext, AudioSession, DrainableAudioSession, PcmBlock, PcmDrainPoll};
use frame_clock::{ClockAnchor, MediaTimestamp};
use frame_contracts::{
    AudioRoute, AudioSource, AudioSourceId, AudioSourceKind, ErrorCode, RecorderError,
};
use std::{
    ffi::{CStr, c_char, c_void},
    num::{NonZeroU16, NonZeroU32},
    ptr::NonNull,
};

#[repr(C)]
#[derive(Clone, Copy)]
struct Application {
    index: u32,
    cookie: u32,
    client: u32,
    pid: u32,
    channels: u32,
    name: [c_char; 256],
}
unsafe extern "C" {
    fn freym_pulse_list(apps: *mut Application, capacity: usize) -> i32;
    fn freym_pulse_open(app: *const Application) -> *mut c_void;
    fn freym_pulse_close(session: *mut c_void);
    fn freym_pulse_read(
        session: *mut c_void,
        samples: *mut f32,
        capacity: usize,
        count: *mut usize,
        pts_ns: *mut i64,
    ) -> i32;
    fn freym_pulse_now() -> i64;
    fn freym_pulse_drain(session: *mut c_void);
}
fn lost(message: &str) -> RecorderError {
    crate::error(
        ErrorCode::SourceLost,
        message,
        "Refresh application audio sources; only the selected stream is captured.",
    )
}
fn list() -> Result<Vec<Application>, RecorderError> {
    // Discovery has a byte budget, never a product-imposed source count.
    let capacity = 1024 * 1024 / std::mem::size_of::<Application>();
    let mut apps = vec![
        Application {
            index: 0,
            cookie: 0,
            client: 0,
            pid: 0,
            channels: 0,
            name: [0; 256]
        };
        capacity
    ];
    // SAFETY: writable initialized array with the exact shared ABI and capacity.
    let count = unsafe { freym_pulse_list(apps.as_mut_ptr(), apps.len()) };
    if count < 0 || count as usize > apps.len() {
        return Err(lost("Application stream monitoring is unavailable."));
    }
    apps.truncate(count as usize);
    Ok(apps)
}
fn identity(app: &Application) -> AudioSourceId {
    AudioSourceId(format!(
        "pulse-app:{}:{}:{}:{}:{}",
        app.cookie, app.index, app.client, app.pid, app.channels
    ))
}
pub(crate) fn supported() -> bool {
    list().is_ok()
}
pub(crate) fn enumerate() -> Vec<AudioSource> {
    list()
        .unwrap_or_default()
        .iter()
        .map(|app| AudioSource {
            controls: Default::default(),
            capture_identity: None,
            muted: false,
            id: identity(app),
            kind: AudioSourceKind::Application,
            // SAFETY: native discovery always terminates this fixed display-only string.
            display_name: unsafe { CStr::from_ptr(app.name.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
            process_id: Some(app.pid),
            app_identifier: None,
            is_active: true,
            route: AudioRoute::Ignore,
        })
        .collect()
}
pub struct ApplicationSession {
    native: Option<NonNull<c_void>>,
    id: AudioSourceId,
    channels: NonZeroU16,
    anchor: ClockAnchor,
    context: AudioContext,
    draining: bool,
    scratch: [f32; 4800 * 2],
}
impl ApplicationSession {
    pub fn start(id: &AudioSourceId, context: AudioContext) -> Result<Self, RecorderError> {
        let app = list()?
            .into_iter()
            .find(|app| identity(app) == *id)
            .ok_or_else(|| lost("The selected application stream exited or changed identity."))?;
        if context.cancellation.is_cancelled() {
            return Err(lost("Application capture was cancelled."));
        }
        // SAFETY: exact initialized descriptor; native revalidates server cookie and stream identity.
        let native = NonNull::new(unsafe { freym_pulse_open(&app) }).ok_or_else(|| {
            lost("The audio server could not isolate the selected application stream.")
        })?;
        // Bracket the same native monotonic clock with the shared recording clock.
        let before = context.clock.now();
        let native_now = unsafe { freym_pulse_now() };
        let after = context.clock.now();
        let Some(anchor) = u64::try_from(native_now).ok().and_then(|raw| {
            ClockAnchor::from_sample(
                raw,
                std::num::NonZeroU64::new(1_000_000_000).unwrap(),
                before,
                after,
            )
        }) else {
            unsafe {
                freym_pulse_close(native.as_ptr());
            }
            return Err(lost("Application audio clock could not be anchored."));
        };
        Ok(Self {
            native: Some(native),
            id: id.clone(),
            channels: NonZeroU16::new(app.channels as u16).unwrap(),
            anchor,
            context,
            draining: false,
            scratch: [0.0; 9600],
        })
    }
}
impl AudioSession for ApplicationSession {
    fn pcm_channels(&self) -> NonZeroU16 {
        self.channels
    }
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        let Some(native) = self.native else {
            return Ok(None);
        };
        if self.context.cancellation.is_cancelled() {
            self.stop()?;
            return Ok(None);
        }
        let mut count = 0;
        let mut pts = 0;
        // SAFETY: live unique control-thread owner; writable fixed scratch and scalar outputs.
        let status = unsafe {
            freym_pulse_read(
                native.as_ptr(),
                self.scratch.as_mut_ptr(),
                self.scratch.len(),
                &mut count,
                &mut pts,
            )
        };
        if status < 0 {
            return Err(lost(
                "Selected application audio ended or its output device changed.",
            ));
        }
        if status == 2 {
            self.stop()?;
            return Ok(None);
        }
        if status == 0 {
            return Ok(None);
        }
        if count > self.scratch.len()
            || count == 0
            || !count.is_multiple_of(usize::from(self.channels.get()))
        {
            return Err(lost(
                "Application PCM exceeded its negotiated layout or buffer budget.",
            ));
        }
        let timestamp: MediaTimestamp = u64::try_from(pts)
            .ok()
            .and_then(|pts| self.anchor.map(pts))
            .ok_or_else(|| lost("Application PCM timestamp could not be mapped."))?;
        Ok(Some(PcmBlock {
            source_id: self.id.clone(),
            timestamp,
            sample_rate: NonZeroU32::new(48000).unwrap(),
            channels: self.channels,
            samples: self.scratch[..count].to_vec(),
        }))
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        if let Some(native) = self.native.take() {
            // SAFETY: unique ownership transferred back to the native destructor exactly once.
            unsafe {
                freym_pulse_close(native.as_ptr());
            }
        }
        Ok(())
    }
}
impl DrainableAudioSession for ApplicationSession {
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        if !self.draining {
            self.draining = true;
            if let Some(native) = self.native {
                unsafe {
                    freym_pulse_drain(native.as_ptr());
                }
            }
        }
        Ok(())
    }
    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        if !self.draining {
            return Err(RecorderError::invalid(
                "Application audio drain was not requested.",
            ));
        }
        if let Some(block) = self.try_pcm()? {
            return Ok(PcmDrainPoll::Block(block));
        }
        Ok(if self.native.is_none() {
            PcmDrainPoll::Drained
        } else {
            PcmDrainPoll::Pending
        })
    }
}
impl Drop for ApplicationSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
