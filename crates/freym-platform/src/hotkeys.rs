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

pub struct Hotkeys {
    manager: GlobalHotKeyManager,
    active: Vec<(HotKey, ShortcutAction)>,
}
impl Hotkeys {
    /// Construct once, on the GPUI event-loop thread (required on macOS).
    pub fn new(on_press: impl Fn(u32) + Send + Sync + 'static) -> Result<Self, String> {
        let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if event.state == HotKeyState::Pressed {
                on_press(event.id);
            }
        }));
        Ok(Self {
            manager,
            active: Vec::new(),
        })
    }
    pub fn action(&self, id: u32) -> Option<ShortcutAction> {
        self.active
            .iter()
            .find(|(key, _)| key.id() == id)
            .map(|(_, action)| *action)
    }
    pub fn configure(&mut self, bindings: &ShortcutBindings) -> Result<(), String> {
        let next = parse(bindings)?;
        let manager = &self.manager;
        apply_bindings(
            &mut self.active,
            next,
            |key| manager.register(key).map_err(|error| error.to_string()),
            |key| manager.unregister(key).map_err(|error| error.to_string()),
        )
    }
}

fn apply_bindings(
    active: &mut Vec<(HotKey, ShortcutAction)>,
    next: Vec<(HotKey, ShortcutAction)>,
    mut register: impl FnMut(HotKey) -> Result<(), String>,
    mut unregister: impl FnMut(HotKey) -> Result<(), String>,
) -> Result<(), String> {
    let previous = active.clone();
    for (key, _) in &previous {
        if let Err(e) = unregister(*key) {
            let rollback = restore(active, &previous, &mut register);
            return Err(format!("Could not unregister shortcut: {e}. {rollback}"));
        }
        active.retain(|(current, _)| current.id() != key.id());
    }
    for (key, action) in next {
        if let Err(e) = register(key) {
            let mut cleanup_failures = Vec::new();
            active.retain(|(registered, _)| match unregister(*registered) {
                Ok(()) => false,
                Err(error) => {
                    cleanup_failures.push(error);
                    true
                }
            });
            let rollback = restore(active, &previous, &mut register);
            let cleanup = if cleanup_failures.is_empty() {
                String::new()
            } else {
                format!(
                    " Could not unregister all replacement shortcuts: {}.",
                    cleanup_failures.join("; ")
                )
            };
            return Err(format!(
                "Global shortcut conflict: {e}.{cleanup} {rollback}"
            ));
        }
        active.push((key, action));
    }
    Ok(())
}

