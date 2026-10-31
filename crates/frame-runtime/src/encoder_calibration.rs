//! Idle, capture-free calibration through the production hardware encoder.
//! Discovery schedules work; startup and recording preparation only read the cache.
//! The bounded process cache is keyed by real driver version, adapter, geometry,
//! nominal rate and transfer choices; no result survives application restart.

use frame_contracts::{
    CaptureTarget, Dimensions, RecorderError, RecorderState, RecordingOptimization,
    RecordingRequest,
};
use frame_settings::{RecordingPlan, assessment::EncoderAssessment};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar,
        atomic::{AtomicBool, Ordering},
    },
    sync::{Mutex, OnceLock},
    time::Duration,
};

const CACHE_LIMIT: usize = 16;
const FRAMES: u32 = 48;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    candidate: String,
    adapter_luid: u64,
    driver_version: u64,
    config: frame_windows::NativeEncoderConfig,
    format: String,
    transport: String,
    pattern: bool,
}

#[derive(Default)]
struct Cache(VecDeque<(Key, EncoderAssessment)>);
impl Cache {
    fn get(&self, key: &Key) -> Option<EncoderAssessment> {
        self.0
            .iter()
            .find(|(known, _)| known == key)
            .map(|(_, value)| *value)
    }
    fn insert(&mut self, key: Key, value: EncoderAssessment) {
        // A driver change removes obsolete entries for this adapter immediately.
        self.0.retain(|(known, _)| {
            known.adapter_luid != key.adapter_luid
                || (known.driver_version == key.driver_version && known != &key)
        });
        if self.0.len() >= CACHE_LIMIT {
            self.0.pop_front();
        }
        self.0.push_back((key, value));
    }
    fn track(&mut self, key: Key) {
        if self.get(&key).is_none() {
            self.insert(
                key,
                EncoderAssessment {
                    completed_frames: 0,
                    elapsed_ns: 0,
                    failures: 0,
                    latency_p95_ns: None,
                    source_cost_p95_ns: None,
                    recording_overhead_p95_ns: None,
                    dropped_frames: 0,
                },
            );
        }
    }
    fn observe(&mut self, key: Key, live: EncoderAssessment) {
        if self.get(&key).is_none_or(|previous| {
            previous.capacity_millihz().is_none()
                || live.capacity_millihz() < previous.capacity_millihz()
        }) {
            self.insert(key, live);
        }
    }
}

struct State {
    cache: Cache,
    pending: VecDeque<RecordingRequest>,
}
struct Service {
    state: Mutex<State>,
    ready: Condvar,
    recording: AtomicBool,
}

fn enabled(value: Option<&str>) -> bool {
    value != Some("0")
}

fn service() -> &'static Arc<Service> {
    static SERVICE: OnceLock<Arc<Service>> = OnceLock::new();
    SERVICE.get_or_init(|| {
        let service = Arc::new(Service {
            state: Mutex::new(State {
                cache: Cache::default(),
                pending: VecDeque::new(),
            }),
            ready: Condvar::new(),
            recording: AtomicBool::new(false),
        });
        let worker = Arc::clone(&service);
        let _ = std::thread::Builder::new()
            .name("freym-encoder-calibration".into())
            .spawn(move || {
                loop {
                    let request = {
                        let mut state = worker.state.lock().unwrap_or_else(|p| p.into_inner());
                        while state.pending.is_empty() || worker.recording.load(Ordering::Acquire) {
                            state = worker
                                .ready
                                .wait_timeout(state, Duration::from_millis(100))
                                .unwrap_or_else(|p| p.into_inner())
                                .0;
                        }
                        state.pending.pop_front().unwrap()
                    };
                    let _ = calibrate(&worker, &request);
                }
            });
        service
    })
}

pub(crate) fn recording_state(state: &RecorderState) {
    let service = service();
    service.recording.store(
        !matches!(state, RecorderState::Idle | RecorderState::Failed(_)),
        Ordering::Release,
    );
    service.ready.notify_one();
}

