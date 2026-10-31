//! Native registrations live on the application event-loop thread. File IO is
//! deliberately outside this boundary. The same parser used by the former host
//! preserves saved bindings and detects semantic (including alias) conflicts.
use frame_contracts::ShortcutBindings;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState, hotkey::HotKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortcutAction {
    StartStop,
    PauseResume,
    SaveReplay,
}

pub fn platform_availability(os: &str, wayland: bool) -> Result<(), String> {
    match os {
        "windows"|"macos"=>Ok(()),
        "linux" if wayland=>Err("Global shortcuts are unavailable on Wayland: this build has no portal shortcut backend. Saved bindings work in the focused Recorder.".into()),
        // Preserve the existing X11 backend; Wayland needs a separate portal.
        "linux"=>Ok(()),
        _=>Err("Global shortcuts are unavailable on this platform. Saved bindings remain Recorder-local.".into()),
    }
}
pub fn global_availability() -> Result<(), String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"));
    platform_availability(std::env::consts::OS, wayland)
}

pub fn local_conflict(binding: &str, action: ShortcutAction) -> Option<&'static str> {
    for (key, owner) in [
        ("Ctrl+Shift+R", ShortcutAction::StartStop),
        ("Ctrl+Shift+P", ShortcutAction::PauseResume),
    ] {
        if owner != action && same_binding(binding, key) {
            return Some("This key is reserved by another built-in Recorder command.");
        }
    }
    for key in [
        "Ctrl+Comma",
        "F11",
        "Escape",
        "Backspace",
        "Delete",
        "Tab",
        "Shift+Tab",
        "Enter",
        "Space",
        "ArrowLeft",
        "ArrowRight",
        "ArrowUp",
        "ArrowDown",
        "Home",
        "End",
        "Ctrl+A",
        "Ctrl+C",
        "Ctrl+X",
        "Ctrl+V",
        "Super+A",
        "Super+C",
        "Super+X",
        "Super+V",
    ] {
        if same_binding(binding, key) {
            return Some(
                "This key conflicts with built-in navigation, text editing or window commands. Choose another combination.",
            );
        }
    }
    None
}
pub fn plugin_conflict<'a>(
    binding: &str,
    enabled: impl Iterator<Item = &'a str>,
) -> Option<&'static str> {
    for id in enabled {
        if id == "freym.key-mouse" && same_binding(binding, "Ctrl+Alt+K") {
            return Some("Ctrl+Alt+K belongs to the enabled Key & Mouse plugin.");
        }
        if id == "freym.screen-ink" && same_binding(binding, "Ctrl+Alt+D") {
            return Some("Ctrl+Alt+D belongs to the enabled Screen Ink plugin.");
        }
    }
    None
}

pub fn parse(bindings: &ShortcutBindings) -> Result<Vec<(HotKey, ShortcutAction)>, String> {
    let mut result = Vec::new();
    for (text, action) in [
        (&bindings.start_stop, ShortcutAction::StartStop),
        (&bindings.pause_resume, ShortcutAction::PauseResume),
        (&bindings.save_replay, ShortcutAction::SaveReplay),
    ] {
        if text.trim().is_empty() {
            continue;
        }
        let key: HotKey = text
            .parse()
            .map_err(|e| format!("Invalid shortcut {text}: {e}"))?;
        if result
            .iter()
            .any(|(other, _): &(HotKey, ShortcutAction)| other.id() == key.id())
        {
            return Err(format!(
                "Shortcut {text} conflicts with another recording command."
            ));
        }
        result.push((key, action));
    }
    Ok(result)
}

