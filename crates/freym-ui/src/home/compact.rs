//! Native shell over the one Recorder entity. No controller, capture, settings
//! store or preview is constructed by either of these windows.
use super::*;
use crate::ui::{
    primitives::{
        Button, ButtonSize, ButtonVariant, FocusNext, FocusPrevious, Icons, Separator, Text,
        TextVariant,
    },
    theme::colors::Color,
};
use gpui::{
    App, AppContext, Bounds, Pixels, Point, Size, WindowBounds, WindowHandle, WindowKind,
    WindowOptions, div, point, size,
};

const BAR_WIDTH: f32 = 152.;
const BAR_HEIGHT: f32 = 470.;
const MENU_WIDTH: f32 = 550.;
const MENU_HEIGHT: f32 = 650.;
const GAP: f32 = 8.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Audio,
    Video,
}

pub(super) struct CompactBar {
    home: Entity<Home>,
    focus: FocusHandle,
    menu: Option<(Menu, WindowHandle<CompactMenu>)>,
    bounds: Bounds<Pixels>,
    excluded: bool,
}

struct CompactMenu {
    home: Entity<Home>,
    focus: FocusHandle,
    kind: Menu,
    bar: gpui::WeakEntity<CompactBar>,
}

// Logical coordinates stay on a single selected display. The display ID is
// retained with the position, so differently scaled monitor coordinate spaces
// are never compared to choose the display on re-entry.
fn clamp_bounds(
    origin: Point<Pixels>,
    requested: Size<Pixels>,
    work: Bounds<Pixels>,
) -> Bounds<Pixels> {
    let dimensions = size(
        requested.width.min(work.size.width),
        requested.height.min(work.size.height),
    );
    Bounds::new(
        point(
            origin
                .x
                .max(work.left())
                .min(work.right() - dimensions.width),
            origin
                .y
                .max(work.top())
                .min(work.bottom() - dimensions.height),
        ),
        dimensions,
    )
}

fn menu_bounds(bar: Bounds<Pixels>, work: Bounds<Pixels>) -> Bounds<Pixels> {
    let right = (work.right() - bar.right() - px(GAP)).max(px(0.));
    let left = (bar.left() - work.left() - px(GAP)).max(px(0.));
    let on_right = right >= px(MENU_WIDTH) || right >= left;
    let width = px(MENU_WIDTH).min(if on_right { right } else { left });
    let x = if on_right {
        bar.right() + px(GAP)
    } else {
        bar.left() - px(GAP) - width
    };
    clamp_bounds(point(x, bar.top()), size(width, px(MENU_HEIGHT)), work)
}

fn floating_options(bounds: Bounds<Pixels>, display: Option<gpui::DisplayId>) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        kind: WindowKind::PopUp,
        is_resizable: false,
        is_minimizable: false,
        display_id: display,
        app_id: Some("dev.frame.recorder".into()),
        ..Default::default()
    }
}

fn request_restore(home: &Entity<Home>, cx: &mut App) {
    let owner = home.read(cx).owner_window;
    let home = home.downgrade();
    // Close/remove only after the originating compact window's event dispatch
    // has returned. This also handles Alt+F4 without touching the engine.
    cx.defer(move |cx| {
        let _ = owner.update(cx, |_, window, cx| {
            let _ = home.update(cx, |home, cx| home.restore_compact(window, cx));
        });
    });
}

fn dispatch_to_owner(home: &Entity<Home>, action: Action, cx: &mut App) {
    let owner = home.read(cx).owner_window;
    let home = home.downgrade();
    let _ = owner.update(cx, |_, window, cx| {
        let _ = home.update(cx, |home, cx| home.dispatch(action, window, cx));
    });
}

impl Home {
    pub(super) fn can_mute(&self, id: &AudioSourceId) -> bool {
        !self.model.connecting
            && !self.model.command_pending
            && !self.model.closing
            && self.pending_mute.is_none()
            && matches!(
                self.model.snapshot.state,
                RecorderState::Idle | RecorderState::Recording | RecorderState::Paused
            )
            && self
                .model
                .snapshot
                .audio_sources
                .iter()
                .any(|s| &s.id == id && s.is_active && s.route != AudioRoute::Ignore)
    }

