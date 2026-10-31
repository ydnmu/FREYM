//! Bounded, explicit native meter verification. Never writes or plays PCM or starts recording.
#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use frame_audio::metering::MeterHub;
    use frame_contracts::AudioSourceKind;
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    if !std::env::args().any(|a| a == "--capture-levels") {
        return Err("Supply --capture-levels for bounded live input metering".into());
    }
    let args: Vec<_> = std::env::args().collect();
    let kind_filter = args
        .windows(2)
        .find(|v| v[0] == "--kind")
        .map(|v| v[1].as_str());
    let name_filter = args
        .windows(2)
        .find(|v| v[0] == "--source-name")
        .map(|v| v[1].to_lowercase());
    let discovered = frame_runtime::audio_discovery::discover()?;
    let mut results = vec![];
    for kind in [
        AudioSourceKind::Microphone,
        AudioSourceKind::System,
        AudioSourceKind::Application,
    ] {
        if kind_filter.is_some_and(|filter| filter != format!("{kind:?}").to_lowercase()) {
            continue;
        }
        let source = discovered
            .sources
            .iter()
            .filter(|s| s.kind == kind && s.is_active)
            .filter(|s| {
                name_filter
                    .as_ref()
                    .is_none_or(|name| s.display_name.to_lowercase().contains(name))
            })
            .find(|s| kind != AudioSourceKind::System || s.id.0.starts_with("wasapi-loopback:"));
        let Some(source) = source else {
            results.push(serde_json::json!({"kind":kind,"status":"no active source"}));
            continue;
        };
        let hub = Arc::new(MeterHub::default());
        let session = format!("ribbon-probe-{kind:?}");
        let start = frame_runtime::audio_levels::AudioLevelTest::start(
            std::slice::from_ref(&source.id),
            hub.clone(),
            session.clone(),
        );
        let mut capture = match start {
            Ok(capture) => capture,
            Err(error) => {
                results.push(
                    serde_json::json!({"kind":kind,"status":"unavailable","error":error.message}),
                );
                continue;
            }
        };
        let until = Instant::now() + Duration::from_secs(3);
        let mut readings = 0;
        let mut active = 0;
        let mut above_minus_80 = 0;
        let mut peak = 0f32;
        let mut rms = 0f32;
        let mut clips = 0;
        let mut error = None;
        while Instant::now() < until {
            if let Err(e) = capture.poll() {
                error = Some(e.message);
                break;
            }
            if let Some(snapshot) = hub.try_snapshot_fresh(Duration::from_millis(250)) {
                if snapshot.session_id.as_deref() != Some(session.as_str()) {
                    return Err("Foreign meter session".into());
                }
                for level in snapshot.sources {
                    if level.source_id != source.id {
                        return Err("Foreign source in native meter".into());
                    }
                    if let (Some(r), Some(p)) = (level.rms, level.peak) {
                        if !r.is_finite() || !p.is_finite() || r < 0. || r > p + 0.000001 {
                            return Err("Invalid native levels".into());
                        }
                        readings += 1;
                        active += usize::from(p > 0.);
                        above_minus_80 += usize::from(p > 0.0001);
                        peak = peak.max(p);
                        rms = rms.max(r);
                        clips += usize::from(level.clipping == Some(true));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        capture.stop()?;
        let stopped = hub.try_snapshot_fresh(Duration::from_millis(250)).unwrap();
        let clean_stop = stopped.session_id.is_none() && stopped.sources.is_empty();
        let db = |value: f32| {
            if value > 0. {
                Some(20. * value.log10())
            } else {
                None
            }
        };
        results.push(
            serde_json::json!({"kind":kind,"sourceName":source.display_name,"status":"captured","readings":readings,
            "nonzeroReadings":active,"aboveMinus80DbfsReadings":above_minus_80,"maxSamplePeakDbfs":db(peak),"maxWindowRmsDbfs":db(rms),
            "clipReadings":clips,"cleanStop":clean_stop,"error":error}),
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"pcmSaved":false,"recordingStarted":false,"secondsPerSource":3,"results":results})
        )?
    );
    Ok(())
}
#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("The native probe uses the existing Windows idle level-test owner.");
}
