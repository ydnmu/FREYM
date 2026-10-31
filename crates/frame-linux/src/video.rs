use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::sync::Arc;
use std::time::{Duration, Instant};

use frame_clock::{ClockAnchor, MasterClock};
use frame_contracts::{ErrorCode, RecorderError};
use frame_encode::{BitstreamFormat, EncodedPacket, EncodedStreamInfo, EncoderCapability};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;

use crate::dma_buf::{
    SharedImportState, device_path, finish_release, install_guard, quiesce, share_va_display,
};
use crate::video_contract::{VideoContract, shm_fallback_allowed};
use crate::{
    DmaBufBlocker, DrmDeviceIdentity, assess_dma_buf_import, error, portal::PortalSession,
};

/// Automatic DMA-BUF qualification uses exact driver modifier caps, actual
/// PRIME import and one VA display for conversion/encoding. SHM remains the
/// fallback. Conversion may copy on the GPU; raw pixels never need mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderPath {
    VaApiShmCopy,
    VaApiDmaBuf,
    VaApiAv1ShmCopy,
    VaApiAv1DmaBuf,
}

impl EncoderPath {
    pub fn capability(self) -> EncoderCapability {
        EncoderCapability {
            id: match self {
                Self::VaApiShmCopy => "linux-vaapi-h264-shm-copy",
                Self::VaApiDmaBuf => "linux-vaapi-h264-dmabuf",
                Self::VaApiAv1ShmCopy => "linux-vaapi-av1-shm-copy",
                Self::VaApiAv1DmaBuf => "linux-vaapi-av1-dmabuf",
            }
            .into(),
            codec: if self.av1() { "av1" } else { "h264" }.into(),
            hardware: true,
        }
    }
    fn av1(self) -> bool {
        matches!(self, Self::VaApiAv1ShmCopy | Self::VaApiAv1DmaBuf)
    }
    fn dma(self) -> bool {
        matches!(self, Self::VaApiDmaBuf | Self::VaApiAv1DmaBuf)
    }
    fn shm(self) -> Self {
        if self.av1() {
            Self::VaApiAv1ShmCopy
        } else {
            Self::VaApiShmCopy
        }
    }
    fn gpu(self) -> Self {
        if self.av1() {
            Self::VaApiAv1DmaBuf
        } else {
            Self::VaApiDmaBuf
        }
    }
    fn bitstream(self) -> BitstreamFormat {
        if self.av1() {
            BitstreamFormat::Av1Obu
        } else {
            BitstreamFormat::H264AnnexB
        }
    }
}

pub fn probe_encoder() -> Result<Option<EncoderPath>, RecorderError> {
    probe_encoder_for_codec(false)
}

/// Actual VA device/caps and rate-control property query; recording additionally
/// requires first encoded output before publishing a usable stream.
pub fn probe_encoder_for_codec(av1: bool) -> Result<Option<EncoderPath>, RecorderError> {
    gst::init().map_err(|e| {
        error(
            ErrorCode::EncoderUnavailable,
            format!("GStreamer could not initialize: {e}"),
            "Install GStreamer and its VA plugin.",
        )
    })?;
    if gst::ElementFactory::find("pipewiresrc").is_none()
        || gst::ElementFactory::find(if av1 { "vaav1enc" } else { "vah264enc" }).is_none()
        || gst::ElementFactory::find(if av1 { "av1parse" } else { "h264parse" }).is_none()
        || gst::ElementFactory::find("videoconvert").is_none()
        || gst::ElementFactory::find("appsink").is_none()
    {
        return Ok(None);
    }
    // A factory alone is insufficient: require that the hardware element can
    // enter Ready, which opens its VA display on typical drivers. Playing is
    // checked again with the real negotiated stream during recording startup.
    let encoder = gst::ElementFactory::make(if av1 { "vaav1enc" } else { "vah264enc" })
        .build()
        .map_err(|e| {
            error(
                ErrorCode::EncoderUnavailable,
                format!("VA-API encoder creation failed: {e}"),
                "Check the VA driver and GStreamer VA plugin.",
            )
        })?;
    let ready = encoder.set_state(gst::State::Ready).is_ok();
    let supported = ready
        && encoder.static_pad("sink").is_some_and(|pad| {
            pad.query_caps(None).can_intersect(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "NV12")
                    .build(),
            )
        });
    let cbr = encoder
        .find_property("rate-control")
        .and_then(|p| gst::glib::EnumClass::with_type(p.value_type()))
        .is_some_and(|class| class.value_by_nick("cbr").is_some());
    encoder.set_state(gst::State::Null).map_err(|_| {
        error(
            ErrorCode::EncoderUnavailable,
            "VA qualification could not release its device.",
            "Inspect VA cleanup before retrying.",
        )
    })?;
    Ok((supported && cbr).then_some(if av1 {
        EncoderPath::VaApiAv1ShmCopy
    } else {
        EncoderPath::VaApiShmCopy
    }))
}

