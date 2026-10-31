use std::num::{NonZeroU16, NonZeroU32};
use std::sync::Arc;

use frame_audio::{
    AudioCaptureBackend, AudioContext, AudioSession, DrainableAudioSession, PcmBlock, PcmDrainPoll,
};
use frame_clock::{ClockAnchor, MasterClock};
use frame_contracts::{
    AudioRoute, AudioSource, AudioSourceId, AudioSourceKind, ErrorCode, RecorderError,
};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;

use crate::{PlatformBackend, error, video::anchor};

const PREFIX: &str = "pipewire:mic:";
const MAX_PCM_FRAMES: usize = 4096;

fn devices() -> Result<(gst::DeviceMonitor, Vec<(String, gst::Device)>), RecorderError> {
    gst::init().map_err(|e| {
        error(
            ErrorCode::SourceLost,
            format!("GStreamer could not start: {e}"),
            "Install the GStreamer PipeWire plugin.",
        )
    })?;
    let monitor = gst::DeviceMonitor::new();
    monitor
        .add_filter(Some("Audio/Source"), None)
        .ok_or_else(|| {
            error(
                ErrorCode::SourceLost,
                "The GStreamer microphone monitor could not be configured.",
                "Check GStreamer device providers.",
            )
        })?;
    monitor.start().map_err(|e| {
        error(
            ErrorCode::SourceLost,
            format!("PipeWire microphone discovery failed: {e}"),
            "Check PipeWire and its GStreamer plugin.",
        )
    })?;
    let mut list = Vec::new();
    for device in monitor.devices() {
        // Exclude Audio/Source/Virtual and other subclasses: a loopback or
        // filter source must not be presented as a physical microphone.
        if device.device_class() != "Audio/Source" {
            continue;
        }
        let Some(props) = device.properties() else {
            continue;
        };
        let Ok(node_name) = props.get::<String>("node.name") else {
            continue;
        };
        if props.get::<String>("device.id").is_err() {
            continue;
        }
        if node_name.ends_with(".monitor") || node_name.is_empty() {
            continue;
        }
        // Only a device-backed source created by the PipeWire provider is
        // admitted. Virtual sources and monitor nodes are not microphones.
        let Ok(source) = device.create_element(None) else {
            continue;
        };
        if source
            .factory()
            .is_none_or(|factory| factory.name() != "pipewiresrc")
        {
            continue;
        }
        list.push((node_name, device));
    }
    Ok((monitor, list))
}

pub(crate) fn enumerate_microphones() -> Result<Vec<AudioSource>, RecorderError> {
    let (monitor, devices) = devices()?;
    let sources = devices
        .into_iter()
        .map(|(node, device)| AudioSource {
            controls: Default::default(),
            capture_identity: None,
            muted: false,
            id: AudioSourceId(format!("{PREFIX}{node}")),
            kind: AudioSourceKind::Microphone,
            display_name: device.display_name().to_string(),
            process_id: None,
            app_identifier: None,
            is_active: true,
            route: AudioRoute::Ignore,
        })
        .collect();
    monitor.stop();
    Ok(sources)
}

pub struct MicrophoneSession {
    source_id: AudioSourceId,
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    monitor: gst::DeviceMonitor,
    cancellation: frame_audio::AudioCancellation,
    channels: Option<NonZeroU16>,
    ready: Option<PcmBlock>,
    clock: Arc<MasterClock>,
    anchor: Option<ClockAnchor>,
    draining: bool,
    stopped: bool,
}

impl AudioCaptureBackend for PlatformBackend {
    type Session = LinuxAudioSession;

    fn reopen_audio_factory(&self) -> Option<frame_audio::AudioReopenFactory<Self::Session>> {
        Some(|source, context| PlatformBackend.start_audio(source, context))
    }