fn restore(
    active: &mut Vec<(HotKey, ShortcutAction)>,
    previous: &[(HotKey, ShortcutAction)],
    register: &mut impl FnMut(HotKey) -> Result<(), String>,
) -> String {
    let mut failures = Vec::new();
    for (key, action) in previous {
        if let Some((_, current_action)) = active.iter_mut().find(|(k, _)| k.id() == key.id()) {
            *current_action = *action;
            continue;
        }
        match register(*key) {
            Ok(()) => active.push((*key, *action)),
            Err(e) => failures.push(e),
        }
    }
    if failures.is_empty() {
        "Previous bindings restored.".into()
    } else {
        format!(
            "Could not restore all previous bindings: {}",
            failures.join("; ")
        )
    }
}
impl Drop for Hotkeys {
    fn drop(&mut self) {
        for (key, _) in &self.active {
            let _ = self.manager.unregister(*key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;

    struct Backend {
        registered: HashSet<u32>,
        conflict: u32,
        fail_unregister_on: usize,
        unregister_calls: usize,
    }

    impl Backend {
        fn new(active: &[(HotKey, ShortcutAction)], conflict: HotKey, fail_on: usize) -> Self {
            Self {
                registered: active.iter().map(|(key, _)| key.id()).collect(),
                conflict: conflict.id(),
                fail_unregister_on: fail_on,
                unregister_calls: 0,
            }
        }

        fn register(&mut self, key: HotKey) -> Result<(), String> {
            if key.id() == self.conflict || !self.registered.insert(key.id()) {
                return Err("registration conflict".into());
            }
            Ok(())
        }

        fn unregister(&mut self, key: HotKey) -> Result<(), String> {
            self.unregister_calls += 1;
            if self.unregister_calls == self.fail_unregister_on {
                return Err("unregister failed".into());
            }
            assert!(self.registered.remove(&key.id()));
            Ok(())
        }
    }

    fn keys() -> [HotKey; 3] {
        ["Ctrl+Shift+R", "Ctrl+Shift+P", "Ctrl+Shift+S"].map(|key| key.parse().unwrap())
    }

    fn apply(
        active: &mut Vec<(HotKey, ShortcutAction)>,
        next: Vec<(HotKey, ShortcutAction)>,
        backend: &RefCell<Backend>,
    ) -> Result<(), String> {
        apply_bindings(
            active,
            next,
            |key| backend.borrow_mut().register(key),
            |key| backend.borrow_mut().unregister(key),
        )
    }

    fn assert_owned(active: &[(HotKey, ShortcutAction)], backend: &RefCell<Backend>) {
        let tracked: HashSet<_> = active.iter().map(|(key, _)| key.id()).collect();
        assert_eq!(tracked.len(), active.len());
        assert_eq!(tracked, backend.borrow().registered);
    }

    #[test]
    fn rollback_retains_replacement_keys_that_could_not_be_unregistered() {
        let [old, replacement, conflict] = keys();
        let previous = vec![(old, ShortcutAction::StartStop)];
        let mut active = previous.clone();
        let backend = RefCell::new(Backend::new(&active, conflict, 2));

        let failure = apply(
            &mut active,
            vec![
                (replacement, ShortcutAction::PauseResume),
                (conflict, ShortcutAction::SaveReplay),
            ],
            &backend,
        )
        .unwrap_err();

        assert!(failure.contains("Could not unregister all replacement shortcuts"));
        assert!(active.contains(&previous[0]));
        assert!(active.contains(&(replacement, ShortcutAction::PauseResume)));
        assert_owned(&active, &backend);
    }

    #[test]
    fn rollback_restores_the_previous_action_for_a_retained_key() {
        let [old, conflict, _] = keys();
        let previous = vec![(old, ShortcutAction::StartStop)];
        let mut active = previous.clone();
        let backend = RefCell::new(Backend::new(&active, conflict, 2));

        let failure = apply(
            &mut active,
            vec![
                (old, ShortcutAction::PauseResume),
                (conflict, ShortcutAction::SaveReplay),
            ],
            &backend,
        )
        .unwrap_err();

        assert!(failure.contains("Could not unregister all replacement shortcuts"));
        assert_eq!(active, previous);
        assert_owned(&active, &backend);
    }

    #[test]
    fn a_successful_rollback_removes_replacements_and_restores_previous_bindings() {
        let [old, replacement, conflict] = keys();
        let previous = vec![(old, ShortcutAction::StartStop)];
        let mut active = previous.clone();
        let backend = RefCell::new(Backend::new(&active, conflict, 0));

        let failure = apply(
            &mut active,
            vec![
                (replacement, ShortcutAction::PauseResume),
                (conflict, ShortcutAction::SaveReplay),
            ],
            &backend,
        )
        .unwrap_err();

        assert!(failure.contains("Previous bindings restored."));
        assert!(!failure.contains("Could not unregister"));
        assert_eq!(active, previous);
        assert_owned(&active, &backend);
    }

    #[test]
    fn partial_previous_unregistration_failure_preserves_and_restores_owned_keys() {
        let [first, second, conflict] = keys();
        let previous = vec![
            (first, ShortcutAction::StartStop),
            (second, ShortcutAction::PauseResume),
        ];
        let mut active = previous.clone();
        let backend = RefCell::new(Backend::new(&active, conflict, 2));

        let failure = apply(&mut active, Vec::new(), &backend).unwrap_err();

        assert!(failure.contains("Could not unregister shortcut"));
        assert!(previous.iter().all(|binding| active.contains(binding)));
        assert_owned(&active, &backend);
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