pub(crate) struct VideoStream {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    clock: Arc<MasterClock>,
    anchor: Option<ClockAnchor>,
    draining: bool,
    eos: bool,
    encoder: gst::Element,
    encoder_device_path: Option<String>,
    pub(crate) encoder_device: Option<DrmDeviceIdentity>,
    pub(crate) dma_buf_blocker: Option<DmaBufBlocker>,
    pub(crate) path: EncoderPath,
    pub(crate) import_state: Option<SharedImportState>,
    pub(crate) dma_buf_rejected_buffers: u64,
    input: PipelineInput,
    startup_deadline: Option<Instant>,
    contract: VideoContract,
    failure: Option<RecorderError>,
    stopped: bool,
    teardown_failure: Option<RecorderError>,
    pub(crate) frames: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone, Copy)]
struct PipelineInput {
    fd: i32,
    node: u32,
    size: (u32, u32),
    bitrate_bps: u32,
    gop_frames: u32,
    path: EncoderPath,
}

struct StartupFailure {
    error: RecorderError,
    blocker: Option<DmaBufBlocker>,
    released: bool,
}

impl VideoStream {
    pub(crate) fn start(
        portal: &PortalSession,
        clock: Arc<MasterClock>,
        bitrate_bps: u32,
        gop_frames: u32,
        path: EncoderPath,
    ) -> Result<Self, RecorderError> {
        if probe_encoder_for_codec(path.av1())?.is_none() {
            return Err(error(
                ErrorCode::EncoderUnavailable,
                "PipeWire, VA-API H.264 or a required GStreamer element is unavailable.",
                "Install GStreamer pipewire, va, videoconvert, app and H.264 parser plugins with a working VA driver.",
            ));
        }
        let input = PipelineInput {
            fd: portal.fd(),
            node: portal.node_id,
            size: portal.size.unwrap_or((0, 0)),
            bitrate_bps,
            gop_frames,
            path,
        };
        // Factory/caps discovery only permits a startup attempt. The route is
        // qualified by real PRIME import, VPP completion and encoded output
        // before the recording owner publishes Tracks or any media.
        if cfg!(frame_linux_va_import)
            && gst::ElementFactory::find("vapostproc").is_some()
            && std::env::var_os("FREYM_LINUX_DMA_BUF_IMPORT").is_none_or(|v| v != "0")
        {
            match Self::open(input, Arc::clone(&clock), path.gpu()) {
                Ok(stream) => return Ok(stream),
                Err(failure) if failure.released => {
                    let mut stream =
                        Self::open(input, clock, path.shm()).map_err(|failure| failure.error)?;
                    stream.dma_buf_blocker = failure.blocker.or(Some(DmaBufBlocker::StartupFailed));
                    return Ok(stream);
                }
                Err(failure) => return Err(failure.error),
            }
        }
        Self::open(input, clock, path.shm()).map_err(|failure| failure.error)
    }

