//! Editing presentation only. The local renderer and edit model have no GPUI dependency.
use crate::{
    input::TextInput,
    ui::{components::*, overlays::Modal, primitives::*, theme::colors::Color},
};
use frame_studio::*;
use gpui::{Context, Entity, Focusable, Window, div, img, prelude::*, px};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

type ZoomFields = (usize, Vec<(&'static str, Entity<TextInput>)>);
type CloseCallback = Box<dyn FnOnce(&mut gpui::App)>;

pub struct Studio {
    source: PathBuf,
    inspector_tab: usize,
    timeline: Option<Arc<Timeline>>,
    media: Option<Arc<MediaInfo>>,
    image: Option<Arc<gpui::RenderImage>>,
    image_busy: bool,
    playback_poll: Option<gpui::Task<()>>,
    position: usize,
    busy: bool,
    preview_busy: bool,
    revision: u64,
    message: String,
    fields: Option<ZoomFields>,
    seek: Entity<TextInput>,
    profile: ExportProfile,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    exporting: bool,
    closing: bool,
    player: Option<Playback>,
    playing: bool,
    playhead_ns: u64,
    audio_track: Option<usize>,
    selected_clip: usize,
    timeline_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    scrubbing: bool,
    undo: Option<Arc<Timeline>>,
    redo: Option<Arc<Timeline>>,
    focus: gpui::FocusHandle,
    resume_after_scrub: bool,
    focus_editor: bool,
    export_settings: bool,
    saved_timeline: Option<Arc<Timeline>>,
    close_requested: bool,
    close_prompt_pending: bool,
    save_on_close: bool,
    close_waiters: Vec<async_channel::Sender<bool>>,
    on_closed: Option<CloseCallback>,
    retired_players: Vec<Playback>,
    player_cleanup: Option<gpui::Task<()>>,
}
#[derive(Clone, Copy)]
enum Edit {
    Suggest,
    Add,
    Toggle(usize),
    Adjust(usize),
    Remove(usize),
    Easing(usize),
    DiscardMetadata,
    Apply,
    CancelFields,
    Cursor,
    Smoothing,
    Emphasis,
    Size,
    Baked,
    SetProfile(ExportProfile),
    Split,
    TrimStart,
    TrimEnd,
    MoveClip(usize, usize),
    Undo,
    Redo,
    SetColour(OutputColour),
}
impl Studio {
    pub fn new(
        source: PathBuf,
        on_closed: impl FnOnce(&mut gpui::App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let _ = view.update(cx, |this, cx| this.close(window, cx));
            false
        });
        cx.observe_window_visibility(window, |this, visibility, _, cx| {
            if !visibility.is_visible() && this.playing {
                if let Some(player) = &this.player {
                    player.pause(true);
                }
                this.playing = false;
                this.playback_poll = None;
                cx.notify();
            }
        })
        .detach();
        let this = Self {
            source,
            inspector_tab: 0,
            timeline: None,
            media: None,
            image: None,
            image_busy: false,
            playback_poll: None,
            position: 0,
            busy: true,
            preview_busy: false,
            revision: 0,
            message: "Reading local media and metadata…".into(),
            fields: None,
            seek: cx.new(|cx| TextInput::new("0", "Seconds", cx)),
            profile: ExportProfile::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(AtomicU64::new(0)),
            exporting: false,
            closing: false,
            player: None,
            playing: false,
            playhead_ns: 0,
            audio_track: Some(0),
            selected_clip: 0,
            timeline_bounds: None,
            scrubbing: false,
            undo: None,
            redo: None,
            focus: cx.focus_handle().tab_stop(true),
            resume_after_scrub: false,
            focus_editor: false,
            export_settings: false,
            saved_timeline: None,
            close_requested: false,
            close_prompt_pending: false,
            save_on_close: false,
            close_waiters: Vec::new(),
            on_closed: Some(Box::new(on_closed)),
            retired_players: Vec::new(),
            player_cleanup: None,
        };
        cx.defer_in(window, |this, window, cx| {
            window.focus(&this.focus, cx);
            this.load(cx)
        });
        this
    }
    pub(crate) fn activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.activate_window();
        window.focus(&self.focus, cx);
    }
    pub(crate) fn request_close(
        &mut self,
        waiter: async_channel::Sender<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_waiters.push(waiter);
        self.activate(window, cx);
        self.close(window, cx);
    }
    fn resolve_close(&mut self, closed: bool) {
        for waiter in self.close_waiters.drain(..) {
            let _ = waiter.try_send(closed);
        }
    }
    fn finish_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.resolve_close(true);
        if let Some(done) = self.on_closed.take() {
            done(cx);
        }
        window.remove_window();
    }
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || self.close_prompt_pending {
            return;
        }
        self.stop_playback();
        if self.busy && !self.exporting {
            self.close_requested = true;
            self.message = "Finishing local work before closing…".into();
            cx.notify();
            return;
        }
        let dirty = self.timeline.as_ref().is_some_and(|t| {
            self.saved_timeline
                .as_ref()
                .is_none_or(|saved| !Arc::ptr_eq(t, saved))
        });
        if dirty || self.exporting || self.fields.is_some() {
            self.close_prompt_pending = true;
            let choices: &[&str] = if dirty {
                &["Save and close", "Discard edits", "Cancel"]
            } else {
                &["Close Studio", "Keep open"]
            };
            let answer = window.prompt(
                gpui::PromptLevel::Warning,
                if dirty {
                    "Save edits before closing Studio?"
                } else {
                    "Close Studio?"
                },
                Some(if self.exporting {
                    "Closing cancels the active export. Original recordings are preserved."
                } else if self.fields.is_some() {
                    "Unapplied field changes will be discarded. Original recordings are preserved."
                } else {
                    "Original recordings are preserved."
                }),
                choices,
                cx,
            );
            cx.spawn_in(window, async move |view, cx| {
                let choice = answer.await.ok();
                let _ = view.update_in(cx, |this, window, cx| {
                    this.close_prompt_pending = false;
                    let accepted = if dirty {
                        matches!(choice, Some(0 | 1))
                    } else {
                        choice == Some(0)
                    };
                    if accepted {
                        this.save_on_close = dirty && choice == Some(0);
                        this.begin_close(window, cx);
                    } else {
                        this.close_requested = false;
                        this.resolve_close(false);
                        this.activate(window, cx);
                        cx.notify();
                    }
                });
            })
            .detach();
            return;
        }
        self.begin_close(window, cx);
    }
    fn begin_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_requested = false;
        self.closing = true;
        self.stop_playback();
        if self.busy
            || self.preview_busy
            || self.image_busy
            || self.save_on_close
            || !self.retired_players.is_empty()
        {
            self.cancel.store(true, Ordering::Relaxed);
            self.message = "Finishing local work before closing…".into();
        } else {
            self.finish_close(window, cx);
        }
        cx.notify();
    }
    fn load(&mut self, cx: &mut Context<Self>) {
        let source = self.source.clone();
        let task = cx
            .background_executor()
            .spawn(async move { LocalTools::default().open(&source) });
        cx.spawn(async move |view,cx| { let result=task.await; let _=view.update(cx,|this,cx| {
            this.busy=false;
            match result {
                Ok((timeline,media))=> {
                    this.message=if let Some(m)=&timeline.metadata { format!("{} interaction events{}. Edits leave the source intact.",m.events.len(),if m.truncated {" · metadata contains gaps"} else {""}) } else {"No interaction metadata. Add manual zooms, or enable metadata before your next recording.".into()};
                    if !timeline.warnings.is_empty() { this.message.push_str(&format!(" {}",timeline.warnings.join(" "))); }
                    if media.hdr() { this.message.push_str(" HDR preview is tone mapped to SDR; exports preserve HDR."); }
                    this.profile=media.default_export_profile();
                    this.source=timeline.source.clone(); this.timeline=Some(Arc::new(timeline)); this.saved_timeline=this.timeline.clone(); this.media=Some(Arc::new(media)); if !this.close_requested {this.preview(cx);}
                }
                Err(error)=>this.message=error,
            }
            cx.notify();
        }); }).detach();
    }
    fn time(&self) -> u64 {
        self.playhead_ns
    }
    fn stop_playback(&mut self) {
        self.revision += 1;
        self.playback_poll = None;
        if let Some(player) = self.player.take() {
            player.stop();
            self.retired_players.push(player);
        }
        self.playing = false;
    }
    fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.closing || self.close_requested || self.close_prompt_pending {
            return;
        }
        if let Some(player) = &self.player {
            self.playing = !self.playing;
            player.pause(!self.playing);
        } else if let (Some(timeline), Some(media)) = (&self.timeline, &self.media) {
            if self.playhead_ns >= timeline.duration_ns.saturating_sub(1) {
                self.playhead_ns = 0;
            }
            self.revision += 1;
            self.player = Some(Playback::start(
                LocalTools::default(),
                timeline.clone(),
                media.clone(),
                self.playhead_ns,
                self.audio_track,
            ));
            self.playing = true;
        }
        if self.playing && self.playback_poll.is_none() {
            self.playback_poll = Some(cx.spawn(async move |view, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(16))
                        .await;
                    if !view
                        .update(cx, |this, cx| {
                            this.poll_playback(cx);
                            this.playing
                        })
                        .unwrap_or(false)
                    {
                        break;
                    }
                }
            }));
        } else if !self.playing {
            self.playback_poll = None;
        }
        cx.notify();
    }
    fn poll_playback(&mut self, cx: &mut Context<Self>) {
        let Some(player) = &self.player else {
            return;
        };
        let old = self.playhead_ns;
        self.playhead_ns = player.time_ns().min(
            self.timeline
                .as_ref()
                .map_or(0, |t| t.duration_ns.saturating_sub(1)),
        );
        if let Some(media) = &self.media {
            self.position = media
                .frames
                .partition_point(|(t, _)| t.0 <= self.playhead_ns)
                .saturating_sub(1);
        }
        if let Some((i, _, _)) = self
            .timeline
            .as_ref()
            .and_then(|t| t.locate(self.playhead_ns))
        {
            self.selected_clip = i;
        }
        let frame = if self.image_busy {
            None
        } else {
            player.take_frame()
        };
        let received = frame.is_some();
        if let Some(frame) = frame {
            // Present only a decoded image. A fresh asynchronous Image asset per
            // frame otherwise clears the preview while its PNG is loading.
            self.image_busy = true;
            let revision = self.revision;
            let renderer = cx.svg_renderer().clone();
            let task = cx.background_executor().spawn(async move {
                gpui::Image::from_bytes(gpui::ImageFormat::Png, frame.png).to_image_data(renderer)
            });
            cx.spawn(async move |view, cx| {
                let result = task.await;
                let _ = view.update(cx, |this, cx| {
                    this.image_busy = false;
                    if revision == this.revision && !this.closing {
                        match result {
                            Ok(image) => this.image = Some(image),
                            Err(error) => {
                                this.message = format!("Could not display the preview: {error}")
                            }
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        if let Some(error) = player.take_error() {
            self.message = error;
        }
        let done = player.finished();
        if done {
            self.player.take();
            self.playing = false;
            self.playback_poll = None;
            // The bounded mailbox can finish before its last image is ready.
            // Resolve the final playhead once rather than leave an older frame.
            self.preview(cx);
            cx.notify();
        } else if received || old != self.playhead_ns {
            cx.notify();
        }
    }
    fn seek_to(&mut self, ns: u64, cx: &mut Context<Self>) {
        if self.closing || self.close_requested || self.close_prompt_pending {
            return;
        }
        let resume = self.playing && !self.scrubbing;
        self.stop_playback();
        if let Some(media) = &self.media {
            self.playhead_ns = ns.min(media.duration_ns.saturating_sub(1));
            self.position = media
                .frames
                .partition_point(|(t, _)| t.0 <= self.playhead_ns)
                .saturating_sub(1);
            if let Some(t) = &self.timeline
                && let Some((i, _, _)) = t.locate(self.playhead_ns)
            {
                self.selected_clip = i;
            }
            self.preview(cx);
        }
        if resume {
            self.toggle_playback(cx);
        }
        cx.notify();
    }
    fn finish_scrub(&mut self, cx: &mut Context<Self>) {
        self.scrubbing = false;
        if std::mem::take(&mut self.resume_after_scrub) && !self.busy {
            self.toggle_playback(cx);
        }
    }
    fn scrub(&mut self, point: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if let (Some(bounds), Some(timeline)) = (self.timeline_bounds, self.timeline.as_ref()) {
            let fraction = ((point.x - bounds.left()) / bounds.size.width).clamp(0., 1.);
            self.seek_to((timeline.duration_ns as f64 * fraction as f64) as u64, cx);
        }
    }
    fn preview(&mut self, cx: &mut Context<Self>) {
        self.revision += 1;
        if self.preview_busy {
            return;
        }
        let (Some(timeline), Some(media)) = (self.timeline.clone(), self.media.clone()) else {
            return;
        };
        let revision = self.revision;
        let time = self.time();
        self.preview_busy = true;
        let renderer = cx.svg_renderer().clone();
        let task = cx.background_executor().spawn(async move {
            let mut preview = (*media).clone();
            let scale = (960.0 / preview.width as f64)
                .min(540.0 / preview.height as f64)
                .min(1.0);
            preview.width = (preview.width as f64 * scale).round().max(1.0) as u32;
            preview.height = (preview.height as f64 * scale).round().max(1.0) as u32;
            let rgb = LocalTools::default().decode_frame(&timeline, &preview, time)?;
            let bytes = encode_png(preview.width, preview.height, &rgb)?;
            gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes)
                .to_image_data(renderer)
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.preview_busy = false;
                if revision == this.revision && this.player.is_none() {
                    match result {
                        Ok(image) => this.image = Some(image),
                        Err(error) => this.message = error,
                    }
                } else if this.player.is_none() && !this.closing {
                    this.preview(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn seek_frames(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(m) = &self.media {
            self.position = self
                .position
                .saturating_add_signed(delta)
                .min(m.frames.len() - 1);
            self.seek_to(m.frames[self.position].0.0, cx);
        }
    }
    fn seek_seconds(&mut self, cx: &mut Context<Self>) {
        match self.seek.read(cx).value().trim().parse::<f64>() {
            Ok(seconds) if seconds.is_finite() && seconds >= 0.0 => {
                if let Some(m) = &self.media {
                    self.position = m
                        .frames
                        .partition_point(|(t, _)| t.0 as f64 / 1e9 <= seconds)
                        .saturating_sub(1);
                    self.seek_to((seconds * 1e9) as u64, cx);
                }
            }
            _ => self.message = "Enter a non-negative time in seconds.".into(),
        }
        cx.notify();
    }
    fn edit(&mut self, edit: Edit, cx: &mut Context<Self>) {
        if self.closing || self.close_requested || self.close_prompt_pending {
            return;
        }
        if self.busy {
            return;
        }
        let time = self.time();
        self.stop_playback();
        if matches!(
            edit,
            Edit::Split
                | Edit::TrimStart
                | Edit::TrimEnd
                | Edit::MoveClip(..)
                | Edit::Undo
                | Edit::Redo
        ) {
            self.edit_clips(edit, cx);
            return;
        }
        if let Edit::Adjust(index) = edit {
            self.focus_editor = true;
            if let Some(z) = self.timeline.as_ref().and_then(|t| t.zooms.get(index)) {
                let values = [
                    ("Start (seconds)", z.start_ns as f64 / 1e9),
                    ("End (seconds)", z.end_ns as f64 / 1e9),
                    ("Zoom", z.scale),
                    ("Ramp (seconds)", z.ramp_ns as f64 / 1e9),
                    ("Focus X (0–1)", z.focus.x),
                    ("Focus Y (0–1)", z.focus.y),
                ];
                self.fields = Some((
                    index,
                    values
                        .into_iter()
                        .map(|(label, value)| {
                            (
                                label,
                                cx.new(|cx| TextInput::new(format!("{value:.3}"), "", cx)),
                            )
                        })
                        .collect(),
                ));
            }
            cx.notify();
            return;
        }
        if let Edit::CancelFields = edit {
            self.fields = None;
            self.focus_editor = true;
            cx.notify();
            return;
        }
        if let Edit::SetProfile(profile) = edit {
            self.profile = profile;
            cx.notify();
            return;
        }
        let apply_fields = if let Edit::Apply = edit {
            let Some((index, fields)) = &self.fields else {
                return;
            };
            let values: Result<Vec<f64>, _> = fields
                .iter()
                .map(|(_, f)| f.read(cx).value().trim().parse::<f64>())
                .collect();
            let values = match values {
                Ok(v) if v.iter().all(|v| v.is_finite() && *v >= 0.0) => v,
                _ => {
                    self.message = "Enter finite non-negative values.".into();
                    cx.notify();
                    return;
                }
            };
            Some((*index, values))
        } else {
            None
        };
        let Some(original) = self.timeline.as_ref() else {
            return;
        };
        let original = original.clone();
        let previous = original.clone();
        self.busy = true;
        let task = cx.background_executor().spawn(async move {
            let mut timeline = (*original).clone();
            match edit {
                Edit::Suggest => timeline.suggest_zooms(2.0),
                Edit::Add => {
                    let start = time.min(timeline.duration_ns.saturating_sub(200_000_000));
                    let end = (start + 2_000_000_000).min(timeline.duration_ns);
                    timeline.zooms.push(ZoomSegment {
                        enabled: true,
                        start_ns: start,
                        end_ns: end,
                        ramp_ns: 350_000_000.min((end - start) / 2),
                        scale: 2.0,
                        focus: timeline
                            .cursor_at(time)
                            .map_or(Point { x: 0.5, y: 0.5 }, |(p, _)| p),
                        easing: Easing::Smooth,
                    });
                    timeline.zooms.sort_by_key(|z| z.start_ns);
                }
                Edit::Toggle(i) => timeline.zooms[i].enabled = !timeline.zooms[i].enabled,
                Edit::Remove(i) => {
                    timeline.zooms.remove(i);
                }
                Edit::Easing(i) => {
                    timeline.zooms[i].easing = match timeline.zooms[i].easing {
                        Easing::Smooth => Easing::Linear,
                        Easing::Linear => Easing::Smooth,
                    };
                }
                Edit::Apply => {
                    let (index, values) = apply_fields.ok_or("No zoom edit is active.")?;
                    let z = timeline
                        .zooms
                        .get_mut(index)
                        .ok_or("The selected zoom no longer exists.")?;
                    z.start_ns = (values[0] * 1e9) as u64;
                    z.end_ns = (values[1] * 1e9) as u64;
                    z.scale = values[2];
                    z.ramp_ns = (values[3] * 1e9) as u64;
                    z.focus = Point {
                        x: values[4],
                        y: values[5],
                    };
                    timeline.zooms.sort_by_key(|z| z.start_ns);
                }
                Edit::Cursor => {
                    timeline.cursor.enabled = !timeline.cursor.enabled;
                    if timeline.metadata.as_ref().is_some_and(|m| m.cursor_baked) {
                        timeline.cursor.smoothing_ms = 0;
                    }
                }
                Edit::Smoothing => {
                    timeline.cursor.smoothing_ms = match timeline.cursor.smoothing_ms {
                        0 => 60,
                        60 => 90,
                        90 => 150,
                        _ => 0,
                    }
                }
                Edit::Emphasis => timeline.cursor.emphasis = !timeline.cursor.emphasis,
                Edit::Size => {
                    timeline.cursor.size = if timeline.cursor.size >= 2.0 {
                        1.0
                    } else {
                        timeline.cursor.size + 0.5
                    }
                }
                Edit::SetColour(colour) => timeline.output_colour = colour,
                Edit::Baked => {
                    if let Some(m) = &mut timeline.metadata {
                        m.cursor_baked = !m.cursor_baked;
                        if m.cursor_baked {
                            timeline.cursor.smoothing_ms = 0;
                        }
                        let baked = m.cursor_baked;
                        for source in &mut timeline.sources {
                            if let Some(m) = &mut source.metadata {
                                m.cursor_baked = baked;
                            }
                        }
                    }
                }
                Edit::DiscardMetadata => {
                    timeline.metadata = None;
                    for source in &mut timeline.sources {
                        source.metadata = None;
                    }
                    timeline.cursor.enabled = false;
                }
                _ => (),
            }
            timeline.validate()?;
            Ok::<_, String>(timeline)
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(timeline) => {
                        this.undo = Some(previous);
                        this.redo = None;
                        this.timeline = Some(Arc::new(timeline));
                        if matches!(edit, Edit::Apply) {
                            this.fields = None;
                            this.focus_editor=true;
                        }
                        if matches!(edit,Edit::SetColour(..)) && this.timeline.as_ref().is_some_and(|t|matches!(t.output_colour,OutputColour::HdrPq|OutputColour::HdrHlg)) && matches!(this.profile,ExportProfile::H264{..}) {this.profile=ExportProfile::Hevc{crf:18};}
                        this.message = if matches!(edit, Edit::DiscardMetadata) {
                            "Event snapshot discarded. Save edits to retain this change. The original recording sidecar remains separate.".into()
                        } else if matches!(edit,Edit::SetColour(..)) {
                            "Output colour updated. Save edits to keep this choice. HDR preview is shown in SDR.".into()
                        } else {
                            "Edits changed. Save the edit plan to keep them.".into()
                        };
                        this.preview(cx);
                    }
                    Err(error) => this.message = error,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn save(&mut self, cx: &mut Context<Self>) {
        if self.closing || self.close_requested || self.close_prompt_pending {
            return;
        }
        let Some(timeline) = self.timeline.clone() else {
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        let path = sidecar_path(&timeline.source, ".freym-edit.json");
        let saved = timeline.clone();
        let task = cx
            .background_executor()
            .spawn(async move { timeline.save(&path) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.busy = false;
                this.message = match result {
                    Ok(()) => {
                        this.saved_timeline = Some(saved);
                        "Edit plan saved beside the recording.".into()
                    }
                    Err(e) => e,
                };
                cx.notify();
            });
        })
        .detach();
    }
    fn append_recording(&mut self, cx: &mut Context<Self>) {
        if self.closing || self.close_requested || self.close_prompt_pending {
            return;
        }
        if self.busy {
            return;
        }
        let Some(original) = self.timeline.clone() else {
            return;
        };
        self.stop_playback();
        self.busy = true;
        let answer = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Append recordings or replay clips".into()),
        });
        cx.spawn(async move |view,cx| {
            let paths=match answer.await {Ok(Ok(Some(paths)))=>paths,_=>Vec::new()};
            if paths.is_empty() {let _=view.update(cx,|this,cx|{this.busy=false;cx.notify();});return;}
            let previous=original.clone();
            let task=cx.background_executor().spawn(async move {
                let tools=LocalTools::default();let mut timeline=(*original).clone();
                for path in paths {
                    let (added,_)=tools.open(&path).map_err(|e|format!("{}: {e} No clips were appended.",path.display()))?;
                    timeline.append(added).map_err(|e|format!("{}: {e} No clips were appended.",path.display()))?;
                }
                let media=tools.inspect_timeline(&timeline)?;
                Ok::<_,String>((timeline,media))
            });
            let result=task.await;
            let _=view.update(cx,|this,cx| {
                this.busy=false;
                match result {
                    Ok((timeline,media))=>{this.undo=Some(previous);this.redo=None;
                        this.message="Recordings appended. Segments use the first clip's canvas and retain their frame timestamps and independent audio tracks. Choose Output colour for HDR/SDR transitions.".into();
                        if !timeline.warnings.is_empty() {this.message.push_str(&format!(" {}",timeline.warnings.join(" ")));}
                        this.timeline=Some(Arc::new(timeline));this.media=Some(Arc::new(media));this.seek_to(this.playhead_ns,cx);},
                    Err(error)=>this.message=error,
                }cx.notify();
            });
        }).detach();
    }
    fn edit_clips(&mut self, edit: Edit, cx: &mut Context<Self>) {
        let Some(original) = self.timeline.clone() else {
            return;
        };
        let next = match edit {
            Edit::Undo => match &self.undo {
                Some(t) => t.clone(),
                None => return,
            },
            Edit::Redo => match &self.redo {
                Some(t) => t.clone(),
                None => return,
            },
            _ => original.clone(),
        };
        let time = self.time();
        let before = original.clone();
        let cached_media = self.media.clone();
        let selected_clip = self.selected_clip;
        let frame_duration = self
            .media
            .as_ref()
            .and_then(|m| m.frames.get(self.position))
            .map_or(1, |(pts, d)| (pts.0 + *d).saturating_sub(time).max(1));
        self.busy = true;
        self.message = "Updating timeline…".into();
        let task = cx.background_executor().spawn(async move {
            let mut next = (*next).clone();
            next.upgrade();
            let result = match edit {
                Edit::Split => next.split(time),
                Edit::MoveClip(from, to) => next.move_clip(from, to),
                Edit::TrimStart | Edit::TrimEnd => {
                    if let Some((index, at, _)) = next.locate(time) {
                        let clip = next.clips[index].clone();
                        if index != selected_clip {
                            Err("Move the playhead inside the selected clip to trim.".into())
                        } else if matches!(edit, Edit::TrimStart) {
                            next.trim(index, at, clip.out_ns)
                        } else {
                            next.trim(
                                index,
                                clip.in_ns,
                                at.saturating_add(frame_duration).min(clip.out_ns),
                            )
                        }
                    } else {
                        Err("Select a clip first.".into())
                    }
                }
                _ => next.validate(),
            };
            result?;
            let tools = LocalTools::default();
            let media = match cached_media {
                Some(media) => tools.retime_timeline(&next, &before, &media)?,
                None => tools.inspect_timeline(&next)?,
            };
            let seek = edited_playhead(&before, &next, time, edit);
            Ok::<_, String>((next, media, seek))
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok((timeline, media, seek)) => {
                        if matches!(edit, Edit::Undo) {
                            this.redo = Some(original);
                            this.undo = None;
                        } else {
                            this.undo = Some(original);
                            this.redo = None;
                        }
                        this.timeline = Some(Arc::new(timeline));
                        this.media = Some(Arc::new(media));
                        this.selected_clip = this.selected_clip.min(
                            this.timeline
                                .as_ref()
                                .unwrap()
                                .clips
                                .len()
                                .saturating_sub(1),
                        );
                        this.fields = None;
                        this.message = "Timeline updated. Save edits to keep this change.".into();
                        this.seek_to(seek, cx);
                    }
                    Err(e) => this.message = e,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn export(&mut self, cx: &mut Context<Self>) {
        if self.closing || self.close_requested || self.close_prompt_pending {
            return;
        }
        let Some(timeline) = self.timeline.clone() else {
            return;
        };
        if self.busy {
            return;
        }
        self.stop_playback();
        self.busy = true;
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
                let active = view
                    .update(cx, |this, cx| {
                        if this.exporting {
                            cx.notify();
                        }
                        this.busy
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        })
        .detach();
        self.cancel.store(false, Ordering::Relaxed);
        self.message = "Choose a new Matroska export file.".into();
        let name = format!(
            "{}-studio.mkv",
            timeline
                .source
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
        );
        let answer = cx.prompt_for_new_path(timeline.source.parent().unwrap(), Some(&name));
        let profile = self.profile;
        let cancel = self.cancel.clone();
        let progress = self.progress.clone();
        cx.spawn(async move |view, cx| {
            let output = match answer.await {
                Ok(Ok(value)) => value,
                _ => None,
            };
            let Some(output) = output else {
                let _ = view.update(cx, |this, cx| {
                    this.busy = false;
                    this.message = "Export cancelled.".into();
                    cx.notify();
                });
                return;
            };
            let _ = view.update(cx, |this, cx| {
                this.exporting = true;
                this.message = "Preparing local export…".into();
                cx.notify();
            });
            let display = output.clone();
            let task = cx.background_executor().spawn(async move {
                LocalTools::default().export(&timeline, &output, profile, &cancel, &progress)
            });
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.busy = false;
                this.exporting = false;
                this.message = match result {
                    Ok(report) => format!(
                        "{} · Export saved · {}",
                        report.method.label(),
                        display.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    Err(e) => e,
                };
                cx.notify();
            });
        })
        .detach();
    }
    fn edit_button(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<gpui::SharedString>,
        edit: Edit,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id, label)
            .size(ButtonSize::Compact)
            .variant(ButtonVariant::Secondary)
            .disabled(
                self.busy
                    || self.timeline.is_none()
                    || matches!(edit, Edit::Suggest)
                        && self.timeline.as_ref().is_none_or(|t| t.metadata.is_none()),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                window.focus(&this.focus, cx);
                this.edit(edit, cx)
            }))
    }
}
// Trimming and moving a clip should keep the picture under the playhead.
fn edited_playhead(before: &Timeline, after: &Timeline, time: u64, edit: Edit) -> u64 {
    let clamp = |t: u64| t.min(after.duration_ns.saturating_sub(1));
    if !matches!(edit, Edit::TrimStart | Edit::TrimEnd | Edit::MoveClip(..)) {
        return clamp(time);
    }
    let (index, local) = if before.version == 1 {
        (0, time)
    } else if let Some((index, local, _)) = before.locate(time) {
        (index, local)
    } else {
        return clamp(time);
    };
    let index = match edit {
        Edit::MoveClip(from, to) if index == from => to,
        Edit::MoveClip(from, to) if from < to && index > from && index <= to => index - 1,
        Edit::MoveClip(from, to) if from > to && index >= to && index < from => index + 1,
        _ => index,
    };
    let Some(clip) = after.clips.get(index) else {
        return clamp(time);
    };
    let start: u64 = after.clips[..index]
        .iter()
        .map(TimelineClip::duration_ns)
        .sum();
    clamp(start + local.clamp(clip.in_ns, clip.out_ns.saturating_sub(1)) - clip.in_ns)
}
impl Render for Studio {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.retired_players.retain(|player| !player.finished());
        if !self.retired_players.is_empty() && self.player_cleanup.is_none() {
            self.player_cleanup = Some(cx.spawn(async move |view, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(20))
                        .await;
                    if !view
                        .update(cx, |this, cx| {
                            this.retired_players.retain(|player| !player.finished());
                            if this.retired_players.is_empty() {
                                this.player_cleanup = None;
                                cx.notify();
                                false
                            } else {
                                true
                            }
                        })
                        .unwrap_or(false)
                    {
                        break;
                    }
                }
            }));
        }
        if std::mem::take(&mut self.focus_editor) {
            if let Some(input) = self
                .fields
                .as_ref()
                .and_then(|(_, f)| f.first())
                .map(|(_, input)| input)
            {
                window.focus(&input.focus_handle(cx), cx);
            } else {
                window.focus(&self.focus, cx);
            }
        }
        if self.close_requested && !self.busy && !self.close_prompt_pending {
            cx.defer_in(window, |this, window, cx| this.close(window, cx));
        }
        if self.closing
            && !self.busy
            && !self.preview_busy
            && !self.image_busy
            && self.retired_players.is_empty()
        {
            if std::mem::take(&mut self.save_on_close) {
                self.busy = true;
                let timeline = self.timeline.clone();
                let task = cx.background_executor().spawn(async move {
                    let timeline = timeline.ok_or("There is no edit plan to save.")?;
                    timeline.save(&sidecar_path(&timeline.source, ".freym-edit.json"))
                });
                cx.spawn_in(window, async move |view, cx| {
                    let result = task.await;
                    let _ = view.update_in(cx, |this, window, cx| {
                        this.busy = false;
                        match result {
                            Ok(()) => this.finish_close(window, cx),
                            Err(e) => {
                                this.closing = false;
                                this.cancel.store(false, Ordering::Relaxed);
                                this.message = e;
                                this.resolve_close(false);
                                cx.notify();
                            }
                        }
                    });
                })
                .detach();
            } else {
                cx.defer_in(window, |this, window, cx| this.finish_close(window, cx));
            }
        }
        let mut controls = Stack::horizontal()
            .child(self.edit_button("suggest", "Suggest zooms", Edit::Suggest, cx))
            .child(self.edit_button("add", "Add zoom here", Edit::Add, cx))
            .child(
                Button::new("save", "Save edits")
                    .size(ButtonSize::Compact)
                    .disabled(self.busy || self.timeline.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
            .child(
                Button::new(
                    "export-settings",
                    format!(
                        "Export settings · {}",
                        match self.profile {
                            ExportProfile::H264 { .. } => "H.264",
                            ExportProfile::Lossless => "Lossless",
                            ExportProfile::Hevc { .. } => "HEVC",
                            ExportProfile::Av1 { .. } => "AV1",
                        }
                    ),
                )
                .size(ButtonSize::Compact)
                .disabled(self.busy || self.timeline.is_none())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.stop_playback();
                    this.export_settings = true;
                    cx.notify();
                })),
            )
            .child(
                Button::new("export", "Export rendered video")
                    .size(ButtonSize::Compact)
                    .disabled(self.busy || self.timeline.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.export(cx))),
            );
        if self.exporting {
            controls = controls.child(
                Button::new("cancel-export", "Cancel export")
                    .size(ButtonSize::Compact)
                    .on_click(
                        cx.listener(|this, _, _, _| this.cancel.store(true, Ordering::Relaxed)),
                    ),
            );
        }
        let preview = div()
            .h(px(
                (f32::from(window.viewport_size().height) * 0.44).clamp(220., 500.)
            ))
            .rounded(px(10.))
            .overflow_hidden()
            .w_full()
            .bg(Color::PreviewSurface.get())
            .flex()
            .justify_center()
            .items_center()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)),
            )
            .when_some(self.image.clone(), |d, image| {
                d.child(img(image).size_full().object_fit(gpui::ObjectFit::Contain))
            });
        let seek = Stack::horizontal()
            .child(
                Button::new("play", if self.playing { "Pause" } else { "Play" })
                    .size(ButtonSize::Compact)
                    .disabled(self.busy || self.timeline.is_none())
                    .on_click(cx.listener(|this, _, window, cx| {
                        window.focus(&this.focus, cx);
                        this.toggle_playback(cx)
                    })),
            )
            .child(
                Button::new("previous", "Previous frame")
                    .size(ButtonSize::Compact)
                    .disabled(self.busy || self.media.is_none() || self.position == 0)
                    .on_click(cx.listener(|this, _, window, cx| {
                        window.focus(&this.focus, cx);
                        this.seek_frames(-1, cx)
                    })),
            )
            .child(
                Button::new("next", "Next frame")
                    .size(ButtonSize::Compact)
                    .disabled(
                        self.busy
                            || self
                                .media
                                .as_ref()
                                .is_none_or(|m| self.position + 1 >= m.frames.len()),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        window.focus(&this.focus, cx);
                        this.seek_frames(1, cx)
                    })),
            )
            .child(div().w(px(90.)).child(self.seek.clone()))
            .child(
                Button::new("seek", "Seek seconds")
                    .size(ButtonSize::Compact)
                    .disabled(self.busy || self.media.is_none())
                    .on_click(cx.listener(|this, _, window, cx| {
                        window.focus(&this.focus, cx);
                        this.seek_seconds(cx)
                    })),
            )
            .child(
                Text::new(format!(
                    "{:.3} / {:.3} s{}",
                    self.time() as f64 / 1e9,
                    self.timeline
                        .as_ref()
                        .map_or(0., |t| t.duration_ns as f64 / 1e9),
                    if self.preview_busy {
                        " · updating preview…"
                    } else {
                        ""
                    }
                ))
                .variant(TextVariant::Caption),
            );
        let mut timeline_strip = div().flex().flex_col().gap(px(6.));
        if let Some(timeline) = &self.timeline {
            let duration = timeline.duration_ns;
            let playhead = self.playhead_ns;
            let selected = self.selected_clip;
            let clips: Vec<_> = if timeline.version == 1 {
                vec![duration]
            } else {
                timeline.clips.iter().map(|c| c.duration_ns()).collect()
            };
            let view = cx.weak_entity();
            timeline_strip = timeline_strip.child(
                div()
                    .id("studio-timeline")
                    .h(px(42.))
                    .w_full()
                    .cursor_pointer()
                    .relative()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, e: &gpui::MouseDownEvent, window, cx| {
                            if this.busy {
                                return;
                            }
                            window.focus(&this.focus, cx);
                            this.resume_after_scrub = this.playing;
                            this.scrubbing = true;
                            this.scrub(e.position, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                        if this.scrubbing {
                            this.scrub(e.position, cx);
                        }
                    }))
                    .on_mouse_up(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.finish_scrub(cx)),
                    )
                    .on_mouse_up_out(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.finish_scrub(cx)),
                    )
                    .child(
                        gpui::canvas(
                            |_, _, _| (),
                            move |bounds, _, window, cx| {
                                let _ =
                                    view.update(cx, |this, _| this.timeline_bounds = Some(bounds));
                                let mut start = 0;
                                for (i, length) in clips.iter().enumerate() {
                                    let x = bounds.left()
                                        + bounds.size.width * (start as f32 / duration as f32);
                                    let width =
                                        bounds.size.width * (*length as f32 / duration as f32);
                                    let rect = gpui::Bounds::new(
                                        gpui::point(x, bounds.top()),
                                        gpui::size(
                                            (width - px(2.)).max(px(1.)),
                                            bounds.size.height,
                                        ),
                                    );
                                    window.paint_quad(gpui::fill(
                                        rect,
                                        if i == selected {
                                            Color::Accent.get().opacity(0.4)
                                        } else {
                                            Color::Accent.get().opacity(0.14)
                                        },
                                    ));
                                    start += length;
                                }
                                let x = bounds.left()
                                    + bounds.size.width * (playhead as f32 / duration as f32);
                                window.paint_quad(gpui::fill(
                                    gpui::Bounds::new(
                                        gpui::point(x, bounds.top()),
                                        gpui::size(px(2.), bounds.size.height),
                                    ),
                                    Color::PrimaryText.get(),
                                ));
                            },
                        )
                        .size_full(),
                    ),
            );
            let audio_label = self
                .audio_track
                .and_then(|i| self.media.as_ref()?.audio.get(i))
                .map_or("Silent preview".to_owned(), |a| {
                    format!(
                        "Listen {}: {} · {} ch",
                        self.audio_track.unwrap_or(0) + 1,
                        a.title,
                        a.channels
                    )
                });
            timeline_strip = timeline_strip.child(
                Stack::horizontal()
                    .child(
                        Button::new("append-recording", "Append recording")
                            .size(ButtonSize::Compact)
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.append_recording(cx))),
                    )
                    .child(self.edit_button("split-clip", "Split here", Edit::Split, cx))
                    .child(self.edit_button("trim-start", "Trim start here", Edit::TrimStart, cx))
                    .child(self.edit_button("trim-end", "Trim end here", Edit::TrimEnd, cx))
                    .child(
                        Button::new("undo", "Undo")
                            .size(ButtonSize::Compact)
                            .disabled(self.busy || self.undo.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                window.focus(&this.focus, cx);
                                this.edit(Edit::Undo, cx)
                            })),
                    )
                    .child(
                        Button::new("redo", "Redo")
                            .size(ButtonSize::Compact)
                            .disabled(self.busy || self.redo.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                window.focus(&this.focus, cx);
                                this.edit(Edit::Redo, cx)
                            })),
                    )
                    .child(
                        Button::new("audio-track", audio_label)
                            .size(ButtonSize::Compact)
                            .disabled(
                                self.busy || self.media.as_ref().is_none_or(|m| m.audio.is_empty()),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                let resume = this.playing;
                                this.stop_playback();
                                let count = this.media.as_ref().map_or(0, |m| m.audio.len());
                                this.audio_track = match this.audio_track {
                                    Some(i) if i + 1 < count => Some(i + 1),
                                    Some(_) => None,
                                    None if count > 0 => Some(0),
                                    _ => None,
                                };
                                if resume {
                                    this.toggle_playback(cx);
                                }
                                cx.notify();
                            })),
                    ),
            );
            let mut rows = div()
                .id("studio-clip-list")
                .max_h(px(100.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(4.));
            let mut start = 0;
            let clip_rows: Vec<_> = if timeline.version == 1 {
                vec![(timeline.source.clone(), 0, timeline.duration_ns)]
            } else {
                timeline
                    .clips
                    .iter()
                    .map(|clip| {
                        (
                            timeline.sources[clip.source_index].path.clone(),
                            clip.in_ns,
                            clip.out_ns,
                        )
                    })
                    .collect()
            };
            for (i, (path, in_ns, out_ns)) in clip_rows.iter().enumerate() {
                let at = start;
                let count = clip_rows.len();
                start += out_ns - in_ns;
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                rows = rows.child(
                    Stack::horizontal()
                        .child(
                            Button::new(
                                format!("clip-{i}"),
                                format!(
                                    "{} · {} · {:.2}–{:.2} s",
                                    i + 1,
                                    name,
                                    *in_ns as f64 / 1e9,
                                    *out_ns as f64 / 1e9
                                ),
                            )
                            .size(ButtonSize::Compact)
                            .selected(i == self.selected_clip)
                            .disabled(self.busy)
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    window.focus(&this.focus, cx);
                                    this.selected_clip = i;
                                    this.seek_to(at, cx);
                                },
                            )),
                        )
                        .child(
                            Button::new(format!("clip-left-{i}"), "Earlier")
                                .size(ButtonSize::Compact)
                                .disabled(self.busy || i == 0)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    window.focus(&this.focus, cx);
                                    this.edit(Edit::MoveClip(i, i.saturating_sub(1)), cx)
                                })),
                        )
                        .child(
                            Button::new(format!("clip-right-{i}"), "Later")
                                .size(ButtonSize::Compact)
                                .disabled(self.busy || i + 1 == count)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    window.focus(&this.focus, cx);
                                    this.edit(Edit::MoveClip(i, i + 1), cx)
                                })),
                        ),
                );
            }
            timeline_strip = timeline_strip.child(rows);
        }
        let mut effects = Stack::vertical();
        let mut segments = div()
            .id("studio-segments")
            .max_h(px(150.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(6.));
        if let Some(timeline) = &self.timeline {
            if let Some(m) = &timeline.metadata {
                effects = effects
                    .child(self.edit_button(
                        "baked",
                        if m.cursor_baked {
                            "Source cursor: baked"
                        } else {
                            "Source cursor: absent"
                        },
                        Edit::Baked,
                        cx,
                    ))
                    .child(self.edit_button(
                        "cursor",
                        if timeline.cursor.enabled {
                            "Cursor effects: on"
                        } else {
                            "Cursor effects: off"
                        },
                        Edit::Cursor,
                        cx,
                    ))
                    .child(self.edit_button(
                        "smooth",
                        format!("Smoothing: {} ms", timeline.cursor.smoothing_ms),
                        Edit::Smoothing,
                        cx,
                    ))
                    .child(self.edit_button(
                        "emphasis",
                        if timeline.cursor.emphasis {
                            "Emphasis: on"
                        } else {
                            "Emphasis: off"
                        },
                        Edit::Emphasis,
                        cx,
                    ))
                    .child(self.edit_button(
                        "size",
                        format!("Cursor size: {}×", timeline.cursor.size),
                        Edit::Size,
                        cx,
                    ))
                    .child(self.edit_button(
                        "discard-metadata",
                        "Discard event snapshot",
                        Edit::DiscardMetadata,
                        cx,
                    ));
            }
            for (i, z) in timeline.zooms.iter().enumerate() {
                let start = z.start_ns;
                segments = segments.child(
                    Stack::horizontal()
                        .child(self.edit_button(
                            format!("toggle-{i}"),
                            if z.enabled { "On" } else { "Off" },
                            Edit::Toggle(i),
                            cx,
                        ))
                        .child(
                            Button::new(
                                format!("jump-{i}"),
                                format!(
                                    "{:.2}–{:.2} s · {:.2}×",
                                    z.start_ns as f64 / 1e9,
                                    z.end_ns as f64 / 1e9,
                                    z.scale
                                ),
                            )
                            .size(ButtonSize::Compact)
                            .disabled(self.busy)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if let Some(m) = &this.media {
                                        this.position = m
                                            .frames
                                            .partition_point(|(t, _)| t.0 <= start)
                                            .saturating_sub(1);
                                        this.seek_to(start, cx);
                                    }
                                },
                            )),
                        )
                        .child(self.edit_button(
                            format!("adjust-{i}"),
                            "Adjust",
                            Edit::Adjust(i),
                            cx,
                        ))
                        .child(self.edit_button(
                            format!("easing-{i}"),
                            format!("{:?}", z.easing),
                            Edit::Easing(i),
                            cx,
                        ))
                        .child(self.edit_button(
                            format!("remove-{i}"),
                            "Remove",
                            Edit::Remove(i),
                            cx,
                        )),
                );
            }
        }
        let tabs = SegmentedControl::new()
            .id("studio-inspector-tabs")
            .selected(self.inspector_tab)
            .children(
                ["Add", "Effects"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, label)| {
                        Button::new(format!("studio-inspector-{index}"), label)
                            .segment()
                            .selected(self.inspector_tab == index)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.inspector_tab = index;
                                cx.notify();
                            }))
                    }),
            );
        let inspector_body = if self.inspector_tab == 0 {
            Stack::vertical()
                .child(Text::new("Add to your recording").variant(TextVariant::Heading))
                .child(self.edit_button(
                    "inspector-add-zoom",
                    "Add zoom at playhead",
                    Edit::Add,
                    cx,
                ))
                .child(self.edit_button(
                    "inspector-suggest-zoom",
                    "Suggest zooms from metadata",
                    Edit::Suggest,
                    cx,
                ))
                .child(
                    Button::new("inspector-import", "Import recording…")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| this.append_recording(cx))),
                )
                .into_any_element()
        } else {
            Stack::vertical()
                .child(effects)
                .child(segments)
                .into_any_element()
        };
        let inspector = div()
            .id("studio-inspector")
            .flex_shrink_0()
            .w(px(300.))
            .p(px(16.))
            .rounded(px(10.))
            .border_1()
            .border_color(Color::DefaultBorder.get())
            .bg(Color::ElevatedSurface.get())
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(tabs)
            .child(inspector_body);
        let narrow = f32::from(window.viewport_size().width) < 1000.;
        let mut content = div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if !this.focus.is_focused(window)
                    || this.fields.is_some()
                    || this.export_settings
                    || this.busy
                    || this.scrubbing
                {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "space" => this.toggle_playback(cx),
                    "left" => this.seek_frames(-1, cx),
                    "right" => this.seek_frames(1, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(Color::PrimarySurface.get())
            .font_family(crate::ui::theme::typography::FONT_FAMILY)
            .text_color(Color::PrimaryText.get())
            .child(
                Titlebar::new(crate::ui::components::titlebar::close_button(
                    cx.listener(|this, _, w, cx| this.close(w, cx)),
                ))
                .status(Text::new("Studio")),
            )
            .child(
                div()
                    .id("studio-content")
                    .flex_1()
                    .overflow_y_scroll()
                    .p(px(20.))
                    .child(
                        Stack::vertical()
                            .child(Text::new(
                                self.source
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .into_owned(),
                            ))
                            .child(controls)
                            .child(
                                div()
                                    .flex()
                                    .gap(px(20.))
                                    .when(narrow, |d| d.flex_col())
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap(px(16.))
                                            .child(preview)
                                            .child(seek)
                                            .child(timeline_strip),
                                    )
                                    .child(inspector),
                            )
                            .child(Text::new(self.message.clone()).variant(TextVariant::Caption))
                            .when(self.exporting, |s| {
                                s.child(
                                    Text::new(format!(
                                        "{} frames rendered",
                                        self.progress.load(Ordering::Relaxed)
                                    ))
                                    .variant(TextVariant::Caption),
                                )
                            }),
                    ),
            );
        if let Some((_, fields)) = &self.fields {
            let mut modal = Modal::new("studio-zoom", "Adjust zoom")
                .on_dismiss(cx.listener(|this, _, _, cx| this.edit(Edit::CancelFields, cx)));
            for (label, input) in fields {
                modal = modal.child(
                    Stack::horizontal()
                        .child(Text::new(*label))
                        .child(input.clone()),
                );
            }
            modal = modal.child(
                Stack::horizontal()
                    .child(self.edit_button("apply", "Apply", Edit::Apply, cx))
                    .child(self.edit_button("cancel-fields", "Cancel", Edit::CancelFields, cx)),
            );
            content = content.child(modal);
        }
        if self.export_settings {
            let colour = self
                .timeline
                .as_ref()
                .map_or(OutputColour::Preserve, |t| t.output_colour);
            let needs_ten_bit = match colour {
                OutputColour::HdrPq | OutputColour::HdrHlg => true,
                OutputColour::Sdr => false,
                OutputColour::Preserve => self.media.as_ref().is_some_and(|m| m.bit_depth > 8),
            };
            let mut modal = Modal::new("studio-export-settings", "Export settings")
                .on_dismiss(cx.listener(|this, _, _, cx| {
                    this.export_settings = false;
                    this.focus_editor = true;
                    cx.notify();
                }))
                .child(Text::new("Format").variant(TextVariant::Label));
            for (id, label, profile) in [
                (
                    "h264-high",
                    "H.264 · high quality",
                    ExportProfile::H264 { crf: 18 },
                ),
                (
                    "h264-small",
                    "H.264 · smaller file",
                    ExportProfile::H264 { crf: 23 },
                ),
                (
                    "hevc",
                    "HEVC · supports 10-bit / HDR",
                    ExportProfile::Hevc { crf: 18 },
                ),
                (
                    "av1",
                    "AV1 · supports 10-bit / HDR",
                    ExportProfile::Av1 { crf: 24 },
                ),
                ("ffv1", "FFV1 · lossless encoding", ExportProfile::Lossless),
            ] {
                modal = modal.child(
                    Button::new(id, label)
                        .size(ButtonSize::Compact)
                        .selected(self.profile == profile)
                        .disabled(
                            self.busy
                                || (needs_ten_bit && matches!(profile, ExportProfile::H264 { .. })),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.edit(Edit::SetProfile(profile), cx)
                        })),
                );
            }
            modal = modal.child(Text::new("Colour").variant(TextVariant::Label));
            for (id, label, option) in [
                ("preserve", "Preserve source colour", OutputColour::Preserve),
                ("pq", "HDR · PQ", OutputColour::HdrPq),
                ("hlg", "HDR · HLG", OutputColour::HdrHlg),
                ("sdr", "SDR · tone map HDR", OutputColour::Sdr),
            ] {
                modal = modal.child(
                    Button::new(id, label)
                        .size(ButtonSize::Compact)
                        .selected(colour == option)
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.edit(Edit::SetColour(option), cx)
                        })),
                );
            }
            modal = modal
                .child(
                    Text::new(if needs_ten_bit {
                        "H.264 cannot preserve this 10-bit output. Choose HEVC, AV1 or Lossless."
                    } else {
                        "Exports use a new Matroska file. Originals stay unchanged."
                    })
                    .variant(TextVariant::Caption),
                )
                .child(
                    Button::new("done-export-settings", "Done")
                        .size(ButtonSize::Compact)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.export_settings = false;
                            this.focus_editor = true;
                            cx.notify();
                        })),
                );
            content = content.child(modal);
        }
        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trimming_and_reordering_keep_the_same_source_picture() {
        let before = Timeline {
            version: 1,
            source: "source.mkv".into(),
            identity: SourceIdentity {
                bytes: 1,
                modified_ns: 0,
            },
            duration_ns: 100,
            metadata: None,
            zooms: vec![],
            cursor: CursorEffects::default(),
            sources: vec![],
            clips: vec![],
            warnings: vec![],
            output_colour: OutputColour::Preserve,
        };
        let mut after = before.clone();
        after.trim(0, 25, 100).unwrap();
        assert_eq!(edited_playhead(&before, &after, 25, Edit::TrimStart), 0);
        assert_eq!(edited_playhead(&before, &after, 60, Edit::TrimStart), 35);
        after.split(40).unwrap();
        let before = after.clone();
        after.move_clip(0, 1).unwrap();
        for time in [0, 10, 39, 40, 60, 74] {
            let mapped = edited_playhead(&before, &after, time, Edit::MoveClip(0, 1));
            let (old_clip, old_local, _) = before.locate(time).unwrap();
            let (new_clip, new_local, _) = after.locate(mapped).unwrap();
            assert_eq!(old_local, new_local);
            assert_eq!(
                before.clips[old_clip].source_index,
                after.clips[new_clip].source_index
            );
        }
    }
}
