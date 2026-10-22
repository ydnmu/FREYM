//! Bounded asynchronous Studio playback. Device-consumed audio drives video time.
//! No decoder, device creation, file I/O or thread join runs on the GPUI loop.
use crate::assembly::{audio_filter, preview_filter};
use crate::render::Process;
use crate::*;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use frame_clock::MediaTimestamp;
use std::{
    collections::VecDeque,
    io::Read,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub struct PlaybackFrame {
    pub time_ns: u64,
    pub png: Vec<u8>,
}
struct Shared {
    stop: AtomicBool,
    paused: AtomicBool,
    done: AtomicBool,
    time: AtomicU64,
    samples: AtomicU64,
    frame: Mutex<Option<PlaybackFrame>>,
    error: Mutex<Option<String>>,
    children: Mutex<Vec<Arc<Mutex<Process>>>>,
    pcm: Mutex<VecDeque<f32>>,
}
pub struct Playback {
    shared: Arc<Shared>,
}
impl Drop for Playback {
    fn drop(&mut self) {
        self.stop();
    }
}
impl Playback {
    pub fn start(
        tools: LocalTools,
        timeline: impl Into<Arc<Timeline>>,
        info: impl Into<Arc<MediaInfo>>,
        time_ns: u64,
        track: Option<usize>,
    ) -> Self {
        let timeline = timeline.into();
        let info = info.into();
        let time_ns = time_ns.min(timeline.duration_ns.saturating_sub(1));
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            done: AtomicBool::new(false),
            time: AtomicU64::new(time_ns),
            samples: AtomicU64::new(0),
            frame: Mutex::new(None),
            error: Mutex::new(None),
            children: Mutex::new(Vec::new()),
            pcm: Mutex::new(VecDeque::new()),
        });
        let state = shared.clone();
        thread::spawn(move || {
            let result = (|| -> StudioResult<()> {
                let timeline = if timeline.version == 1 {
                    let mut upgraded = (*timeline).clone();
                    upgraded.upgrade();
                    Arc::new(upgraded)
                } else {
                    timeline
                };
                timeline.verify_source()?;
                let info = if info.segments.is_empty() {
                    Arc::new(tools.inspect_timeline(&timeline)?)
                } else {
                    info
                };
                let video_state = state.clone();
                let video_tools = tools.clone();
                let video_timeline = timeline.clone();
                let video_info = info.clone();
                let video = thread::spawn(move || {
                    if let Err(e) = video_loop(
                        &video_tools,
                        &video_timeline,
                        &video_info,
                        &video_state,
                        time_ns,
                    ) {
                        fail(&video_state, e);
                    }
                });
                let result = if let Some(index) = track.filter(|i| *i < info.audio.len()) {
                    audio_loop(&tools, &timeline, &info, &state, time_ns, index)
                } else {
                    let mut previous = Instant::now();
                    while !state.stop.load(Ordering::Relaxed)
                        && state.time.load(Ordering::Relaxed) < timeline.duration_ns
                    {
                        thread::sleep(Duration::from_millis(4));
                        let now = Instant::now();
                        if !state.paused.load(Ordering::Relaxed) {
                            state.time.fetch_add(
                                now.duration_since(previous).as_nanos() as u64,
                                Ordering::Relaxed,
                            );
                        }
                        previous = now;
                    }
                    Ok(())
                };
                if let Err(e) = result {
                    fail(&state, e);
                }
                state.stop.store(true, Ordering::Relaxed);
                let _ = video.join();
                Ok(())
            })();
            if let Err(e) = result {
                fail(&state, e);
            }
            state.stop.store(true, Ordering::Relaxed);
            state.done.store(true, Ordering::Relaxed);
        });
        let control = shared.clone();
        thread::spawn(move || {
            while !control.stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(10));
            }
            for child in control
                .children
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
            {
                if let Ok(mut child) = child.lock() {
                    let _ = child.0.kill();
                }
            }
        });
        Self { shared }
    }
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
    pub fn pause(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }
    pub fn time_ns(&self) -> u64 {
        self.shared.time.load(Ordering::Relaxed)
    }
    pub fn finished(&self) -> bool {
        self.shared.done.load(Ordering::Relaxed)
    }
    pub fn take_frame(&self) -> Option<PlaybackFrame> {
        self.shared.frame.try_lock().ok()?.take()
    }
    pub fn take_error(&self) -> Option<String> {
        self.shared.error.try_lock().ok()?.take()
    }
}
fn fail(state: &Shared, error: String) {
    if !state.stop.load(Ordering::Relaxed) {
        *state.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
    }
    state.stop.store(true, Ordering::Relaxed);
}
fn spawn_decoder(
    mut cmd: std::process::Command,
    state: &Shared,
) -> StudioResult<(Arc<Mutex<Process>>, std::process::ChildStdout)> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Studio decoder unavailable: {e}"))?;
    let stdout = child.stdout.take().ok_or("Decoder pipe unavailable.")?;
    let process = Arc::new(Mutex::new(Process(child)));
    let mut children = state
        .children
        .lock()
        .map_err(|_| "Playback process lock failed.")?;
    if state.stop.load(Ordering::Relaxed) {
        let _ = process.lock().unwrap().0.kill();
        return Err("Playback canceled.".into());
    }
    children.push(process.clone());
    Ok((process, stdout))
}
fn retire(process: Arc<Mutex<Process>>, state: &Shared) -> StudioResult<()> {
    // EOF can precede process exit. Wait off the UI loop so decoder failures
    // cannot be mistaken for a clean end or silently replaced with silence.
    let status = loop {
        let mut child = process.lock().map_err(|_| "Decoder process lock failed.")?;
        if let Some(status) = child.0.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if state.stop.load(Ordering::Relaxed) {
            let _ = child.0.kill();
        }
        drop(child);
        thread::sleep(Duration::from_millis(2));
    };
    if !status.success() && !state.stop.load(Ordering::Relaxed) {
        return Err("Studio decoder failed. The original media remains intact.".into());
    }
    state
        .children
        .lock()
        .map_err(|_| "Playback process lock failed.")?
        .retain(|p| !Arc::ptr_eq(p, &process));
    Ok(())
}
fn video_loop(
    tools: &LocalTools,
    timeline: &Timeline,
    info: &MediaInfo,
    state: &Shared,
    start_ns: u64,
) -> StudioResult<()> {
    let factor = (960.0 / info.width as f64)
        .min(540.0 / info.height as f64)
        .min(1.0);
    let width = (info.width as f64 * factor).round().max(1.) as u32;
    let height = (info.height as f64 * factor).round().max(1.) as u32;
    let mut offset = 0u64;
    for clip in &timeline.clips {
        let end_time = offset + clip.duration_ns();
        if end_time <= start_ns {
            offset = end_time;
            continue;
        }
        let s = &info.segments[clip.source_index];
        let local = clip.in_ns + start_ns.saturating_sub(offset);
        let first = s
            .frames
            .partition_point(|(t, _)| t.0 <= local)
            .saturating_sub(1);
        let end = s.frames.partition_point(|(t, _)| t.0 < clip.out_ns);
        let mut cmd = command(&tools.ffmpeg);
        let seek = s.frames[first].0.0;
        let graph = format!(
            "select=gte(t\\,{:.9}),{}",
            seek as f64 / 1e9,
            preview_filter(s, width, height)?
        );
        cmd.args([
            "-v",
            "error",
            "-xerror",
            "-nostdin",
            "-copyts",
            "-noautorotate",
            "-ss",
            &format!("{:.9}", seek as f64 / 1e9),
            "-i",
        ])
        .arg(&timeline.sources[clip.source_index].path)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-vf",
            &graph,
            "-frames:v",
            &(end - first).to_string(),
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "pipe:1",
        ]);
        let (process, mut stdout) = spawn_decoder(cmd, state)?;
        let mut rgb = vec![0; width as usize * height as usize * 3];
        for index in first..end {
            if state.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            stdout
                .read_exact(&mut rgb)
                .map_err(|e| format!("Playback video decode stopped: {e}"))?;
            let at = offset + s.frames[index].0.0.max(clip.in_ns) - clip.in_ns;
            let next = if index + 1 < end {
                offset + s.frames[index + 1].0.0 - clip.in_ns
            } else {
                end_time
            };
            while !state.stop.load(Ordering::Relaxed)
                && (state.paused.load(Ordering::Relaxed) || state.time.load(Ordering::Relaxed) < at)
            {
                thread::sleep(Duration::from_millis(2));
            }
            if state.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if state.time.load(Ordering::Relaxed) >= next {
                continue;
            }
            let mut previous = None;
            while !state.stop.load(Ordering::Relaxed) && state.time.load(Ordering::Relaxed) < next {
                let time = state.time.load(Ordering::Relaxed).max(at).min(next - 1);
                if previous.is_none_or(|old: u64| time.saturating_sub(old) >= 33_000_000) {
                    let png = encode_png(
                        width,
                        height,
                        &render_rgb(timeline, MediaTimestamp(time), width, height, &rgb),
                    )?;
                    *state.frame.lock().map_err(|_| "Playback mailbox failed.")? =
                        Some(PlaybackFrame { time_ns: time, png });
                    previous = Some(time);
                }
                thread::sleep(Duration::from_millis(3));
            }
        }
        retire(process, state)?;
        offset = end_time;
    }
    Ok(())
}
fn output_stream<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    state: Arc<Shared>,
    start: u64,
) -> StudioResult<cpal::Stream> {
    let rate = config.sample_rate.0 as u64;
    let channels = config.channels as u64;
    let error_state = state.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                data.fill(T::EQUILIBRIUM);
                if state.stop.load(Ordering::Relaxed) || state.paused.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(mut queue) = state.pcm.try_lock() else {
                    return;
                };
                if queue.len() < data.len() {
                    return;
                }
                for sample in data.iter_mut() {
                    *sample = T::from_sample(queue.pop_front().unwrap());
                }
                let samples = state
                    .samples
                    .fetch_add(data.len() as u64, Ordering::Relaxed)
                    + data.len() as u64;
                let latency = info
                    .timestamp()
                    .playback
                    .duration_since(&info.timestamp().callback)
                    .map_or(0, |d| d.as_nanos() as u64);
                // CPAL's playback timestamp is for the first sample of this
                // buffer. Submitted samples include its end, which has not yet
                // played. Subtract both the buffer and device latency.
                state.time.fetch_max(
                    start
                        + ((samples - data.len() as u64) / channels * 1_000_000_000 / rate)
                            .saturating_sub(latency),
                    Ordering::Relaxed,
                );
            },
            move |e| fail(&error_state, format!("Studio audio output failed: {e}")),
            Some(Duration::from_secs(2)),
        )
        .map_err(|e| format!("Studio audio output unavailable: {e}"))
}
fn audio_loop(
    tools: &LocalTools,
    timeline: &Timeline,
    info: &MediaInfo,
    state: &Arc<Shared>,
    start: u64,
    track: usize,
) -> StudioResult<()> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("No Studio audio output device. Choose Silent preview to play video.")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let stream = match format {
        cpal::SampleFormat::F32 => output_stream::<f32>(&device, &config, state.clone(), start)?,
        cpal::SampleFormat::I16 => output_stream::<i16>(&device, &config, state.clone(), start)?,
        cpal::SampleFormat::U16 => output_stream::<u16>(&device, &config, state.clone(), start)?,
        _ => {
            return Err(
                "Studio audio output sample format is unsupported. Choose Silent preview.".into(),
            );
        }
    };
    stream.play().map_err(|e| e.to_string())?;
    let selected = &info.audio[track];
    let mut offset = 0;
    for original in &timeline.clips {
        let end = offset + original.duration_ns();
        if end <= start {
            offset = end;
            continue;
        }
        let mut clip = original.clone();
        clip.in_ns += start.saturating_sub(offset);
        let s = &info.segments[clip.source_index];
        let mut cmd = command(&tools.ffmpeg);
        cmd.args(["-v", "error", "-xerror", "-nostdin"]);
        if let Some(index) = s.audio.iter().position(|a| a.same_track(selected)) {
            cmd.args(["-copyts", "-i"])
                .arg(&timeline.sources[clip.source_index].path)
                .args([
                    "-map",
                    &format!("0:a:{index}"),
                    "-af",
                    &audio_filter(&clip, config.sample_rate.0),
                ]);
        } else {
            cmd.args([
                "-f",
                "lavfi",
                "-i",
                &format!("anullsrc=r={}:cl=stereo", config.sample_rate.0),
                "-t",
                &format!("{:.9}", clip.duration_ns() as f64 / 1e9),
            ]);
        }
        cmd.args([
            "-vn",
            "-ac",
            &config.channels.to_string(),
            "-ar",
            &config.sample_rate.0.to_string(),
            "-f",
            "f32le",
            "pipe:1",
        ]);
        let (process, mut stdout) = spawn_decoder(cmd, state)?;
        let chunk = (config.sample_rate.0 as usize / 100 * config.channels as usize)
            .max(config.channels as usize)
            * 4;
        let mut bytes = vec![0u8; chunk];
        loop {
            if state.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let mut count = 0;
            while count < chunk {
                let n = stdout
                    .read(&mut bytes[count..])
                    .map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                count += n;
            }
            if count == 0 {
                break;
            }
            if count % 4 != 0 {
                return Err("Studio audio decoder returned incomplete samples.".into());
            }
            loop {
                if state.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let mut queue = state.pcm.lock().map_err(|_| "Studio audio queue failed.")?;
                if queue.len() + count / 4
                    <= config.sample_rate.0 as usize * config.channels as usize / 2
                {
                    queue.extend(
                        bytes[..count]
                            .chunks_exact(4)
                            .map(|b| f32::from_le_bytes(b.try_into().unwrap())),
                    );
                    break;
                }
                drop(queue);
                thread::sleep(Duration::from_millis(3));
            }
        }
        retire(process, state)?;
        offset = end;
    }
    // Keep supplying silence until the last media sample reaches the device.
    // Counting submitted samples would close the stream before its tail plays.
    let tail = config.sample_rate.0 as usize * config.channels as usize / 10;
    while !state.stop.load(Ordering::Relaxed)
        && state.time.load(Ordering::Relaxed) < timeline.duration_ns
    {
        let mut queue = state.pcm.lock().map_err(|_| "Studio audio queue failed.")?;
        if queue.len() < tail {
            queue.extend(std::iter::repeat_n(0., tail));
        }
        drop(queue);
        thread::sleep(Duration::from_millis(4));
    }
    if !state.stop.load(Ordering::Relaxed) {
        state.time.store(timeline.duration_ns, Ordering::Relaxed);
    }
    Ok(())
}