/// Coalesced idle work. No native probe, encoding or wait on the caller.
pub fn schedule(request: RecordingRequest) {
    if !enabled(std::env::var("FREYM_ENCODER_CALIBRATION").ok().as_deref()) {
        return;
    }
    let service = service();
    let mut state = service.state.lock().unwrap_or_else(|p| p.into_inner());
    state.pending.retain(|r| r.target != request.target);
    if state.pending.len() >= CACHE_LIMIT {
        state.pending.pop_back();
    }
    state.pending.push_front(request);
    drop(state);
    service.ready.notify_one();
}

/// Enumerated capture adapters only; discovery never waits for calibration.
pub(crate) fn schedule_displays(displays: &[frame_contracts::DisplayInfo]) {
    for display in displays {
        schedule(RecordingRequest {
            target: CaptureTarget::Display(display.id.clone()),
            fps_mode: frame_contracts::FpsMode::AutoMatchSource,
            resolution: frame_contracts::Resolution::Native,
            quality: frame_contracts::Quality::Standard,
            encoder: frame_contracts::EncoderPreference::Auto,
            optimization: Some(RecordingOptimization::default()),
            audio_encoding: Default::default(),
            audio_routes: vec![],
            camera: None,

            show_cursor: true,
            output: frame_contracts::OutputSettings {
                directory: String::new(),
                file_stem: String::new(),
            },
        });
    }
}

fn key(adapter: &frame_windows::NativeAdapterIdentity, plan: &RecordingPlan) -> Option<Key> {
    let frame_settings::ResolutionPlan::Native(dimensions) = plan.resolution;
    Some(Key {
        candidate: plan.encoder.id.clone(),
        adapter_luid: adapter.luid,
        driver_version: frame_windows::adapter_driver_version(adapter.luid).ok()?,
        config: config_for_plan(dimensions, plan).ok()?,
        format: std::env::var("FREYM_MF_TRANSFER_FORMAT").unwrap_or_else(|_| "bgra8".into()),
        transport: std::env::var("FREYM_MF_TRANSFER_TRANSPORT").unwrap_or_else(|_| "cpu".into()),
        pattern: true,
    })
}

fn candidate_plans(
    inputs: &frame_settings::PlanInputs<'_>,
    options: Option<&RecordingOptimization>,
    codec: &'static str,
) -> Vec<RecordingPlan> {
    inputs.candidates.iter().filter_map(|candidate| {
        if matches!(&inputs.encoder, frame_contracts::EncoderPreference::Explicit(id) if id != &candidate.id) { return None; }
        let mut candidate_inputs = inputs.clone();
        candidate_inputs.encoder = frame_contracts::EncoderPreference::Explicit(candidate.id.clone());
        frame_settings::policy::plan_optimized_recording(&candidate_inputs, options, codec, &[]).ok()
    }).collect()
}

/// Keep the first real recording's feedback even if idle calibration was not ready.
pub(crate) fn track_recording(
    adapter: &frame_windows::NativeAdapterIdentity,
    plan: &RecordingPlan,
) {
    if !enabled(std::env::var("FREYM_ENCODER_CALIBRATION").ok().as_deref()) {
        return;
    }
    if let Some(key) = key(adapter, plan) {
        service()
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cache
            .track(key);
    }
}

