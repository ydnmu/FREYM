//! Assignment is intercepted before GPUI's keymap, not in late KeyDown dispatch.
use super::*;
use freym_platform::hotkeys::{parse, same_binding, same_key, serialize_key};

#[derive(Default)]
pub(super) struct ShortcutUi {
    pub manager_error: Option<String>,
    pub app_edit: Option<ApplicationShortcut>,
    app_pending: Option<(String, Option<(ApplicationShortcut, String)>)>,
    application: Option<ApplicationShortcut>,
    previous_app: Option<serde_json::Value>,
    previous_defaults: Option<serde_json::Value>,
    pub previous: Option<ShortcutBindings>,
    interceptor: Option<gpui::Subscription>,
    activation: Option<gpui::Subscription>,
    window: Option<gpui::AnyWindowHandle>,
    pub(super) pending: Option<(String, Option<ShortcutBindings>)>,
    pub error: Option<String>,
    local: Option<ShortcutAction>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ApplicationShortcut {
    Settings,
    Fullscreen,
    OpenOutput,
    Refresh,
    Sources,
    Video,
    Audio,
    Camera,
    Files,
    Shortcuts,
    Diagnostics,
    Recovery,
}
impl ApplicationShortcut {
    pub const ALL: [Self; 12] = [
        Self::Settings,
        Self::Fullscreen,
        Self::OpenOutput,
        Self::Refresh,
        Self::Sources,
        Self::Diagnostics,
        Self::Recovery,
        Self::Video,
        Self::Audio,
        Self::Camera,
        Self::Files,
        Self::Shortcuts,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Settings => "Open Settings",
            Self::Fullscreen => "Toggle fullscreen",
            Self::OpenOutput => "Open recording folder",
            Self::Refresh => "Refresh sources",
            Self::Sources => "Choose capture source",
            Self::Video => "Video settings",
            Self::Audio => "Audio settings",
            Self::Camera => "Camera settings",
            Self::Files => "File settings",
            Self::Shortcuts => "Shortcut settings",
            Self::Diagnostics => "Recording details",
            Self::Recovery => "Recover a recording",
        }
    }
    pub fn default_binding(self) -> &'static str {
        match self {
            Self::Settings => "Ctrl+Comma",
            Self::Fullscreen => "F11",
            Self::OpenOutput => "Ctrl+Shift+O",
            Self::Refresh => "Ctrl+Shift+F5",
            Self::Sources => "Ctrl+Shift+D",
            Self::Video => "Ctrl+Shift+F6",
            Self::Audio => "Ctrl+Shift+F7",
            Self::Camera => "Ctrl+Shift+F8",
            Self::Files => "Ctrl+Shift+F9",
            Self::Shortcuts => "Ctrl+Shift+F10",
            Self::Diagnostics => "Ctrl+Shift+I",
            Self::Recovery => "Ctrl+Shift+U",
        }
    }
    pub fn settings_category(self) -> Option<usize> {
        match self {
            Self::Video => Some(1),
            Self::Audio => Some(2),
            Self::Camera => Some(6),
            Self::Files => Some(3),
            Self::Shortcuts => Some(7),
            _ => None,
        }
    }
}
pub(super) fn binding(keys: &ShortcutBindings, action: ShortcutAction) -> &str {
    match action {
        ShortcutAction::StartStop => &keys.start_stop,
        ShortcutAction::PauseResume => &keys.pause_resume,
        ShortcutAction::SaveReplay => &keys.save_replay,
    }
}
fn serialized(key: &gpui::Keystroke) -> Result<Option<String>, String> {
    let m = key.modifiers;
    if m.function {
        return Err("The system Fn modifier cannot be registered as a global shortcut.".into());
    }
    serialize_key(&key.key, m.control, m.alt, m.shift, m.platform)
}
impl Home {
    pub(super) fn shortcut_capture_active(&self) -> bool {
        self.shortcut_edit.is_some()
            || self.shortcut_ui.pending.is_some()
            || self.shortcut_ui.app_edit.is_some()
            || self.shortcut_ui.app_pending.is_some()
    }
    fn effective_recording_binding(
        &self,
        keys: &ShortcutBindings,
        action: ShortcutAction,
    ) -> String {
        let saved = binding(keys, action);
        if !saved.is_empty() {
            return saved.into();
        }
        if self
            .model
            .preferences
            .extra_preferences
            .get("cleared_recording_shortcuts")
            .and_then(|v| v.get(format!("{action:?}")))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return String::new();
        }
        match action {
            ShortcutAction::StartStop => "Ctrl+Shift+R",
            ShortcutAction::PauseResume => "Ctrl+Shift+P",
            ShortcutAction::SaveReplay => "Ctrl+Shift+S",
        }
        .into()
    }
    pub(super) fn recording_binding(&self, action: ShortcutAction) -> String {
        self.effective_recording_binding(&self.model.shortcuts, action)
    }
    pub(super) fn application_binding(&self, action: ApplicationShortcut) -> String {
        let overrides = self
            .model
            .preferences
            .extra_preferences
            .get("application_shortcuts");
        if let Some(saved) = overrides
            .and_then(|v| v.get(format!("{action:?}")))
            .and_then(|v| v.as_str())
        {
            return saved.into();
        }
        let default = action.default_binding();
        // A newly introduced default must never steal an existing explicit
        // binding (or a recording binding) during an upgrade.
        let recording_conflict = [
            ShortcutAction::StartStop,
            ShortcutAction::PauseResume,
            ShortcutAction::SaveReplay,
        ]
        .into_iter()
        .any(|other| {
            let key = self.recording_binding(other);
            !key.is_empty() && same_binding(&key, default)
        });
        let saved_conflict = ApplicationShortcut::ALL
            .into_iter()
            .filter(|other| *other != action)
            .filter_map(|other| {
                overrides
                    .and_then(|v| v.get(format!("{other:?}")))
                    .and_then(|v| v.as_str())
            })
            .any(|key| !key.is_empty() && same_binding(key, default));
        if recording_conflict || saved_conflict {
            String::new()
        } else {
            default.into()
        }
    }
    fn application_conflict(
        &self,
        text: &str,
        except: Option<ApplicationShortcut>,
    ) -> Option<&'static str> {
        ApplicationShortcut::ALL
            .into_iter()
            .filter(|a| Some(*a) != except)
            .any(|a| {
                let key = self.application_binding(a);
                !key.is_empty() && same_binding(&key, text)
            })
            .then_some("This key is assigned to another application command.")
    }
    pub(super) fn set_application_binding(
        &mut self,
        action: ApplicationShortcut,
        text: String,
        cx: &mut Context<Self>,
    ) {
        if self.model.closing {
            return;
        }
        self.cancel_shortcut_capture(cx);
        if !text.is_empty() {
            let parsed = parse(&ShortcutBindings {
                start_stop: text.clone(),
                ..Default::default()
            });
            let reserved = if same_binding(&text, action.default_binding()) {
                None
            } else {
                freym_platform::hotkeys::local_conflict(&text, ShortcutAction::StartStop)
            };
            let recording = [
                ShortcutAction::StartStop,
                ShortcutAction::PauseResume,
                ShortcutAction::SaveReplay,
            ]
            .into_iter()
            .any(|a| {
                let key = self.recording_binding(a);
                !key.is_empty() && same_binding(&key, &text)
            });
            if parsed.is_err()
                || reserved.is_some()
                || recording
                || self.application_conflict(&text, Some(action)).is_some()
            {
                self.shortcut_ui.error =
                    Some("Shortcut conflicts with another command or a reserved key.".into());
                cx.notify();
                return;
            }
        }
        self.shortcut_ui.previous = Some(self.model.shortcuts.clone());
        self.shortcut_ui.previous_defaults = Some(
            self.model
                .preferences
                .extra_preferences
                .get("cleared_recording_shortcuts")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        );
        self.shortcut_ui.previous_app = Some(
            self.model
                .preferences
                .extra_preferences
                .get("application_shortcuts")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        );
        let value = self
            .model
            .preferences
            .extra_preferences
            .entry("application_shortcuts".into())
            .or_insert_with(|| serde_json::json!({}));
        if !value.is_object() {
            *value = serde_json::json!({});
        }
        value[format!("{action:?}")] = serde_json::Value::String(text);
        self.shortcut_ui.error = None;
        self.persist_mode(cx);
    }
    pub(super) fn begin_application_shortcut(
        &mut self,
        action: ApplicationShortcut,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_shortcut_capture(ShortcutAction::StartStop, window, cx);
        if self.shortcut_edit.is_some() {
            self.shortcut_edit = None;
            self.shortcut_ui.app_edit = Some(action);
        }
    }
    fn capture_application_shortcut(
        &mut self,
        action: ApplicationShortcut,
        key: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) {
        let text = if key.key == "escape" {
            None
        } else if key.key == "backspace" {
            Some(String::new())
        } else {
            match serialized(key) {
                Ok(Some(t)) => Some(t),
                Ok(None) => return,
                Err(e) => {
                    self.shortcut_ui.error = Some(e);
                    cx.notify();
                    return;
                }
            }
        };
        let release = if key.key == "escape" {
            "Escape".into()
        } else if key.key == "backspace" {
            "Backspace".into()
        } else {
            serialized(key).ok().flatten().unwrap()
        };
        self.shortcut_ui.app_pending = Some((release, text.map(|text| (action, text))));
        cx.notify();
    }

    pub(super) fn shortcut_feedback(&self) -> &'static str {
        if self.shortcut_ui.pending.is_some() || self.shortcut_ui.app_pending.is_some() {
            "Release keys…"
        } else {
            "Press keys…"
        }
    }
    pub(super) fn shortcut_supported(&self, action: ShortcutAction) -> bool {
        match action {
            ShortcutAction::StartStop => true,
            ShortcutAction::PauseResume => self.model.snapshot.capabilities.can_pause,
            ShortcutAction::SaveReplay => self.model.snapshot.capabilities.can_replay,
        }
    }
    pub(super) fn native_shortcuts(&self, saved: &ShortcutBindings) -> ShortcutBindings {
        ShortcutBindings {
            start_stop: saved.start_stop.clone(),
            pause_resume: if self.shortcut_supported(ShortcutAction::PauseResume) {
                saved.pause_resume.clone()
            } else {
                String::new()
            },
            save_replay: if self.shortcut_supported(ShortcutAction::SaveReplay) {
                saved.save_replay.clone()
            } else {
                String::new()
            },
        }
    }
    pub(super) fn clear_shortcut(&mut self, action: ShortcutAction, cx: &mut Context<Self>) {
        if self.model.shortcuts_pending {
            return;
        }
        self.cancel_shortcut_capture(cx);
        let previous_defaults = self
            .model
            .preferences
            .extra_preferences
            .get("cleared_recording_shortcuts")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let cleared = self
            .model
            .preferences
            .extra_preferences
            .entry("cleared_recording_shortcuts".into())
            .or_insert_with(|| serde_json::json!({}));
        if !cleared.is_object() {
            *cleared = serde_json::json!({});
        }
        cleared[format!("{action:?}")] = serde_json::Value::Bool(true);
        let mut next = self.model.shortcuts.clone();
        match action {
            ShortcutAction::StartStop => next.start_stop.clear(),
            ShortcutAction::PauseResume => next.pause_resume.clear(),
            ShortcutAction::SaveReplay => next.save_replay.clear(),
        }
        self.configure_shortcuts(next, cx);
        self.shortcut_ui.previous_defaults = Some(previous_defaults);
        cx.notify();
    }
    pub(super) fn restore_shortcuts(&mut self, cx: &mut Context<Self>) {
        if self.model.shortcuts_pending {
            return;
        }
        self.cancel_shortcut_capture(cx);
        if let Some(previous) = self.shortcut_ui.previous_defaults.take() {
            if previous.is_null() {
                self.model
                    .preferences
                    .extra_preferences
                    .remove("cleared_recording_shortcuts");
            } else {
                self.model
                    .preferences
                    .extra_preferences
                    .insert("cleared_recording_shortcuts".into(), previous);
            }
        }
        if let Some(previous) = self.shortcut_ui.previous_app.take() {
            if previous.is_null() {
                self.model
                    .preferences
                    .extra_preferences
                    .remove("application_shortcuts");
            } else {
                self.model
                    .preferences
                    .extra_preferences
                    .insert("application_shortcuts".into(), previous);
            }
        }
        if let Some(previous) = self.shortcut_ui.previous.clone() {
            self.configure_shortcuts(previous, cx);
        }
        cx.notify();
    }
    pub(super) fn install_shortcut_interceptor(&mut self, cx: &mut Context<Self>) {
        if self.shortcut_ui.interceptor.is_some() {
            return;
        }
        let view = cx.weak_entity();
        self.shortcut_ui.interceptor = Some(cx.intercept_keystrokes(move |event, window, cx| {
            let _ = view.update(cx, |home, cx| {
                home.shortcut_ui.local = None;
                home.shortcut_ui.application = None;
                let here = window.window_handle();
                if home.shortcut_capture_active()
                    && here == home.shortcut_ui.window.unwrap_or(home.owner_window)
                {
                    window.prevent_default();
                    cx.stop_propagation();
                    home.capture_shortcut(&event.keystroke, cx);
                    return;
                }
                let owned = here == home.owner_window;
                if !owned || home.model.closing || home.model.shortcuts_pending {
                    return;
                }
                let Some(text) = serialized(&event.keystroke).ok().flatten() else {
                    return;
                };
                for action in [
                    ShortcutAction::StartStop,
                    ShortcutAction::PauseResume,
                    ShortcutAction::SaveReplay,
                ] {
                    if !home.shortcut_supported(action) {
                        continue;
                    }
                    let saved =
                        home.effective_recording_binding(&home.save_state.shortcuts, action);
                    if !saved.is_empty() && same_binding(&saved, &text) {
                        window.prevent_default();
                        cx.stop_propagation();
                        if !home
                            .hotkeys
                            .as_ref()
                            .is_some_and(|h| h.is_active(action, &saved))
                        {
                            // Global failure/unavailability does not discard the user's
                            // saved focused-window binding. Native-active keys dispatch
                            // through their native event only, never twice.
                            home.shortcut_ui.local = Some(action);
                        }
                        break;
                    }
                }
                if !window.default_prevented() {
                    for action in ApplicationShortcut::ALL {
                        let saved = home.application_binding(action);
                        if !saved.is_empty() && same_binding(&saved, &text) {
                            window.prevent_default();
                            cx.stop_propagation();
                            home.shortcut_ui.application = Some(action);
                            break;
                        }
                    }
                }
            });
        }));
    }
    pub(super) fn dispatch_local_shortcut(
        &mut self,
        held: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(action) = self.shortcut_ui.application.take().filter(|_| !held) {
            if let Some(category) = action.settings_category() {
                self.dispatch(Action::Settings, window, cx);
                self.dispatch(Action::SettingsCategory(category), window, cx);
            } else {
                match action {
                    ApplicationShortcut::Settings => self.dispatch(Action::Settings, window, cx),
                    ApplicationShortcut::Fullscreen => window.toggle_fullscreen(),
                    ApplicationShortcut::OpenOutput => {
                        self.dispatch(Action::OpenOutput, window, cx)
                    }
                    ApplicationShortcut::Refresh if self.model.editable() => {
                        self.dispatch(Action::Refresh, window, cx)
                    }
                    ApplicationShortcut::Refresh => (),
                    ApplicationShortcut::Sources => {
                        self.dispatch(Action::OpenSourcePicker, window, cx)
                    }
                    ApplicationShortcut::Diagnostics => {
                        self.dispatch(Action::Diagnostics, window, cx)
                    }
                    ApplicationShortcut::Recovery => self.dispatch(Action::Recovery, window, cx),
                    ApplicationShortcut::Video
                    | ApplicationShortcut::Audio
                    | ApplicationShortcut::Camera
                    | ApplicationShortcut::Files
                    | ApplicationShortcut::Shortcuts => {
                        unreachable!("settings navigation handled above")
                    }
                }
            }
        }
        if let Some(action) = self.shortcut_ui.local.take().filter(|_| !held) {
            self.dispatch(
                match action {
                    ShortcutAction::StartStop => Action::Record,
                    ShortcutAction::PauseResume => Action::Pause,
                    ShortcutAction::SaveReplay => Action::SaveReplay,
                },
                window,
                cx,
            );
        }
    }
    pub(super) fn begin_shortcut_capture(
        &mut self,
        action: ShortcutAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.model.shortcuts_pending || self.model.closing || !self.shortcut_supported(action) {
            return;
        }
        self.cancel_shortcut_capture(cx);
        if let Some(hotkeys) = &mut self.hotkeys
            && let Err(error) = hotkeys.suspend()
        {
            self.shortcut_ui.error = Some(error);
            return;
        }
        self.shortcut_ui.error = None;
        self.shortcut_ui.window = Some(window.window_handle());
        self.shortcut_edit = Some(action);
        self.shortcut_ui.activation =
            Some(cx.observe_window_activation(window, |home, window, cx| {
                if !window.is_window_active() {
                    home.cancel_shortcut_capture(cx);
                    cx.notify();
                }
            }));
    }
    pub(super) fn cancel_shortcut_capture(&mut self, _: &mut Context<Self>) {
        let had_app =
            self.shortcut_ui.app_edit.take().is_some() || self.shortcut_ui.app_pending.is_some();
        self.shortcut_ui.app_pending = None;
        let had_capture =
            self.shortcut_edit.take().is_some() || self.shortcut_ui.pending.is_some() || had_app;
        self.shortcut_ui.pending = None;
        self.shortcut_ui.window = None;
        self.shortcut_ui.activation = None;
        if had_capture
            && let Some(hotkeys) = &mut self.hotkeys
            && let Err(error) = hotkeys.resume()
        {
            self.shortcut_ui.error =
                Some(format!("Could not restore all global shortcuts: {error}"));
        }
    }
    fn capture_shortcut(&mut self, key: &gpui::Keystroke, cx: &mut Context<Self>) {
        if self.shortcut_ui.pending.is_some() || self.shortcut_ui.app_pending.is_some() {
            return;
        } // includes repeats until release
        if let Some(action) = self.shortcut_ui.app_edit {
            self.capture_application_shortcut(action, key, cx);
            return;
        }
        let Some(action) = self.shortcut_edit else {
            return;
        };
        if key.key == "escape" {
            self.shortcut_edit = None;
            self.shortcut_ui.pending = Some(("Escape".into(), None));
            cx.notify();
            return;
        }
        let text = if key.key == "backspace" {
            String::new()
        } else {
            match serialized(key) {
                Ok(Some(text)) => text,
                Ok(None) => return,
                Err(error) => {
                    self.shortcut_ui.error = Some(error);
                    cx.notify();
                    return;
                }
            }
        };
        let mut next = self.model.shortcuts.clone();
        match action {
            ShortcutAction::StartStop => next.start_stop = text,
            ShortcutAction::PauseResume => next.pause_resume = text,
            ShortcutAction::SaveReplay => next.save_replay = text,
        }
        let release = if key.key == "backspace" {
            "Backspace".into()
        } else {
            serialized(key).ok().flatten().unwrap()
        };
        self.shortcut_ui.pending = Some((release, Some(next)));
        cx.notify();
    }
    pub(super) fn release_shortcut_capture(
        &mut self,
        key: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shortcut_ui.window != Some(window.window_handle()) {
            return;
        }
        if let Some((pressed, _)) = &self.shortcut_ui.app_pending {
            let Some(released) = serialized(key).ok().flatten() else {
                return;
            };
            if !same_key(pressed, &released) {
                return;
            }
            cx.stop_propagation();
            window.prevent_default();
            let (_, next) = self.shortcut_ui.app_pending.take().unwrap();
            self.cancel_shortcut_capture(cx);
            if let Some((action, text)) = next {
                self.set_application_binding(action, text, cx);
            }
            cx.notify();
            return;
        }
        let Some((pressed, _)) = &self.shortcut_ui.pending else {
            return;
        };
        let Some(released) = serialized(key).ok().flatten() else {
            return;
        };
        if !same_key(pressed, &released) {
            return;
        }
        cx.stop_propagation();
        window.prevent_default();
        let (_, next) = self.shortcut_ui.pending.take().unwrap();
        self.shortcut_edit = None;
        self.shortcut_ui.window = None;
        self.shortcut_ui.activation = None;
        if let Some(next) = next {
            self.configure_shortcuts(next, cx);
        } else if let Some(hotkeys) = &mut self.hotkeys
            && let Err(error) = hotkeys.resume()
        {
            self.shortcut_ui.error = Some(error);
        }
        cx.notify();
    }
    fn configure_shortcuts(&mut self, next: ShortcutBindings, cx: &mut Context<Self>) {
        let restoring = self.shortcut_ui.previous.as_ref() == Some(&next);
        let validation = parse(&next).map(|_| ()).and_then(|_| {
            for action in [
                ShortcutAction::StartStop,
                ShortcutAction::PauseResume,
                ShortcutAction::SaveReplay,
            ] {
                let key = binding(&next, action);
                if key.is_empty() || key == binding(&self.model.shortcuts, action) {
                    continue;
                }
                if !self.shortcut_supported(action) && !restoring {
                    return Err(
                        "This recording action is unavailable on the current platform.".into(),
                    );
                }
                if !restoring {
                    for other in [
                        ShortcutAction::StartStop,
                        ShortcutAction::PauseResume,
                        ShortcutAction::SaveReplay,
                    ] {
                        let other_key = self.effective_recording_binding(&next, other);
                        if other != action && !other_key.is_empty() && same_binding(key, &other_key)
                        {
                            return Err("This key is assigned to another recording command.".into());
                        }
                    }
                    if let Some(error) = freym_platform::hotkeys::local_conflict(key, action)
                        .or_else(|| self.application_conflict(key, None))
                    {
                        return Err(error.to_owned());
                    }
                }
            }
            Ok(())
        });
        if let Err(error) = validation {
            self.shortcut_ui.error = Some(error);
        } else {
            self.shortcut_ui.previous = Some(self.model.shortcuts.clone());
            self.shortcut_ui.previous_defaults = Some(
                self.model
                    .preferences
                    .extra_preferences
                    .get("cleared_recording_shortcuts")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            );
            for action in [
                ShortcutAction::StartStop,
                ShortcutAction::PauseResume,
                ShortcutAction::SaveReplay,
            ] {
                if !binding(&next, action).is_empty()
                    && let Some(v) = self
                        .model
                        .preferences
                        .extra_preferences
                        .get_mut("cleared_recording_shortcuts")
                        .and_then(|v| v.as_object_mut())
                {
                    v.remove(&format!("{action:?}"));
                }
            }
            self.model.shortcuts = next;
            self.shortcut_ui.error = None;
            self.persist_mode(cx);
        }
        if let Some(hotkeys) = &mut self.hotkeys
            && let Err(error) = hotkeys.resume()
        {
            self.shortcut_ui.error = Some(error);
        }
    }
    #[allow(dead_code)]
    pub(super) fn shortcut_status(&self, action: ShortcutAction) -> &'static str {
        let saved = binding(&self.save_state.shortcuts, action);
        if self.model.shortcuts != self.save_state.shortcuts {
            return if self.save_state.error.is_some() {
                "Save failed"
            } else {
                "Saving…"
            };
        }
        if saved.is_empty() {
            return "Unassigned";
        }
        if self.model.shortcuts_pending {
            return "Saving…";
        }
        if !self.shortcut_supported(action) {
            return "Saved · unsupported action";
        }
        if self.shortcut_edit.is_some() || self.shortcut_ui.pending.is_some() {
            return "Saved";
        }

        if self
            .hotkeys
            .as_ref()
            .is_some_and(|hotkeys| hotkeys.is_active(action, saved))
        {
            "Active · global"
        } else if freym_platform::hotkeys::global_availability().is_err() {
            "Saved · local only"
        } else {
            "Registration failed · saved"
        }
    }
}