    fn start_audio(
        &mut self,
        source_id: &AudioSourceId,
        context: AudioContext,
    ) -> Result<Self::Session, RecorderError> {
        #[cfg(frame_linux_pulse)]
        if source_id.0.starts_with("pulse-app:") {
            return crate::application_audio::ApplicationSession::start(source_id, context)
                .map(LinuxAudioSession::Application);
        }
        let name = source_id.0.strip_prefix(PREFIX).ok_or_else(|| {
            error(
                ErrorCode::InvalidRequest,
                "Only a selected PipeWire microphone can be opened.",
                "Refresh microphone sources.",
            )
        })?;
        let (monitor, devices) = devices()?;
        let device = devices
            .into_iter()
            .find(|(node, _)| node == name)
            .map(|(_, d)| d)
            .ok_or_else(|| {
                error(
                    ErrorCode::SourceLost,
                    format!("Selected microphone {name} disappeared."),
                    "Refresh microphones and select the device again.",
                )
            })?;
        let source = device.create_element(None).map_err(|e| {
            error(
                ErrorCode::SourceLost,
                format!("Selected PipeWire microphone could not open: {e}"),
                "Check its permission and connection.",
            )
        })?;
        let queue = gst::ElementFactory::make("queue")
            .property("max-size-buffers", 8u32)
            .property("max-size-bytes", 131_072u32)
            .property("max-size-time", 0u64)
            .build()
            .map_err(gst_error)?;
        let convert = gst::ElementFactory::make("audioconvert")
            .build()
            .map_err(gst_error)?;
        let resample = gst::ElementFactory::make("audioresample")
            .build()
            .map_err(gst_error)?;
        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("rate", 48_000i32)
            .field("layout", "interleaved")
            .build();
        let filter = gst::ElementFactory::make("capsfilter")
            .property("caps", caps)
            .build()
            .map_err(gst_error)?;
        let sink = gst::ElementFactory::make("appsink")
            .property("max-buffers", 8u32)
            .property("max-bytes", 131_072u64)
            .property("drop", false)
            .property("sync", false)
            .build()
            .map_err(gst_error)?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| gst_error("Audio appsink type mismatch"))?;
        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([
                &source,
                &queue,
                &convert,
                &resample,
                &filter,
                sink.upcast_ref(),
            ])
            .map_err(gst_error)?;
        gst::Element::link_many([
            &source,
            &queue,
            &convert,
            &resample,
            &filter,
            sink.upcast_ref(),
        ])
        .map_err(gst_error)?;
        pipeline.set_state(gst::State::Playing).map_err(gst_error)?;
        let mut session = MicrophoneSession {
            source_id: source_id.clone(),
            pipeline,
            sink,
            monitor,
            cancellation: context.cancellation,
            channels: None,
            ready: None,
            clock: context.clock,
            anchor: None,
            draining: false,
            stopped: false,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if session.cancellation.is_cancelled() {
                return Err(gst_error("Microphone preparation was cancelled"));
            }
            if let Some(block) = session.try_pcm()? {
                session.ready = Some(block);
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(gst_error(
                    "Microphone did not negotiate PCM within three seconds",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Ok(LinuxAudioSession::Microphone(session))
    }
}

impl MicrophoneSession {
    fn check_bus(&self) -> Result<(), RecorderError> {
        if let Some(bus) = self.pipeline.bus() {
            for _ in 0..32 {
                let Some(message) = bus.pop() else {
                    break;
                };
                match message.view() {
                    gst::MessageView::Error(e) => {
                        return Err(error(
                            ErrorCode::SourceLost,
                            format!("Selected PipeWire microphone failed: {}", e.error()),
                            "Refresh microphones and check PipeWire permissions.",
                        ));
                    }
                    gst::MessageView::Eos(..) if !self.draining => {
                        return Err(error(
                            ErrorCode::SourceLost,
                            "Selected microphone ended before Stop.",
                            "Reconnect the microphone.",
                        ));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

impl AudioSession for MicrophoneSession {
    fn pcm_channels(&self) -> NonZeroU16 {
        self.channels.unwrap_or(NonZeroU16::new(2).unwrap())
    }
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        if self.stopped {
            return Ok(None);
        }
        if let Some(block) = self.ready.take() {
            return Ok(Some(block));
        }
        if self.cancellation.is_cancelled() {
            self.stop()?;
            return Ok(None);
        }
        self.check_bus()?;
        let Some(sample) = self.sink.try_pull_sample(gst::ClockTime::from_nseconds(0)) else {
            if self.sink.is_eos() && !self.draining {
                return Err(error(
                    ErrorCode::SourceLost,
                    "The microphone stream ended before Stop.",
                    "Reconnect the microphone.",
                ));
            }
            return Ok(None);
        };
        let channels = sample
            .caps()
            .and_then(|caps| caps.structure(0))
            .and_then(|s| s.get::<i32>("channels").ok())
            .filter(|c| matches!(c, 1 | 2))
            .and_then(|c| NonZeroU16::new(c as u16))
            .ok_or_else(|| {
                gst_error("Unsupported microphone channel layout; mono or stereo is required")
            })?;
        if self.channels.is_some_and(|previous| previous != channels) {
            return Err(gst_error("Microphone channel layout changed"));
        }
        self.channels = Some(channels);
        let buffer = sample
            .buffer()
            .ok_or_else(|| gst_error("Microphone sample has no buffer"))?;
        if self.anchor.is_none() {
            self.anchor = Some(anchor(&self.pipeline, &self.clock)?);
        }
        let base = self
            .pipeline
            .base_time()
            .ok_or_else(|| gst_error("Microphone pipeline has no base time"))?
            .nseconds();
        let raw = buffer
            .pts()
            .ok_or_else(|| gst_error("Microphone sample has no PTS"))?
            .nseconds();
        let timestamp = self
            .anchor
            .as_ref()
            .and_then(|a| a.map(base.checked_add(raw)?))
            .ok_or_else(|| gst_error("Microphone PTS mapping failed"))?;
        let map = buffer.map_readable().map_err(gst_error)?;
        let bytes = map.as_slice();
        if bytes.is_empty()
            || !bytes.len().is_multiple_of(usize::from(channels.get()) * 4)
            || bytes.len() / (usize::from(channels.get()) * 4) > MAX_PCM_FRAMES
        {
            return Err(gst_error(
                "Microphone PCM block has an invalid or oversized layout",
            ));
        }
        let samples = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        Ok(Some(PcmBlock {
            source_id: self.source_id.clone(),
            timestamp,
            sample_rate: NonZeroU32::new(48_000).unwrap(),
            channels,
            samples,
        }))
    }

    fn stop_for_continuity(&mut self) -> Result<(), RecorderError> {
        self.stop()
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        if !self.stopped {
            self.stopped = true;
            let result = self.pipeline.set_state(gst::State::Null).map_err(gst_error);
            self.monitor.stop();
            result?;
        }
        Ok(())
    }
}

impl DrainableAudioSession for MicrophoneSession {
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        if !self.draining {
            self.draining = true;
            if !self.pipeline.send_event(gst::event::Eos::new()) {
                return Err(gst_error("Microphone EOS request failed"));
            }
        }
        Ok(())
    }

    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        if !self.draining {
            return Err(gst_error("Microphone drain was not requested"));
        }
        if let Some(block) = self.try_pcm()? {
            return Ok(PcmDrainPoll::Block(block));
        }
        if self.sink.is_eos() {
            self.stop()?;
            Ok(PcmDrainPoll::Drained)
        } else {
            Ok(PcmDrainPoll::Pending)
        }
    }
}

impl Drop for MicrophoneSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn gst_error(e: impl std::fmt::Display) -> RecorderError {
    error(
        ErrorCode::SourceLost,
        format!("PipeWire microphone capture failed: {e}"),
        "Check the selected microphone and GStreamer PipeWire audio plugins.",
    )
}

pub enum LinuxAudioSession {
    Microphone(MicrophoneSession),
    #[cfg(frame_linux_pulse)]
    Application(crate::application_audio::ApplicationSession),
}
macro_rules! delegate {
    ($self:ident, $method:ident) => {
        match $self {
            Self::Microphone(session) => session.$method(),
            #[cfg(frame_linux_pulse)]
            Self::Application(session) => session.$method(),
        }
    };
}
impl AudioSession for LinuxAudioSession {
    fn pcm_channels(&self) -> NonZeroU16 {
        delegate!(self, pcm_channels)
    }
    fn try_pcm(&mut self) -> Result<Option<PcmBlock>, RecorderError> {
        delegate!(self, try_pcm)
    }
    fn stop(&mut self) -> Result<(), RecorderError> {
        delegate!(self, stop)
    }
    fn stop_for_continuity(&mut self) -> Result<(), RecorderError> {
        delegate!(self, stop_for_continuity)
    }
}
impl DrainableAudioSession for LinuxAudioSession {
    fn request_drain(&mut self) -> Result<(), RecorderError> {
        delegate!(self, request_drain)
    }
    fn poll_drain(&mut self) -> Result<PcmDrainPoll, RecorderError> {
        delegate!(self, poll_drain)
    }
}