    fn open(
        input: PipelineInput,
        clock: Arc<MasterClock>,
        path: EncoderPath,
    ) -> Result<Self, StartupFailure> {
        let bitrate_kbps = input.bitrate_bps.div_ceil(1000).max(1);
        // Only numeric values are interpolated. DMA caps let the source and VA
        // importer intersect the driver's actual fourcc/modifier list. Native
        // VA output caps forbid CPU download/conversion on the GPU route.
        let conversion = match path {
            EncoderPath::VaApiShmCopy | EncoderPath::VaApiAv1ShmCopy => {
                "video/x-raw ! videoconvert ! video/x-raw,format=NV12"
            }
            EncoderPath::VaApiDmaBuf | EncoderPath::VaApiAv1DmaBuf => {
                "capsfilter name=frame-dma-formats ! vapostproc name=frame-va-import disable-passthrough=true ! video/x-raw(memory:VAMemory),format=NV12"
            }
        };
        let codec = if path.av1() {
            format!(
                "vaav1enc name=frame-va-encoder rate-control=cbr hierarchical-level=1 gf-group-size=1 ref-frames=1 bitrate={bitrate_kbps} key-int-max={} ! av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu",
                input.gop_frames
            )
        } else {
            format!(
                "vah264enc name=frame-va-encoder rate-control=cbr b-frames=0 bitrate={bitrate_kbps} key-int-max={} ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au",
                input.gop_frames
            )
        };
        let description = format!(
            "pipewiresrc name=frame-pipewire fd={} path={} do-timestamp=false ! queue max-size-buffers=3 max-size-bytes=33554432 max-size-time=0 ! {} ! {} ! appsink name=frame-video max-buffers=8 max-bytes=33554432 sync=false drop=false",
            input.fd, input.node, conversion, codec,
        );
        let pipeline = gst::parse::launch(&description)
            .map_err(|e| StartupFailure {
                error: error(
                    ErrorCode::EncoderUnavailable,
                    format!("VA-API pipeline could not be negotiated: {e}"),
                    "Check PipeWire SHM support and VA-API NV12 H.264 support.",
                ),
                blocker: Some(DmaBufBlocker::ImportCapsUnsupported),
                released: true,
            })?
            .downcast::<gst::Pipeline>()
            .map_err(|_| StartupFailure {
                error: error(
                    ErrorCode::Internal,
                    "GStreamer did not create a video pipeline.",
                    "Report the pipeline failure.",
                ),
                blocker: None,
                released: true,
            })?;
        let mut opened_import = None;
        let setup = (|| -> Result<_, (RecorderError, Option<DmaBufBlocker>)> {
            let sink = pipeline
                .by_name("frame-video")
                .and_then(|element| element.downcast::<gst_app::AppSink>().ok())
                .ok_or_else(|| {
                    error(
                        ErrorCode::Internal,
                        "The H.264 appsink is missing.",
                        "Report the pipeline failure.",
                    )
                })
                .map_err(|error| (error, None))?;
            let encoder = pipeline
                .by_name("frame-va-encoder")
                .ok_or_else(|| {
                    error(
                        ErrorCode::EncoderUnavailable,
                        "The selected VA encoder is missing.",
                        "Inspect the installed VA plugin.",
                    )
                })
                .map_err(|error| (error, None))?;
            if path.dma() {
                encoder.set_state(gst::State::Ready).map_err(|_| {
                    (
                        dma_error(DmaBufBlocker::EncoderDeviceUnknown),
                        Some(DmaBufBlocker::EncoderDeviceUnknown),
                    )
                })?;
                // Set one display before VPP opens its filter/context. Replacing
                // a display after Ready could leave its filter on the old one.
                share_va_display(&encoder, &pipeline)
                    .map_err(|blocker| (dma_error(blocker), Some(blocker)))?;
            }
            let ready = pipeline.set_state(gst::State::Ready);
            if let Err(failure) = ready {
                return Err((
                    error(
                        ErrorCode::EncoderUnavailable,
                        format!("The VA device could not open: {failure:?}"),
                        "Inspect the VA driver.",
                    ),
                    None,
                ));
            }
            let encoder_device_path = device_path(&encoder);
            let encoder_device = encoder_device_path.as_deref().and_then(drm_identity);
            let import_state = if path.dma() {
                let importer = pipeline.by_name("frame-va-import").ok_or_else(|| {
                    (
                        dma_error(DmaBufBlocker::ImportCapsUnsupported),
                        Some(DmaBufBlocker::ImportCapsUnsupported),
                    )
                })?;
                let filter = pipeline.by_name("frame-dma-formats").ok_or_else(|| {
                    (
                        dma_error(DmaBufBlocker::ImportCapsUnsupported),
                        Some(DmaBufBlocker::ImportCapsUnsupported),
                    )
                })?;
                let source = pipeline.by_name("frame-pipewire").ok_or_else(|| {
                    (
                        dma_error(DmaBufBlocker::BufferOwnership),
                        Some(DmaBufBlocker::BufferOwnership),
                    )
                })?;
                // This disables PipeWire's gst_memory_copy path and enables
                // ParentBufferMeta retention until VPP releases each input.
                // Missing properties on an old plugin reject this attempt.
                if source.find_property("use-bufferpool").is_none() {
                    return Err((
                        dma_error(DmaBufBlocker::BufferOwnership),
                        Some(DmaBufBlocker::BufferOwnership),
                    ));
                }
                source.set_property("use-bufferpool", true);
                source.set_property("min-buffers", 4_i32);
                source.set_property("max-buffers", 16_i32);
                let state = install_guard(&importer, &encoder, &filter, encoder_device.as_ref())
                    .map_err(|blocker| (dma_error(blocker), Some(blocker)))?;
                opened_import = Some(Arc::clone(&state));
                Some(state)
            } else {
                None
            };
            let dma_buf_blocker = if !path.dma() {
                assess_dma_buf_import(None, encoder_device.as_ref(), false).err()
            } else {
                None
            };
            pipeline.set_state(gst::State::Playing).map_err(|e| {
                (
                    error(
                        ErrorCode::EncoderUnavailable,
                        format!("VA-API video pipeline could not start: {e:?}"),
                        "Check the VA driver and the selected portal stream.",
                    ),
                    None,
                )
            })?;
            Ok((
                sink,
                encoder,
                encoder_device_path,
                encoder_device,
                import_state,
                dma_buf_blocker,
            ))
        })();
        let (sink, encoder, encoder_device_path, encoder_device, import_state, dma_buf_blocker) =
            match setup {
                Ok(values) => values,
                Err((error, blocker)) => {
                    return Err(match release_pipeline(&pipeline, opened_import.as_ref()) {
                        Ok(()) => StartupFailure {
                            error,
                            blocker,
                            released: true,
                        },
                        Err(cleanup) => {
                            // An unresolved native owner must never return a
                            // PipeWire buffer for compositor reuse.
                            std::mem::forget(pipeline.clone());
                            if let Some(state) = opened_import.take() {
                                std::mem::forget(state);
                            }
                            StartupFailure {
                                error: cleanup,
                                blocker,
                                released: false,
                            }
                        }
                    });
                }
            };
        Ok(Self {
            pipeline,
            sink,
            clock,
            anchor: None,
            draining: false,
            eos: false,
            encoder,
            encoder_device_path,
            encoder_device,
            dma_buf_blocker,
            path,
            import_state,
            dma_buf_rejected_buffers: 0,
            input,
            startup_deadline: (path.dma()).then(|| Instant::now() + Duration::from_secs(3)),
            contract: VideoContract::new(input.size.0, input.size.1),
            failure: None,
            stopped: false,
            teardown_failure: None,
            frames: 0,
            width: input.size.0,
            height: input.size.1,
        })
    }