/// Cache only: a missing, cancelled or unavailable calibration never delays Record.
pub(crate) fn assessments(
    adapter: &frame_windows::NativeAdapterIdentity,
    inputs: &frame_settings::PlanInputs<'_>,
    options: Option<&RecordingOptimization>,
    codec: &'static str,
) -> Vec<(String, EncoderAssessment)> {
    if !enabled(std::env::var("FREYM_ENCODER_CALIBRATION").ok().as_deref()) {
        return Vec::new();
    }
    candidate_plans(inputs, options, codec).into_iter().filter_map(|plan| {
        let mut key = key(adapter, &plan)?;
        let state = service().state.lock().unwrap_or_else(|p| p.into_inner());
        let mut value = state.cache.get(&key)?;
        let mut selected = inputs.clone();
        selected.encoder = frame_contracts::EncoderPreference::Explicit(plan.encoder.id.clone());
        // A measured automatic reduction can produce another exact config. Follow
        // its real recording feedback without treating that feedback as a probe
        // of the original config or making sparse desktop updates into a ceiling.
        for _ in 0..CACHE_LIMIT {
            let Ok(resolved) = frame_settings::policy::plan_optimized_recording(
                &selected, options, codec, &[(plan.encoder.id.clone(), value)],
            ) else { break; };
            let frame_settings::ResolutionPlan::Native(dimensions) = resolved.resolution;
            let Ok(config) = config_for_plan(dimensions, &resolved) else { break; };
            key.config = config;
            let Some(actual) = state.cache.get(&key) else { break; };
            if !matches!((actual.capacity_millihz(), value.capacity_millihz()), (Some(a), Some(b)) if a < b) { break; }
            value = actual;
        }
        Some((plan.encoder.id, value))
    }).collect()
}

/// Feed actual loss back into the same bounded device/config cache for the next
/// recording. Sparse desktop updates and deliberate FPS skips impose no limit.
pub(crate) fn observe_recording(plan: &RecordingPlan, frames: u64, elapsed_ns: u64, dropped: u64) {
    if frames == 0 || elapsed_ns == 0 || dropped == 0 {
        return;
    }
    let offered = u128::from(frames.saturating_add(dropped))
        * 1_000_000_000
        * u128::from(plan.nominal_rate.den().get());
    let nominal = u128::from(elapsed_ns) * u128::from(plan.nominal_rate.num().get());
    // Allow the same 20% headroom used by policy, plus ordinary timing jitter;
    // an isolated loss on an otherwise quiet desktop is not a capacity sample.
    if offered * 5 < nominal * 4 {
        return;
    }
    let frame_settings::ResolutionPlan::Native(dimensions) = plan.resolution;
    let Ok(config) = config_for_plan(dimensions, plan) else {
        return;
    };
    let mut state = service().state.lock().unwrap_or_else(|p| p.into_inner());
    let Some((mut key, mut value)) = state
        .cache
        .0
        .iter()
        .rev()
        .find(|(key, _)| {
            key.candidate == plan.encoder.id
                && key.config.width == dimensions.width
                && key.config.height == dimensions.height
        })
        .cloned()
    else {
        return;
    };
    if key.config != config {
        value.latency_p95_ns = None;
        value.recording_overhead_p95_ns = None;
    }
    key.config = config;
    let live = EncoderAssessment {
        completed_frames: frames,
        elapsed_ns,
        dropped_frames: dropped,
        failures: 0,
        ..value
    };
    state.cache.observe(key, live);
}