    fn configured_microphone(&self) -> Option<&AudioSource> {
        // Use the configured selection, never silently switch to the first
        // discovered microphone when the selected device is lost or disabled.
        let routes = &self.model.request.as_ref()?.audio_routes;
        routes
            .iter()
            .filter(|r| r.route != AudioRoute::Ignore)
            .find_map(|r| {
                self.model
                    .snapshot
                    .audio_sources
                    .iter()
                    .find(|s| s.id == r.source_id && s.kind == AudioSourceKind::Microphone)
            })
    }

    pub(super) fn enter_compact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.closing {
            return;
        }
        if let Some(compact) = self.compact {
            let _ = compact.update(cx, |_, window, _| window.activate_window());
            return;
        }
        let displays = cx.displays();
        let display = self
            .compact_position
            .and_then(|(id, _)| displays.iter().find(|d| d.id() == id).cloned())
            .or_else(|| window.display(cx));
        let Some(display) = display else {
            return;
        };
        let work = display.visible_bounds();
        let origin = self
            .compact_position
            .filter(|(id, _)| *id == display.id())
            .map(|(_, b)| b.origin)
            .unwrap_or(point(
                work.right() - px(BAR_WIDTH + 24.),
                work.top() + px(24.),
            ));
        let bounds = clamp_bounds(origin, size(px(BAR_WIDTH), px(BAR_HEIGHT)), work);
        let home = cx.entity();
        let owner = self.owner_window;
        // open_window draws its root immediately. Release the Recorder entity
        // before the compact root reads it on that first frame.
        cx.defer(move |cx| {
            if home.read(cx).model.closing || home.read(cx).compact.is_some() {
                return;
            }
            let root = home.clone();
            match cx.open_window(
                floating_options(bounds, Some(display.id())),
                |window, cx| cx.new(|cx| CompactBar::new(root, bounds, window, cx)),
            ) {
                Ok(compact) => {
                    if owner
                        .update(cx, |_, window, cx| {
                            home.update(cx, |home, cx| {
                                home.compact = Some(compact);
                                home.model.sheet = None;
                                home.number_editor = None;
                                #[cfg(target_os = "windows")]
                                if let Some(preview) = &home.preview {
                                    preview.update(cx, |preview, cx| {
                                        preview.configure(None, (1, 1), false, window, cx)
                                    });
                                }
                                // GPUI's TestWindow has no minimize implementation.
                                #[cfg(not(test))]
                                window.minimize_window();
                                cx.notify();
                            });
                        })
                        .is_err()
                    {
                        let _ = compact.update(cx, |_, window, _| window.remove_window());
                    } else {
                        let _ = compact.update(cx, |_, window, _| window.activate_window());
                    }
                }
                Err(error) => home.update(cx, |home, cx| {
                    home.model.error = Some(format!("Could not open Compact Mode: {error}"));
                    cx.notify();
                }),
            }
        });
    }

    pub(super) fn restore_compact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(compact) = self.compact.take() else {
            return;
        };
        let _ = compact.update(cx, |bar, window, cx| {
            bar.close_menu(cx);
            window.remove_window();
        });
        self.model.sheet = None;
        self.number_editor = None;
        freym_platform::restore_recorder(Self::raw_owner(window));
        window.activate_window();
        window.focus(&self.focus, cx);
        cx.notify();
    }
}

impl CompactBar {
    fn new(
        home: Entity<Home>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        window.set_window_title("FREYM · Compact");
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        cx.observe(&home, |_, _, cx| cx.notify()).detach();
        let restore = home.clone();
        window.on_window_should_close(cx, move |_, cx| {
            request_restore(&restore, cx);
            false
        });
        cx.observe_window_bounds(window, |this, window, cx| {
            let bounds = window.bounds();
            if this.bounds == bounds {
                return;
            }
            this.bounds = bounds;
            // A moving toolbar dismisses its transient menu. Reopening uses
            // the destination monitor's work area and DPI, never stale bounds.
            this.close_menu(cx);
            if let Some(display) = window.display(cx) {
                let clamped = clamp_bounds(bounds.origin, bounds.size, display.visible_bounds());
                if clamped.origin != bounds.origin {
                    freym_platform::move_compact_window(
                        Home::raw_owner(window),
                        clamped.origin.x.into(),
                        clamped.origin.y.into(),
                        window.scale_factor(),
                    );
                }
                this.home.update(cx, |home, _| {
                    home.compact_position = Some((display.id(), clamped));
                    home.number_editor = None;
                });
            }
        })
        .detach();
        let excluded = freym_platform::exclude_compact_from_capture(Home::raw_owner(window));
        Self {
            home,
            focus,
            menu: None,
            bounds,
            excluded,
        }
    }