    pub(crate) fn poll(&mut self) -> Result<Option<EncodedPacket>, RecorderError> {
        if let Some(error) = self.failure.as_ref() {
            return Err(error.clone());
        }
        let result = self.poll_inner();
        if result.is_err() && self.path.dma() && shm_fallback_allowed(self.frames, self.draining) {
            // No media writer exists until the owner receives and validates the
            // first packet. Discard exactly one failed, unpublished generation;
            // the borrowed portal fd remains alive in the recording owner.
            let fallback = self.fallback_to_shm();
            if let Err(error) = fallback.as_ref() {
                self.failure = Some(error.clone());
                let _ = self.stop();
            }
            return fallback.map(|()| None);
        }
        if let Err(error) = result.as_ref() {
            self.failure = Some(error.clone());
            let _ = self.stop();
        }
        result
    }

    fn poll_inner(&mut self) -> Result<Option<EncodedPacket>, RecorderError> {
        self.check_import()?;
        self.check_bus()?;
        if device_path(&self.encoder) != self.encoder_device_path {
            return Err(error(
                ErrorCode::EncoderUnavailable,
                "The VA device changed after this stream opened.",
                "Start a new recording on the new device.",
            ));
        }
        let Some(sample) = self.sink.try_pull_sample(gst::ClockTime::from_nseconds(0)) else {
            if self
                .startup_deadline
                .is_some_and(|deadline| self.frames == 0 && Instant::now() >= deadline)
            {
                self.dma_buf_blocker = Some(DmaBufBlocker::StartupTimedOut);
                return Err(dma_error(DmaBufBlocker::StartupTimedOut));
            }
            self.eos = self.sink.is_eos();
            return Ok(None);
        };
        let buffer = sample.buffer().ok_or_else(|| {
            error(
                ErrorCode::Internal,
                "The H.264 encoder returned a sample without a buffer.",
                "Report this encoder failure.",
            )
        })?;
        if buffer.flags().contains(gst::BufferFlags::CORRUPTED) {
            return Err(error(
                ErrorCode::EncoderUnavailable,
                "The VA driver marked the encoded access unit as corrupt.",
                "Keep the partial recording and inspect the driver.",
            ));
        }
        if self.anchor.is_none() {
            self.anchor = Some(anchor(&self.pipeline, &self.clock)?);
        }
        let base = self
            .pipeline
            .base_time()
            .ok_or_else(|| {
                error(
                    ErrorCode::Internal,
                    "The H.264 pipeline has no base time.",
                    "Report this timestamp failure.",
                )
            })?
            .nseconds();
        let raw_pts = buffer
            .pts()
            .ok_or_else(|| {
                error(
                    ErrorCode::Internal,
                    "The H.264 encoder returned a frame without PTS.",
                    "Check the PipeWire timestamp source.",
                )
            })?
            .nseconds();
        let pts = self
            .anchor
            .as_ref()
            .and_then(|a| a.map(base.checked_add(raw_pts)?))
            .ok_or_else(|| {
                error(
                    ErrorCode::Internal,
                    "The H.264 PTS could not be mapped to the session clock.",
                    "Report this timestamp failure.",
                )
            })?;
        if let Some(caps) = sample.caps().and_then(|caps| caps.structure(0))
            && let (Ok(w), Ok(h)) = (caps.get::<i32>("width"), caps.get::<i32>("height"))
        {
            self.width = u32::try_from(w).unwrap_or(0);
            self.height = u32::try_from(h).unwrap_or(0);
        }
        let map = buffer.map_readable().map_err(|e| {
            error(
                ErrorCode::Internal,
                format!("H.264 packet memory could not be mapped: {e}"),
                "Report the encoder failure.",
            )
        })?;
        if map.size() > 16 * 1024 * 1024 {
            return Err(error(
                ErrorCode::EncoderUnavailable,
                "A single encoded frame exceeds 16 MiB.",
                "Use a smaller source or lower quality.",
            ));
        }
        let data = map.as_slice().to_vec();
        let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
        if self.frames == 0
            && (!keyframe
                || !frame_mux::independently_decodable_start(self.path.bitstream(), &data))
        {
            return Err(error(
                ErrorCode::EncoderUnavailable,
                "The first VA output cannot independently start a recording.",
                "Use a compatible VA encoder configuration or the SHM path.",
            ));
        }
        let codec_config = if keyframe {
            Some(self.codec_config(&data)?)
        } else {
            None
        };
        self.contract.packet(
            (self.width, self.height),
            raw_pts,
            buffer.dts().map(|dts| dts.nseconds()),
            codec_config.as_deref(),
        )?;
        // An asynchronous raw admission fault arriving while a packet is being
        // inspected must be surfaced before that packet reaches the mux.
        self.check_import()?;
        self.frames += 1;
        Ok(Some(EncodedPacket {
            track_id: 0,
            pts,
            dts: pts,
            duration_ns: buffer.duration().map_or(0, |d| d.nseconds()),
            keyframe,
            data,
        }))
    }