fn calibrate(service: &Service, request: &RecordingRequest) -> Result<(), RecorderError> {
    use frame_capture::DisplayBackend;
    use frame_settings::{NominalRate, PlanInputs};
    let displays = frame_windows::PlatformBackend.enumerate_displays()?;
    let window = match &request.target {
        CaptureTarget::Window(id) => Some(frame_windows::window_recording_source(id)?),
        CaptureTarget::Game(game) => Some(frame_windows::window_recording_source(
            &frame_windows::resolve_game_window(game)?,
        )?),
        _ => None,
    };
    let display = match &request.target {
        CaptureTarget::Display(id) => displays.iter().find(|d| &d.id == id),
        CaptureTarget::Region(r) => displays.iter().find(|d| d.id == r.display_id),
        _ => displays.iter().find(|d| d.is_primary),
    }
    .ok_or_else(|| RecorderError::invalid("Calibration source is no longer attached."))?;
    let dimensions = if let Some(window) = &window {
        Dimensions {
            width: window.width,
            height: window.height,
        }
    } else {
        match &request.target {
            CaptureTarget::Region(r) => Dimensions {
                width: r.width,
                height: r.height,
            },
            _ => Dimensions {
                width: display.width,
                height: display.height,
            },
        }
    };
    let adapter = if let Some(window) = &window {
        window.adapter.clone()
    } else {
        frame_windows::display_capture_adapter_identity(&display.id)?
    };
    let rate = if window.is_some()
        && let frame_contracts::FpsMode::Fixed(fps) = request.fps_mode
    {
        NominalRate::recover_exact(fps)?
    } else {
        NominalRate::recover_exact(
            display
                .refresh_rate_hz
                .ok_or_else(|| RecorderError::invalid("Unknown source rate."))?,
        )?
    };
    let hdr = window.is_none() && frame_windows::active_hdr_display(&display.id)? == Some(true);
    if service.recording.load(Ordering::Acquire) {
        schedule(request.clone());
        return Ok(());
    }
    let candidates = crate::recording_control::windows_policy_candidates(
        request,
        &adapter,
        dimensions,
        rate.num(),
        rate.den(),
        hdr,
    )?;
    let inputs = PlanInputs {
        quality: request.quality,
        resolution: request.resolution,
        fps_mode: request.fps_mode,
        encoder: request.encoder.clone(),
        native: dimensions,
        nominal_rate: rate,
        candidates: &candidates,
    };
    let codec = if matches!(&request.encoder, frame_contracts::EncoderPreference::Explicit(id) if id.contains("av1"))
    {
        "av1"
    } else if hdr {
        "hevc"
    } else {
        "h264"
    };
    for plan in candidate_plans(&inputs, request.optimization.as_ref(), codec) {
        if service.recording.load(Ordering::Acquire) {
            schedule(request.clone());
            break;
        }
        let Some(key) = key(&adapter, &plan) else {
            continue;
        };
        if service
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cache
            .get(&key)
            .is_some_and(|m| m.failures != 0 || m.throughput_millihz().is_some())
        {
            continue;
        }
        let result = frame_windows::synthetic_encoder_sequence(
            &adapter,
            &key.config,
            Some(&plan.encoder.id),
            FRAMES,
            || service.recording.load(Ordering::Acquire),
            |_, _| Ok(()),
        );
        if service.recording.load(Ordering::Acquire) {
            schedule(request.clone());
            break;
        }
        let value = match result {
            Ok(result) => EncoderAssessment {
                completed_frames: result.stats.completed_frames,
                elapsed_ns: result.elapsed_ns,
                failures: u32::try_from(result.stats.native_failures).unwrap_or(u32::MAX),
                latency_p95_ns: result.latency_p95_ns,
                source_cost_p95_ns: None,
                recording_overhead_p95_ns: result.overhead_p95_ns,
                dropped_frames: result.stats.rejected_inputs,
            },
            // Failure is bounded to this exact device/config, not global hardware admission.
            Err(_) => EncoderAssessment {
                completed_frames: 0,
                elapsed_ns: 0,
                failures: 1,
                latency_p95_ns: None,
                source_cost_p95_ns: None,
                recording_overhead_p95_ns: None,
                dropped_frames: 0,
            },
        };
        service
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cache
            .insert(key, value);
    }
    Ok(())
}