    fn close_menu(&mut self, cx: &mut App) {
        if let Some((_, menu)) = self.menu.take() {
            let _ = menu.update(cx, |_, window, _| window.remove_window());
        }
    }

    fn toggle_menu(&mut self, kind: Menu, window: &mut Window, cx: &mut Context<Self>) {
        let same = self
            .menu
            .as_ref()
            .is_some_and(|(old, handle)| *old == kind && handle.update(cx, |_, _, _| ()).is_ok());
        self.close_menu(cx);
        self.home.update(cx, |home, _| home.number_editor = None);
        if same {
            window.focus(&self.focus, cx);
            cx.notify();
            return;
        }
        let Some(display) = window.display(cx) else {
            return;
        };
        let bounds = menu_bounds(window.bounds(), display.visible_bounds());
        let home = self.home.clone();
        let bar = cx.weak_entity();
        match cx.open_window(
            floating_options(bounds, Some(display.id())),
            |window, cx| cx.new(|cx| CompactMenu::new(home, bar, kind, window, cx)),
        ) {
            Ok(menu) => self.menu = Some((kind, menu)),
            Err(error) => self.home.update(cx, |home, cx| {
                home.model.error = Some(format!("Could not open compact menu: {error}"));
                cx.notify();
            }),
        }
        cx.notify();
    }
}