    pub(crate) fn stream_info(
        &self,
        first: &EncodedPacket,
        rate: NonZeroU32,
    ) -> Result<EncodedStreamInfo, RecorderError> {
        let width = NonZeroU32::new(self.width).ok_or_else(|| {
            error(
                ErrorCode::EncoderUnavailable,
                "The selected video stream has no negotiated width.",
                "Choose a screen that offers raw SHM video.",
            )
        })?;
        let height = NonZeroU32::new(self.height).ok_or_else(|| {
            error(
                ErrorCode::EncoderUnavailable,
                "The selected video stream has no negotiated height.",
                "Choose a screen that offers raw SHM video.",
            )
        })?;
        if !first.keyframe
            || !frame_mux::independently_decodable_start(self.path.bitstream(), &first.data)
        {
            return Err(error(
                ErrorCode::EncoderUnavailable,
                "The first VA-API output is not an IDR frame.",
                "Check the VA encoder configuration.",
            ));
        }
        Ok(EncodedStreamInfo {
            capability: self.path.capability(),
            width,
            height,
            nominal_rate_num: rate,
            nominal_rate_den: NonZeroU32::new(1).unwrap(),
            bitstream_format: self.path.bitstream(),
            colour: if self.path.av1() {
                Some(
                    frame_mux::av1_sequence_configuration(&first.data)
                        .map_err(|e| {
                            error(
                                ErrorCode::UnsupportedCodec,
                                e,
                                "Use a supported AV1 sequence.",
                            )
                        })?
                        .1,
                )
            } else {
                None
            },
            codec_config: self.codec_config(&first.data)?,
            allows_reordering: false,
        })
    }