/// Recording and calibration use the identical bitrate, rate, GOP and preset.
pub fn config_for_plan(
    dimensions: Dimensions,
    plan: &RecordingPlan,
) -> Result<frame_windows::NativeEncoderConfig, RecorderError> {
    let config = frame_windows::NativeEncoderConfig {
        width: dimensions.width,
        height: dimensions.height,
        rate_num: plan.nominal_rate.num(),
        rate_den: plan.nominal_rate.den(),
        encoder_overrides: plan.encoder_overrides,
        bitrate_bps: plan.bitrate_bps,
        peak_bitrate_bps: plan.peak_bitrate_bps,
        gop_frames: plan.gop_frames,
        rate_control: match (plan.preset, plan.rate_control) {
            (frame_settings::EncoderPreset::Lossless, _) => {
                frame_windows::NativeRateControl::Lossless
            }
            (_, frame_settings::RateControl::Cbr) => frame_windows::NativeRateControl::Cbr,
            (_, frame_settings::RateControl::Vbr) => frame_windows::NativeRateControl::Vbr,
            (_, frame_settings::RateControl::Cqp) => frame_windows::NativeRateControl::Cqp,
        },
        preset: match plan.preset {
            frame_settings::EncoderPreset::Lossless => frame_windows::NativeEncoderPreset::Lossless,
            frame_settings::EncoderPreset::LowLatency => {
                frame_windows::NativeEncoderPreset::LowLatency
            }
            frame_settings::EncoderPreset::HighQuality => {
                frame_windows::NativeEncoderPreset::HighQuality
            }
            frame_settings::EncoderPreset::MaximumQuality => {
                frame_windows::NativeEncoderPreset::MaximumQuality
            }
        },
    };
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    fn key(driver_version: u64) -> Key {
        Key {
            candidate: "test".into(),
            adapter_luid: 1,
            driver_version,
            config: frame_windows::NativeEncoderConfig::proof(
                1920,
                1080,
                NonZeroU32::new(60).unwrap(),
                NonZeroU32::new(1).unwrap(),
            )
            .unwrap(),
            format: "bgra8".into(),
            transport: "cpu".into(),
            pattern: false,
        }
    }
    fn value() -> EncoderAssessment {
        EncoderAssessment {
            completed_frames: 48,
            elapsed_ns: 800_000_000,
            failures: 0,
            latency_p95_ns: None,
            source_cost_p95_ns: None,
            recording_overhead_p95_ns: None,
            dropped_frames: 0,
        }
    }
    #[test]
    fn cache_requires_same_driver_geometry_and_transfer_and_is_bounded() {
        let mut cache = Cache::default();
        let first = key(1);
        cache.insert(first.clone(), value());
        assert_eq!(cache.get(&first), Some(value()));
        let mut changed = first.clone();
        changed.transport = "shared_gpu".into();
        assert_eq!(cache.get(&changed), None);
        changed = first.clone();
        changed.config.width = 1280;
        assert_eq!(cache.get(&changed), None);
        changed = first.clone();
        changed.config.bitrate_bps += 1;
        assert_eq!(cache.get(&changed), None);
        changed = first.clone();
        changed.config.rate_num = NonZeroU32::new(30).unwrap();
        assert_eq!(cache.get(&changed), None);
        cache.insert(key(2), value());
        assert_eq!(cache.get(&first), None);
        for width in 1..40 {
            let mut next = key(2);
            next.config.width = width;
            cache.insert(next, value());
        }
        assert_eq!(cache.0.len(), CACHE_LIMIT);
        let mut failed = value();
        failed.failures = 1;
        let failed_key = key(3);
        cache.insert(failed_key.clone(), failed);
        assert_eq!(cache.get(&failed_key), Some(failed));
    }
    #[test]
    fn calibration_defaults_to_background_and_supports_disable() {
        assert!(enabled(None));
        assert!(!enabled(Some("0")));
        assert!(enabled(Some("1")));
    }

    #[test]
    fn first_recording_feedback_is_kept_without_an_idle_measurement() {
        let mut cache = Cache::default();
        let first = key(1);
        cache.track(first.clone());
        assert!(cache.get(&first).unwrap().capacity_millihz().is_none());
        let mut actual = value();
        actual.dropped_frames = 8;
        cache.observe(first.clone(), actual);
        assert_eq!(cache.get(&first), Some(actual));
        cache.track(first.clone());
        assert_eq!(cache.get(&first), Some(actual));
        cache.observe(first.clone(), value());
        assert_eq!(cache.get(&first), Some(actual));
    }

    #[test]
    #[ignore = "requires local NVIDIA hardware; writes short synthetic streams under C:/FREYM/build/recording-optimizer"]
    fn native_calibration_and_lossless_sequence() {
        use std::io::Write;
        let adapter = frame_windows::enumerate_native_adapters()
            .unwrap()
            .into_iter()
            .find(|a| a.vendor_id == 0x10de)
            .expect("NVIDIA calibration adapter");
        let directory = std::path::Path::new("C:/FREYM/build/recording-optimizer");
        std::fs::create_dir_all(directory).unwrap();
        for (name, preset) in [
            ("h264", frame_windows::NativeEncoderPreset::LowLatency),
            ("lossless", frame_windows::NativeEncoderPreset::Lossless),
        ] {
            let config = frame_windows::NativeEncoderConfig::with_quality(
                640,
                360,
                NonZeroU32::new(144).unwrap(),
                NonZeroU32::new(1).unwrap(),
                frame_windows::NativeQualitySettings {
                    bitrate_bps: if name == "lossless" { 0 } else { 8_000_000 },
                    peak_bitrate_bps: if name == "lossless" { 0 } else { 8_000_000 },
                    gop_frames: 144,
                    rate_control: if name == "lossless" {
                        frame_windows::NativeRateControl::Lossless
                    } else {
                        frame_windows::NativeRateControl::Cbr
                    },
                    preset,
                },
            )
            .unwrap();
            let mut stream = std::fs::File::create(directory.join(format!("{name}.h264"))).unwrap();
            let result = frame_windows::synthetic_encoder_sequence(
                &adapter,
                &config,
                Some(&format!(
                    "nvidia-nvenc-h264-d3d11:luid={:016X}",
                    adapter.luid
                )),
                FRAMES,
                || false,
                |_, packet| {
                    stream.write_all(&packet.data).unwrap();
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(result.stats.completed_frames, u64::from(FRAMES));
            assert_eq!(result.stats.rejected_inputs, 0);
            assert!(result.latency_p95_ns.is_some());
            assert!(result.overhead_p95_ns.is_some());
        }
    }

    #[test]
    #[ignore = "requires local NVIDIA AV1/Main10 hardware"]
    fn native_automatic_codecs_sequence() {
        use std::io::Write;
        let adapter = frame_windows::enumerate_native_adapters()
            .unwrap()
            .into_iter()
            .find(|a| a.vendor_id == 0x10de)
            .expect("NVIDIA calibration adapter");
        let directory = std::path::Path::new("C:/FREYM/build/recording-optimizer");
        std::fs::create_dir_all(directory).unwrap();
        let config = frame_windows::NativeEncoderConfig::with_quality(
            640,
            360,
            NonZeroU32::new(144).unwrap(),
            NonZeroU32::new(1).unwrap(),
            frame_windows::NativeQualitySettings {
                bitrate_bps: 8_000_000,
                peak_bitrate_bps: 12_000_000,
                gop_frames: 144,
                rate_control: frame_windows::NativeRateControl::Vbr,
                preset: frame_windows::NativeEncoderPreset::HighQuality,
            },
        )
        .unwrap();
        for (profile, extension) in [
            ("av1", "obu"),
            ("hevc-main10", "hevc"),
            ("av1-main10", "obu"),
        ] {
            let mut stream =
                std::fs::File::create(directory.join(format!("{profile}.{extension}"))).unwrap();
            let result = frame_windows::synthetic_encoder_sequence(
                &adapter,
                &config,
                Some(&format!(
                    "nvidia-nvenc-{profile}-d3d11:luid={:016X}",
                    adapter.luid
                )),
                FRAMES,
                || false,
                |_, packet| {
                    stream.write_all(&packet.data).unwrap();
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(result.stats.completed_frames, u64::from(FRAMES));
            assert_eq!(result.stats.rejected_inputs, 0);
            assert!(result.latency_p95_ns.is_some());
            assert!(result.overhead_p95_ns.is_some());
        }
    }
}