impl Render for CompactBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let home = self.home.clone();
        let controls = home.update(cx, |home, cx| {
            let state = &home.model.snapshot.state;
            let busy = home.model.command_pending || home.model.closing || home.model.connecting;
            let status = match state {
                _ if home.model.connecting => "Connecting…",
                RecorderState::Idle => "Ready",
                RecorderState::Preparing => "Preparing…",
                RecorderState::Recording if home.model.snapshot.replay_only => "Replay buffering",
                RecorderState::Recording => "Recording",
                RecorderState::Paused => "Paused",
                RecorderState::Finalizing => "Saving…",
                RecorderState::Failed(_) => "Failed",
            };
            let mut controls = div()
                .flex()
                .flex_col()
                .gap_2()
                .child(Text::new(status).variant(TextVariant::Caption).color(
                    if *state == RecorderState::Recording {
                        Color::Record
                    } else {
                        Color::SecondaryText
                    },
                ))
                .child(
                    Text::new(format_elapsed(
                        home.model
                            .snapshot
                            .metrics
                            .as_ref()
                            .map(|m| m.elapsed_ms)
                            .unwrap_or(0.),
                    ))
                    .variant(TextVariant::Label),
                );
            let microphone = home.configured_microphone();
            let mic_label = match microphone {
                Some(s) if !s.is_active => "Mic unavailable",
                Some(s) if s.muted => "Mic off",
                Some(_) => "Mic on",
                None => "Mic disabled",
            };
            let mic_action = microphone.map(|s| Action::MuteAudio(s.id.clone(), !s.muted));
            let mic_enabled = microphone.is_some_and(|s| home.can_mute(&s.id));
            let mut mic = Button::new("compact-microphone", mic_label)
                .icon(Icons::Mic)
                .size(ButtonSize::Compact)
                .disabled(!mic_enabled)
                .selected(microphone.is_some_and(|s| !s.muted && s.is_active));
            if let Some(action) = mic_action {
                mic = mic.on_click(home.handler(action, cx));
            }
            controls = controls.child(mic).child(Separator);
            match state {
                RecorderState::Idle => {
                    controls = controls.child(
                        Button::new("compact-record", "Start recording")
                            .size(ButtonSize::Compact)
                            .variant(ButtonVariant::Record)
                            .disabled(busy || !home.model.can_start())
                            .on_click(home.handler(Action::Record, cx)),
                    )
                }
                RecorderState::Preparing => {
                    controls = controls.child(
                        Button::new("compact-stop", "Cancel start")
                            .size(ButtonSize::Compact)
                            .disabled(busy)
                            .on_click(home.handler(Action::Record, cx)),
                    )
                }
                RecorderState::Recording | RecorderState::Paused => {
                    let paused = *state == RecorderState::Paused;
                    controls = controls
                        .child(
                            Button::new("compact-pause", if paused { "Resume" } else { "Pause" })
                                .size(ButtonSize::Compact)
                                .icon(if paused { Icons::Play } else { Icons::Pause })
                                .disabled(busy || !home.model.snapshot.capabilities.can_pause)
                                .on_click(home.handler(Action::Pause, cx)),
                        )
                        .child(
                            Button::new("compact-stop", "Stop recording")
                                .size(ButtonSize::Compact)
                                .variant(ButtonVariant::Danger)
                                .disabled(busy)
                                .on_click(home.handler(Action::Record, cx)),
                        );
                }
                RecorderState::Failed(_) => {
                    controls = controls.child(
                        Button::new("compact-reset", "Reset recorder")
                            .size(ButtonSize::Compact)
                            .disabled(busy)
                            .on_click(home.handler(Action::Record, cx)),
                    )
                }
                RecorderState::Finalizing => {}
            }
            let error = home
                .model
                .error
                .clone()
                .or_else(|| match &home.model.snapshot.state {
                    RecorderState::Failed(e) => Some(format!("{} {}", e.message, e.action)),
                    _ => None,
                });
            if let Some(error) = error {
                controls = controls.child(
                    Text::new(error)
                        .variant(TextVariant::Caption)
                        .color(Color::Error),
                );
            } else if let Some(notice) = &home.model.notice {
                controls = controls.child(
                    Text::new(notice.clone())
                        .variant(TextVariant::Caption)
                        .color(Color::Warning),
                );
            }
            controls
        });
        div()
            .id("compact-bar")
            .size_full()
            .bg(Color::Background.get())
            .border_1()
            .border_color(Color::DefaultBorder.get())
            .rounded(px(10.))
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .track_focus(&self.focus)
            .key_context("FREYM")
            .on_action(cx.listener(|this, _: &Cancel, window, cx| {
                this.close_menu(cx);
                this.home.update(cx, |home, _| home.number_editor = None);
                window.focus(&this.focus, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Record, _, cx| {
                if this.home.read(cx).model.shortcuts.start_stop.is_empty() {
                    dispatch_to_owner(&this.home, Action::Record, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PauseResume, _, cx| {
                if this.home.read(cx).model.shortcuts.pause_resume.is_empty() {
                    dispatch_to_owner(&this.home, Action::Pause, cx);
                }
            }))
            .on_action(|_: &FocusNext, window, cx| window.focus_next(cx))
            .on_action(|_: &FocusPrevious, window, cx| window.focus_prev(cx))
            .child(
                div()
                    .id("compact-drag")
                    .h(px(28.))
                    .flex_shrink_0()
                    .window_control_area(gpui::WindowControlArea::Drag)
                    .child(Text::new("FREYM · Compact").variant(TextVariant::Caption)),
            )
            .child(
                Button::new("compact-audio", "Audio")
                    .icon(Icons::Audio)
                    .size(ButtonSize::Compact)
                    .selected(self.menu.as_ref().is_some_and(|(m, _)| *m == Menu::Audio))
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.toggle_menu(Menu::Audio, window, cx)
                        }),
                    ),
            )
            .child(
                Button::new("compact-video", "Video")
                    .icon(Icons::Quality)
                    .size(ButtonSize::Compact)
                    .selected(self.menu.as_ref().is_some_and(|(m, _)| *m == Menu::Video))
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.toggle_menu(Menu::Video, window, cx)
                        }),
                    ),
            )
            .child(
                div()
                    .id("compact-session")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(controls),
            )
            .when(cfg!(target_os = "windows") && !self.excluded, |d| {
                d.child(Text::new("Capture exclusion unavailable").variant(TextVariant::Caption))
            })
            .child(
                Button::new("compact-restore", "Full Recorder")
                    .icon(Icons::Restore)
                    .size(ButtonSize::Compact)
                    .on_click(cx.listener(|this, _, _, cx| request_restore(&this.home, cx))),
            )
    }
}

impl CompactMenu {
    fn new(
        home: Entity<Home>,
        bar: gpui::WeakEntity<CompactBar>,
        kind: Menu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        window.set_window_title(if kind == Menu::Audio {
            "FREYM · Compact Audio"
        } else {
            "FREYM · Compact Video"
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        cx.observe(&home, |_, _, cx| cx.notify()).detach();
        cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() && this.home.read(cx).number_editor.is_none() {
                this.dismiss(window, cx);
            }
        })
        .detach();
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let _ = view.update(cx, |this, cx| this.dismiss(window, cx));
            false
        });
        freym_platform::exclude_compact_from_capture(Home::raw_owner(window));
        Self {
            home,
            focus,
            kind,
            bar,
        }
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.home.update(cx, |home, cx| {
            home.number_editor = None;
            cx.notify();
        });
        let bar = self.bar.clone();
        let handle = window.window_handle();
        window.remove_window();
        cx.defer(move |cx| {
            let _ = bar.update(cx, |bar, cx| {
                if bar
                    .menu
                    .as_ref()
                    .is_some_and(|(_, menu)| menu.window_id() == handle.window_id())
                {
                    bar.menu = None;
                    cx.notify();
                }
            });
        });
    }
}