    fn codec_config(&self, data: &[u8]) -> Result<Vec<u8>, RecorderError> {
        if self.path.av1() {
            frame_mux::av1_sequence_configuration(data)
                .map(|(config, _)| config)
                .map_err(|e| {
                    error(
                        ErrorCode::UnsupportedCodec,
                        e,
                        "Use an AV1 Main 8-bit single-layer sequence.",
                    )
                })
        } else {
            parameter_sets(data)
        }
    }

    pub(crate) fn begin_drain(&mut self) -> Result<(), RecorderError> {
        if !self.draining {
            self.draining = true;
            if !self.pipeline.send_event(gst::event::Eos::new()) {
                return Err(error(
                    ErrorCode::FinalizationFailed,
                    "Video EOS was rejected.",
                    "Keep the partial MKV for diagnosis.",
                ));
            }
        }
        Ok(())
    }
    pub(crate) fn drained(&self) -> bool {
        self.eos
    }
    pub(crate) fn stop(&mut self) -> Result<(), RecorderError> {
        if self.stopped {
            return self.teardown_failure.clone().map_or(Ok(()), Err);
        }
        self.stopped = true;
        if self.path.dma() {
            let released = release_pipeline(&self.pipeline, self.import_state.as_ref());
            if let Err(error) = released {
                self.failure.get_or_insert(error.clone());
                self.teardown_failure = Some(error);
                // Terminal quarantine on driver/teardown failure. Deliberate
                // retained refs protect borrowed fds/storage from early reuse.
                std::mem::forget(self.pipeline.clone());
                if let Some(state) = self.import_state.as_ref() {
                    std::mem::forget(Arc::clone(state));
                }
            }
        } else {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
        self.teardown_failure.clone().map_or(Ok(()), Err)
    }

    fn check_import(&mut self) -> Result<(), RecorderError> {
        if let Some(state) = self.import_state.as_ref() {
            let failure = state
                .lock()
                .map_err(|_| {
                    error(
                        ErrorCode::Internal,
                        "The DMA-BUF admission state is unavailable.",
                        "Keep the partial recording and inspect the native pipeline.",
                    )
                })?
                .failure;
            // Drop the admission mutex before touching GStreamer objects or
            // the VA device; a streaming callback also owns native pad locks.
            if let Some(blocker) = failure {
                self.dma_buf_blocker = Some(blocker);
                return Err(dma_error(blocker));
            }
            let importer = self.pipeline.by_name("frame-va-import");
            let device = importer
                .as_ref()
                .and_then(device_path)
                .as_deref()
                .and_then(drm_identity);
            let admission = match device.as_ref() {
                Some(device) => {
                    assess_dma_buf_import(Some(device), self.encoder_device.as_ref(), true)
                }
                None => Err(DmaBufBlocker::ImportDeviceUnknown),
            };
            if let Err(blocker) = admission {
                self.dma_buf_blocker = Some(blocker);
                return Err(dma_error(blocker));
            }
        }
        Ok(())
    }

    fn fallback_to_shm(&mut self) -> Result<(), RecorderError> {
        // Null must complete before opening another native VA/PipeWire owner.
        // Failure or uncertain teardown forbids fallback even before output.
        release_pipeline(&self.pipeline, self.import_state.as_ref())?;
        let rejected = if let Some(state) = self.import_state.as_ref() {
            state
                .lock()
                .map_err(|_| {
                    error(
                        ErrorCode::Internal,
                        "DMA-BUF teardown lost its admission state.",
                        "Inspect the native pipeline before retrying.",
                    )
                })?
                .rejected_buffers
        } else {
            0
        };
        let reason = self.dma_buf_blocker.or(Some(DmaBufBlocker::StartupFailed));
        let mut next = Self::open(self.input, Arc::clone(&self.clock), self.input.path.shm())
            .map_err(|failure| failure.error)?;
        next.dma_buf_blocker = reason;
        next.dma_buf_rejected_buffers = self.dma_buf_rejected_buffers.saturating_add(rejected);
        *self = next;
        Ok(())
    }

    fn check_bus(&mut self) -> Result<(), RecorderError> {
        if let Some(bus) = self.pipeline.bus() {
            while let Some(message) = bus.pop() {
                match message.view() {
                    gst::MessageView::Error(e) => {
                        return Err(error(
                            ErrorCode::SourceLost,
                            format!("PipeWire or VA-API video pipeline failed: {}", e.error()),
                            "Refresh the source and inspect PipeWire and VA-API logs.",
                        ));
                    }
                    gst::MessageView::Eos(..) if !self.draining => {
                        return Err(error(
                            ErrorCode::SourceLost,
                            "The selected screen stream ended before Stop.",
                            "Choose a screen again.",
                        ));
                    }
                    // Bus EOS can precede the last appsink packet. Only an
                    // empty appsink pull with EOS proves complete output.
                    gst::MessageView::Eos(..) => {}
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn drm_identity(path: &str) -> Option<DrmDeviceIdentity> {
    let node = std::fs::canonicalize(path).ok()?;
    let metadata = std::fs::metadata(&node).ok()?;
    if !metadata.file_type().is_char_device() {
        return None;
    }
    let device = metadata.rdev();
    let major = ((device >> 8) & 0xfff) | ((device >> 32) & 0xfffff000);
    let minor = (device & 0xff) | ((device >> 12) & 0xffffff00);
    if major != 226 {
        return None;
    }
    let physical_device =
        std::fs::canonicalize(format!("/sys/dev/char/{major}:{minor}/device")).ok()?;
    Some(DrmDeviceIdentity {
        node,
        physical_device,
    })
}

fn dma_error(blocker: DmaBufBlocker) -> RecorderError {
    error(
        ErrorCode::EncoderUnavailable,
        format!("DMA-BUF import admission failed: {blocker:?}."),
        "Use SHM capture or a source with a supported PRIME layout and synchronization on the selected VA device.",
    )
}

fn release_pipeline(
    pipeline: &gst::Pipeline,
    state: Option<&SharedImportState>,
) -> Result<(), RecorderError> {
    if let Some(state) = state {
        quiesce(state).map_err(dma_error)?;
    }
    stop_pipeline(pipeline)?;
    if let Some(state) = state {
        finish_release(state).map_err(dma_error)?;
    }
    Ok(())
}

fn stop_pipeline(pipeline: &gst::Pipeline) -> Result<(), RecorderError> {
    pipeline.set_state(gst::State::Null).map_err(|_| {
        error(
            ErrorCode::EncoderUnavailable,
            "The failed DMA-BUF pipeline could not release its native owners.",
            "Keep the partial recording; another pipeline cannot open safely.",
        )
    })?;
    let (result, state, pending) = pipeline.state(gst::ClockTime::from_seconds(1));
    if result.is_err() || state != gst::State::Null || pending != gst::State::VoidPending {
        return Err(error(
            ErrorCode::EncoderUnavailable,
            "The failed DMA-BUF pipeline did not prove complete native teardown.",
            "Inspect PipeWire and VA driver cleanup before retrying.",
        ));
    }
    Ok(())
}

fn parameter_sets(access_unit: &[u8]) -> Result<Vec<u8>, RecorderError> {
    let mut config = Vec::new();
    let mut saw_sps = false;
    let mut saw_pps = false;
    let mut cursor = 0;
    while let Some((prefix, start)) = next_start_code(access_unit, cursor) {
        let end = next_start_code(access_unit, start).map_or(access_unit.len(), |(at, _)| at);
        if start < end {
            match access_unit[start] & 0x1f {
                7 | 8 => {
                    saw_sps |= access_unit[start] & 0x1f == 7;
                    saw_pps |= access_unit[start] & 0x1f == 8;
                    if config.len().saturating_add(end - start + 4)
                        > frame_mux::MAX_CODEC_CONFIG_BYTES
                    {
                        return Err(error(
                            ErrorCode::UnsupportedCodec,
                            "The VA-API codec parameter sets exceed the MKV limit.",
                            "Use a different VA driver or screen geometry.",
                        ));
                    }
                    config.extend_from_slice(&[0, 0, 0, 1]);
                    config.extend_from_slice(&access_unit[start..end]);
                }
                _ => {}
            }
        }
        cursor = end.max(prefix + 1);
    }
    if !saw_sps || !saw_pps {
        return Err(error(
            ErrorCode::UnsupportedCodec,
            "The first VA-API access unit has no H.264 SPS and PPS.",
            "Check h264parse configuration and the VA driver.",
        ));
    }
    Ok(config)
}

fn next_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 3 <= data.len() {
        if index + 4 <= data.len() && data[index..index + 4] == [0, 0, 0, 1] {
            return Some((index, index + 4));
        }
        if data[index..index + 3] == [0, 0, 1] {
            return Some((index, index + 3));
        }
        index += 1;
    }
    None
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub(crate) fn anchor(
    pipeline: &gst::Pipeline,
    clock: &MasterClock,
) -> Result<ClockAnchor, RecorderError> {
    let gst_clock = pipeline.clock().ok_or_else(|| {
        error(
            ErrorCode::Internal,
            "A GStreamer pipeline clock is unavailable.",
            "Report this timestamp failure.",
        )
    })?;
    let before = clock.now();
    let tick = gst_clock.time().nseconds();
    let after = clock.now();
    ClockAnchor::from_sample(tick, NonZeroU64::new(1_000_000_000).unwrap(), before, after)
        .ok_or_else(|| {
            error(
                ErrorCode::Internal,
                "GStreamer clock mapping overflowed.",
                "Report this timestamp failure.",
            )
        })
}