#[derive(Clone, Copy, Debug)]
pub struct ShortcutEvent {
    pub id: u32,
    generation: u64,
}
trait Registrar {
    fn add(&self, key: HotKey) -> Result<(), String>;
    fn remove(&self, key: HotKey) -> Result<(), String>;
}
impl Registrar for GlobalHotKeyManager {
    fn add(&self, key: HotKey) -> Result<(), String> {
        self.register(key).map_err(|e| e.to_string())
    }
    fn remove(&self, key: HotKey) -> Result<(), String> {
        self.unregister(key).map_err(|e| e.to_string())
    }
}
#[derive(Default)]
struct Registrations {
    installed: Vec<(HotKey, ShortcutAction)>,
    committed: Vec<(HotKey, ShortcutAction)>,
}
impl Registrations {
    fn reconcile(
        &mut self,
        target: &[(HotKey, ShortcutAction)],
        manager: &impl Registrar,
    ) -> Result<(), String> {
        // Keep unchanged working registrations. Stage additions before retiring old keys.
        for (key, action) in target {
            if !self.installed.iter().any(|(old, _)| old.id() == key.id()) {
                manager.add(*key)?;
                self.installed.push((*key, *action));
            }
        }
        for (key, _) in self.installed.clone() {
            if !target.iter().any(|(next, _)| next.id() == key.id()) {
                manager.remove(key)?;
                self.installed.retain(|(old, _)| old.id() != key.id());
            }
        }
        self.installed = target.to_vec();
        Ok(())
    }
    fn apply(
        &mut self,
        next: Vec<(HotKey, ShortcutAction)>,
        manager: &impl Registrar,
    ) -> Result<(), String> {
        if let Err(error) = self.reconcile(&next, manager) {
            let rollback = self.reconcile(&self.committed.clone(), manager);
            return Err(format!(
                "Global shortcut registration failed: {error}. {}",
                match rollback {
                    Ok(()) => "Previous working bindings restored.".into(),
                    Err(error) => format!(
                        "Rollback was incomplete: {error}. Some saved bindings are not active."
                    ),
                }
            ));
        }
        self.committed = next;
        Ok(())
    }
    fn action(&self, id: u32) -> Option<ShortcutAction> {
        // Residual registrations after a failed cleanup must never dispatch an
        // uncommitted command. Track OS state separately from committed actions.
        self.committed
            .iter()
            .find(|(key, _)| key.id() == id)
            .filter(|_| self.installed.iter().any(|(key, _)| key.id() == id))
            .map(|(_, action)| *action)
    }
}
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
pub struct Hotkeys {
    manager: GlobalHotKeyManager,
    registrations: Registrations,
    suppressed: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    suspended: bool,
}
impl Hotkeys {
    pub fn new(on_press: impl Fn(ShortcutEvent) + Send + Sync + 'static) -> Result<Self, String> {
        global_availability()?;
        let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
        let suppressed = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let gate = suppressed.clone();
        let epoch = generation.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if event.state == HotKeyState::Pressed && !gate.load(Ordering::Acquire) {
                on_press(ShortcutEvent {
                    id: event.id,
                    generation: epoch.load(Ordering::Acquire),
                });
            }
        }));
        Ok(Self {
            manager,
            registrations: Registrations::default(),
            suppressed,
            generation,
            suspended: false,
        })
    }
    fn suppress(&self) {
        self.suppressed.store(true, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
    pub fn action_event(&self, event: ShortcutEvent) -> Option<ShortcutAction> {
        if self.suppressed.load(Ordering::Acquire)
            || event.generation != self.generation.load(Ordering::Acquire)
        {
            return None;
        }
        self.registrations.action(event.id)
    }
    pub fn is_active(&self, action: ShortcutAction, binding: &str) -> bool {
        !self.suspended
            && binding
                .parse::<HotKey>()
                .is_ok_and(|key| self.registrations.action(key.id()) == Some(action))
    }
    pub fn configure(&mut self, bindings: &ShortcutBindings) -> Result<(), String> {
        let next = parse(bindings)?;
        self.suppress();
        let result = self.registrations.apply(next, &self.manager);
        if !self.suspended {
            self.suppressed.store(false, Ordering::Release);
        }
        result
    }
    pub fn suspend(&mut self) -> Result<(), String> {
        self.suppress();
        self.suspended = true;
        if let Err(error) = self.registrations.reconcile(&[], &self.manager) {
            let restore = self.resume();
            return Err(format!(
                "Could not suspend global shortcuts for assignment: {error}. {restore:?}"
            ));
        }
        Ok(())
    }
    pub fn resume(&mut self) -> Result<(), String> {
        let result = self
            .registrations
            .reconcile(&self.registrations.committed.clone(), &self.manager);
        self.suspended = false;
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.suppressed.store(false, Ordering::Release);
        result
    }
}
impl Drop for Hotkeys {
    fn drop(&mut self) {
        self.suppress();
        for (key, _) in &self.registrations.installed {
            let _ = self.manager.unregister(*key);
        }
    }
}

pub fn same_binding(a: &str, b: &str) -> bool {
    a.parse::<HotKey>()
        .ok()
        .zip(b.parse::<HotKey>().ok())
        .is_some_and(|(a, b)| a.id() == b.id())
}
pub fn same_key(a: &str, b: &str) -> bool {
    a.parse::<HotKey>()
        .ok()
        .zip(b.parse::<HotKey>().ok())
        .is_some_and(|(a, b)| a.key == b.key)
}

/// Convert GPUI key names to the same physical key vocabulary as global-hotkey.
/// Windows punctuation uses the current keyboard layout rather than a US guess.
pub fn serialize_key(
    key: &str,
    mut control: bool,
    mut alt: bool,
    mut shift: bool,
    platform: bool,
) -> Result<Option<String>, String> {
    if matches!(
        key.to_ascii_lowercase().as_str(),
        "shift" | "control" | "ctrl" | "alt" | "platform" | "super" | "cmd" | "function" | "fn"
    ) {
        return Ok(None);
    }
    let mut token = key.to_owned();
    let lower = key.to_ascii_lowercase();
    if let Some(kp) = lower
        .strip_prefix("kp_")
        .or_else(|| lower.strip_prefix("numpad"))
    {
        token=match kp {
            _ if kp.len()==1 && kp.as_bytes()[0].is_ascii_digit()=>format!("Numpad{kp}"),
            "add"|"plus"=>"NumpadAdd".into(),"subtract"|"minus"=>"NumpadSubtract".into(),
            "multiply"=>"NumpadMultiply".into(),"divide"=>"NumpadDivide".into(),"decimal"|"delete"=>"NumpadDecimal".into(),
            "comma"=>return Err("Keypad separator is not supported by the global shortcut parser.".into()),
            "enter"=>return Err("Keypad Enter shares Return in the global backend and cannot be assigned independently.".into()),
            _=>return Err("This keypad key has no supported global mapping.".into()),
        };
    }
    #[cfg(target_os = "windows")]
    if key.chars().count() == 1 {
        #[allow(unsafe_code)]
        let scan = unsafe {
            windows_sys::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW(
                key.encode_utf16().next().unwrap(),
            )
        };
        if scan == -1 {
            return Err(
                "This keyboard-layout key cannot be registered as a global shortcut.".into(),
            );
        }
        let vk = scan as u16 & 0xff;
        shift |= scan as u16 & 0x100 != 0;
        control |= scan as u16 & 0x200 != 0;
        alt |= scan as u16 & 0x400 != 0;
        token = match vk {
            0x30..=0x39 | 0x41..=0x5a => char::from_u32(vk as u32).unwrap().to_string(),
            0x20 => "Space".into(),
            0xba => "Semicolon".into(),
            0xbb => "Equal".into(),
            0xbc => "Comma".into(),
            0xbd => "Minus".into(),
            0xbe => "Period".into(),
            0xbf => "Slash".into(),
            0xc0 => "Backquote".into(),
            0xdb => "BracketLeft".into(),
            0xdc => "Backslash".into(),
            0xdd => "BracketRight".into(),
            0xde => "Quote".into(),
            _ => {
                return Err(format!(
                    "This physical key (Windows VK {vk:#04x}) is not supported by the shortcut backend."
                ));
            }
        };
    }
    if token == "pageup" {
        token = "PageUp".into();
    }
    if token == "pagedown" {
        token = "PageDown".into();
    }
    if token == "return" {
        token = "Enter".into();
    }
    let mut parts = Vec::new();
    if control {
        parts.push("Ctrl".to_owned());
    }
    if alt {
        parts.push("Alt".to_owned());
    }
    if shift {
        parts.push("Shift".to_owned());
    }
    if platform {
        parts.push("Super".to_owned());
    }
    parts.push(token);
    let text = parts.join("+");
    text.parse::<HotKey>()
        .map_err(|e| format!("Unsupported shortcut: {e}"))?;
    Ok(Some(text))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn followup_keypad_conflicts_and_platform_availability_are_explicit() {
        for (input, expected) in [
            ("numpad1", "Numpad1"),
            ("kp_0", "Numpad0"),
            ("numpadadd", "NumpadAdd"),
            ("numpaddecimal", "NumpadDecimal"),
            ("numpaddivide", "NumpadDivide"),
            ("numpadsubtract", "NumpadSubtract"),
            ("numpadmultiply", "NumpadMultiply"),
        ] {
            let saved = serialize_key(input, true, false, false, false)
                .unwrap()
                .unwrap();
            assert!(same_binding(&saved, &format!("Ctrl+{expected}")));
        }
        assert!(!same_binding("Ctrl+Numpad1", "Ctrl+1"));
        assert!(
            serialize_key("kp_enter", false, false, false, false)
                .unwrap_err()
                .contains("independently")
        );
        assert!(local_conflict("F11", ShortcutAction::StartStop).is_some());
        assert!(local_conflict("Ctrl+Comma", ShortcutAction::PauseResume).is_some());
        assert!(local_conflict("Ctrl+Shift+R", ShortcutAction::StartStop).is_none());
        assert!(local_conflict("Ctrl+Shift+R", ShortcutAction::PauseResume).is_some());
        assert!(plugin_conflict("Ctrl+Alt+K", ["freym.key-mouse"].into_iter()).is_some());
        assert!(plugin_conflict("Ctrl+Alt+K", [].into_iter()).is_none());
        assert!(
            platform_availability("linux", true)
                .unwrap_err()
                .contains("Wayland")
        );
        assert!(platform_availability("linux", false).is_ok());
        assert!(platform_availability("windows", true).is_ok());
    }
    #[derive(Default)]
    struct Fake {
        installed: std::cell::RefCell<std::collections::HashSet<u32>>,
        fail_add: std::cell::Cell<Option<u32>>,
        fail_remove: std::cell::Cell<Option<u32>>,
        calls: std::cell::RefCell<Vec<(bool, u32)>>,
    }
    impl Registrar for Fake {
        fn add(&self, k: HotKey) -> Result<(), String> {
            self.calls.borrow_mut().push((true, k.id()));
            if self.fail_add.get() == Some(k.id()) {
                self.fail_add.set(None);
                return Err("owned by another application".into());
            }
            self.installed.borrow_mut().insert(k.id());
            Ok(())
        }
        fn remove(&self, k: HotKey) -> Result<(), String> {
            self.calls.borrow_mut().push((false, k.id()));
            if self.fail_remove.get() == Some(k.id()) {
                self.fail_remove.set(None);
                return Err("OS unregister failed".into());
            }
            self.installed.borrow_mut().remove(&k.id());
            Ok(())
        }
    }
    fn keys(a: &str, b: &str) -> Vec<(HotKey, ShortcutAction)> {
        parse(&ShortcutBindings {
            start_stop: a.into(),
            pause_resume: b.into(),
            save_replay: String::new(),
        })
        .unwrap()
    }
    #[test]
    fn revision_registration_conflict_and_retirement_failure_roll_back_without_losing_working_keys()
    {
        let os = Fake::default();
        let mut r = Registrations::default();
        let previous = keys("Ctrl+Shift+R", "Ctrl+Shift+P");
        r.apply(previous.clone(), &os).unwrap();
        os.calls.borrow_mut().clear();
        r.apply(previous.clone(), &os).unwrap();
        assert!(os.calls.borrow().is_empty());
        let next = keys("Ctrl+Shift+K", "Ctrl+Shift+P");
        os.fail_add.set(Some(next[0].0.id()));
        assert!(r.apply(next.clone(), &os).unwrap_err().contains("restored"));
        assert_eq!(
            r.action(previous[0].0.id()),
            Some(ShortcutAction::StartStop)
        );
        os.fail_remove.set(Some(previous[0].0.id()));
        assert!(r.apply(next.clone(), &os).is_err());
        assert_eq!(r.committed, previous);
        assert!(!os.installed.borrow().contains(&next[0].0.id()));
        r.apply(next.clone(), &os).unwrap();
        r.apply(vec![], &os).unwrap();
        assert!(os.installed.borrow().is_empty());
        r.apply(next.clone(), &os).unwrap();
        assert_eq!(r.action(next[0].0.id()), Some(ShortcutAction::StartStop));
    }
    #[test]
    fn revision_serialization_modifiers_shifted_punctuation_and_aliases() {
        assert_eq!(
            serialize_key("shift", false, false, true, false).unwrap(),
            None
        );
        assert!(same_binding(
            "Ctrl+Shift+R",
            &serialize_key("r", true, false, true, false)
                .unwrap()
                .unwrap()
        ));
        for key in [",", "+", "!", ":"] {
            let text = serialize_key(key, true, false, false, false)
                .unwrap_or_else(|e| panic!("{key:?}: {e}"))
                .unwrap();
            assert!(text.parse::<HotKey>().is_ok(), "{text}");
            assert!(!text.ends_with("++"));
        }
        // OEM8/OEM102 are absent from the pinned global-hotkey Windows map.
        // A layout that puts '?' there must report a limitation, never bind another key.
        match serialize_key("?", true, false, false, false) {
            Ok(Some(text)) => assert!(text.parse::<HotKey>().is_ok()),
            Err(e) => assert!(e.contains("not supported")),
            _ => panic!("punctuation cannot become a modifier-only binding"),
        }
        assert!(same_binding("Control+Shift+KeyR", "Ctrl+Shift+R"));
        assert!(same_key("Ctrl+Shift+R", "R"));
    }
    #[test]
    fn disabled_keys_and_alias_conflicts_preserve_existing_format() {
        let keys = ShortcutBindings {
            start_stop: "Ctrl+Shift+R".into(),
            pause_resume: "".into(),
            save_replay: "".into(),
        };
        assert_eq!(parse(&keys).unwrap().len(), 1);
        let duplicate = ShortcutBindings {
            pause_resume: "Control+Shift+KeyR".into(),
            ..keys
        };
        assert!(parse(&duplicate).unwrap_err().contains("conflicts"));
        assert!(
            parse(&ShortcutBindings {
                start_stop: "not-a-key".into(),
                ..Default::default()
            })
            .is_err()
        );
    }
}