impl Render for CompactMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = self.home.update(cx, |home, cx| {
            if let Some(editor) = &home.number_editor {
                return home.number_sheet(editor, cx);
            }
            match self.kind {
                Menu::Audio => div()
                    .child(home.home_audio_panel(cx))
                    .child(
                        Text::new("Volume adjustment is unavailable in the current audio runtime.")
                            .variant(TextVariant::Caption),
                    )
                    .into_any_element(),
                Menu::Video if home.model.sheet == Some(Sheet::Settings) => {
                    home.preferences_sheet(cx)
                }
                Menu::Video => home.quality_options(cx),
            }
        });
        div()
            .id("compact-menu")
            .size_full()
            .bg(Color::Background.get())
            .border_1()
            .border_color(Color::DefaultBorder.get())
            .rounded(px(10.))
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .track_focus(&self.focus)
            .key_context("FREYM")
            .on_action(cx.listener(|this, _: &Cancel, window, cx| {
                this.dismiss(window, cx);
            }))
            .on_action(|_: &FocusNext, window, cx| window.focus_next(cx))
            .on_action(|_: &FocusPrevious, window, cx| window.focus_prev(cx))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        Text::new(if self.kind == Menu::Audio {
                            "Audio"
                        } else {
                            "Video / Quality"
                        })
                        .variant(TextVariant::Label),
                    )
                    .child(
                        Button::new("compact-menu-close", "Close")
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
                    ),
            )
            .child(
                div()
                    .id("compact-menu-content")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .child(content),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext};

    #[gpui::test]
    fn compact_entry_first_render_and_restore_preserve_the_recorder_owner(cx: &mut TestAppContext) {
        let (home, cx) = cx.add_window_view(super::super::tests::home);
        cx.update(|window, cx| home.update(cx, |home, cx| home.enter_compact(window, cx)));
        cx.run_until_parked();
        cx.update(|window, cx| {
            home.update(cx, |home, cx| {
                assert!(home.compact.is_some());
                assert_eq!(home.services.control.published().state, RecorderState::Idle);
                home.restore_compact(window, cx);
                assert!(home.compact.is_none());
            });
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("record-button").is_some());
    }
    #[test]
    fn menus_flip_and_clamp_on_negative_and_scaled_display_coordinates() {
        for work in [
            Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1040.))),
            Bounds::new(point(px(-1707.), px(-240.)), size(px(1707.), px(920.))),
            Bounds::new(point(px(960.), px(0.)), size(px(1280.), px(680.))),
            Bounds::new(point(px(0.), px(0.)), size(px(480.), px(360.))),
        ] {
            for origin in [
                work.origin,
                work.bottom_right() - point(px(BAR_WIDTH), px(BAR_HEIGHT)),
            ] {
                let bar = clamp_bounds(origin, size(px(BAR_WIDTH), px(BAR_HEIGHT)), work);
                let menu = menu_bounds(bar, work);
                assert!(work.contains(&menu.origin));
                assert!(menu.right() <= work.right() && menu.bottom() <= work.bottom());
                assert!(menu.right() <= bar.left() || menu.left() >= bar.right());
            }
        }
    }

    #[gpui::test]
    fn compact_controls_follow_shared_publication_and_do_not_own_a_session(
        cx: &mut TestAppContext,
    ) {
        let (bar, cx) = cx.add_window_view(|window, cx| {
            let home = cx.new(|cx| super::super::tests::home(window, cx));
            CompactBar::new(
                home,
                Bounds::new(point(px(0.), px(0.)), size(px(BAR_WIDTH), px(BAR_HEIGHT))),
                window,
                cx,
            )
        });
        cx.simulate_resize(size(px(BAR_WIDTH), px(BAR_HEIGHT)));
        for (state, expected, absent) in [
            (
                RecorderState::Idle,
                vec!["compact-record"],
                vec!["compact-pause", "compact-stop"],
            ),
            (
                RecorderState::Preparing,
                vec!["compact-stop"],
                vec!["compact-record", "compact-pause"],
            ),
            (
                RecorderState::Recording,
                vec!["compact-pause", "compact-stop"],
                vec!["compact-record"],
            ),
            (
                RecorderState::Paused,
                vec!["compact-pause", "compact-stop"],
                vec!["compact-record"],
            ),
            (
                RecorderState::Finalizing,
                vec![],
                vec!["compact-pause", "compact-stop", "compact-record"],
            ),
        ] {
            cx.update(|_, cx| {
                bar.read(cx).home.clone().update(cx, |home, cx| {
                    home.model.snapshot.state = state;
                    cx.notify();
                });
            });
            cx.run_until_parked();
            for id in expected.into_iter().chain([
                "compact-audio",
                "compact-video",
                "compact-microphone",
                "compact-restore",
            ]) {
                let bounds = cx
                    .debug_bounds(id)
                    .unwrap_or_else(|| panic!("missing {id}"));
                assert!(
                    bounds.left() >= px(0.) && bounds.right() <= px(BAR_WIDTH),
                    "{id}: {bounds:?}"
                );
                assert!(
                    bounds.top() >= px(0.) && bounds.bottom() <= px(BAR_HEIGHT),
                    "{id}: {bounds:?}"
                );
            }
            for id in absent {
                assert!(cx.debug_bounds(id).is_none(), "unexpected {id}");
            }
            cx.update(|_, cx| {
                let home = &bar.read(cx).home;
                assert_eq!(
                    home.read(cx).services.control.published().state,
                    RecorderState::Idle
                );
                assert!(
                    home.read(cx)
                        .services
                        .control
                        .published()
                        .session_id
                        .is_none()
                );
            });
        }
        // An unavailable microphone has no action; no optimistic mutation.
        let mic = cx.debug_bounds("compact-microphone").unwrap().center();
        cx.simulate_click(mic, Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, cx| assert!(bar.read(cx).home.read(cx).pending_mute.is_none()));
    }

    #[gpui::test]
    fn microphone_uses_configured_identity_and_waits_for_runtime_acknowledgement(
        cx: &mut TestAppContext,
    ) {
        let (home, cx) = cx.add_window_view(super::super::tests::home);
        cx.update(|window, cx| {
            home.update(cx, |home, cx| {
                let selected = AudioSourceId("selected-microphone".into());
                let other = AudioSourceId("other-microphone".into());
                let source = |id| AudioSource {
                    controls: Default::default(),
                    capture_identity: None,
                    id,
                    kind: AudioSourceKind::Microphone,
                    display_name: "Microphone".into(),
                    process_id: None,
                    app_identifier: None,
                    is_active: true,
                    route: AudioRoute::MainMix,
                    muted: false,
                };
                home.model.snapshot.audio_sources = vec![source(other), source(selected.clone())];
                home.services
                    .control
                    .observe_sources(home.model.snapshot.audio_sources.clone());
                home.model.request = Some(RecordingRequest {
                    target: CaptureTarget::Display(DisplayId("test".into())),
                    fps_mode: FpsMode::AutoMatchSource,
                    resolution: Resolution::Native,
                    quality: Quality::Standard,
                    encoder: EncoderPreference::Auto,
                    optimization: None,
                    audio_routes: vec![AudioRouteSelection {
                        controls: Default::default(),
                        source_id: selected.clone(),
                        route: AudioRoute::MainMix,
                        muted: false,
                    }],
                    camera: None,
                    show_cursor: true,
                    output: OutputSettings {
                        directory: "recordings".into(),
                        file_stem: "test".into(),
                    },
                });
                assert_eq!(home.configured_microphone().unwrap().id, selected);
                home.dispatch(Action::MuteAudio(selected.clone(), true), window, cx);
                assert!(!home.configured_microphone().unwrap().muted);
                assert_eq!(home.pending_mute, Some((selected.clone(), true)));
                assert!(!home.can_mute(&selected));
                home.pending_mute = None;
                home.model.snapshot.audio_sources[1].is_active = false;
                assert!(!home.can_mute(&selected));
                assert_eq!(home.configured_microphone().unwrap().id, selected);
                home.model.request.as_mut().unwrap().audio_routes[0].route = AudioRoute::Ignore;
                assert!(home.configured_microphone().is_none());
            });
        });
    }
}
