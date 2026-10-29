//! Bounded replaceable input: provider motion and Dock reports never displace
//! clicks. A delayed retry cannot overwrite a newer physical target.
use crate::ActionMessage;
use jwm_shell::edge_shell::Action;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use xbar_core::{
    UserAction, WindowToken,
    desktop_shell::{MAX_SHELL_OUTPUTS, ShellSnapshot},
};

const LIMIT: usize = MAX_SHELL_OUTPUTS * (xbar_core::model::MAX_MODEL_MINIMIZED_WINDOWS + 3);
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Key {
    Level(i32, u64, bool),
    Geometry(i32, u64, Option<WindowToken>),
}
fn key(message: &ActionMessage) -> Option<Key> {
    let (output, identity, Action::Bound { action, .. }) = message else {
        return None;
    };
    match action.as_ref() {
        Action::Level { audio, .. } => Some(Key::Level(*output, *identity, *audio)),
        Action::Model(UserAction::SetDockGeometry { window, .. }) => {
            Some(Key::Geometry(*output, *identity, *window))
        }
        _ => None,
    }
}
#[derive(Default)]
pub struct LatestActions {
    entries: HashMap<Key, (ActionMessage, Instant)>,
}
impl LatestActions {
    pub fn replaceable(message: &ActionMessage) -> bool {
        key(message).is_some()
    }
    pub fn push(&mut self, message: ActionMessage) -> bool {
        let Some(key) = key(&message) else {
            return false;
        };
        if self.entries.len() >= LIMIT && !self.entries.contains_key(&key) {
            return false;
        }
        self.entries.insert(key, (message, Instant::now()));
        true
    }
    pub fn retry(&mut self, message: ActionMessage) {
        if let Some(key) = key(&message)
            && self.entries.len() < LIMIT
        {
            self.entries
                .entry(key)
                .or_insert_with(|| (message, Instant::now() + Duration::from_millis(100)));
        }
    }
    pub fn pop(&mut self) -> Option<ActionMessage> {
        let now = Instant::now();
        let key = self
            .entries
            .iter()
            .find(|(_, (_, ready))| *ready <= now)
            .map(|(key, _)| *key)?;
        self.entries.remove(&key).map(|(message, _)| message)
    }
    pub fn retain_current(&mut self, snapshot: &ShellSnapshot) {
        self.entries.retain(|_, ((id, identity, action), _)| {
            let Action::Bound {
                session,
                generation,
                action,
            } = action
            else {
                return false;
            };
            *session == snapshot.session
                && snapshot.outputs.iter().any(|output| {
                    output.id == *id
                        && output.identity == *identity
                        && (!matches!(
                            action.as_ref(),
                            Action::Model(UserAction::SetDockGeometry { .. })
                        ) || output.snapshot.wm_sequence.unwrap_or_default() == *generation)
                })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn level(percent: i32) -> ActionMessage {
        (
            0,
            7,
            Action::Bound {
                session: 19,
                generation: 27,
                action: Box::new(Action::Level {
                    audio: true,
                    percent,
                }),
            },
        )
    }
    #[test]
    fn high_rate_motion_keeps_final_target_without_filling_click_queue() {
        let mut queue = LatestActions::default();
        for n in 0..10_000 {
            assert!(queue.push(level(n)));
        }
        assert_eq!(queue.entries.len(), 1);
        assert_eq!(queue.pop(), Some(level(9_999)));
        assert!(queue.pop().is_none());
    }
    #[test]
    fn unrelated_minimize_preserves_slider_but_retires_old_dock_target() {
        let mut view = xbar_core::BarRuntime::default().snapshot();
        view.wm_session_id = 19;
        view.wm_sequence = Some(28);
        let snapshot = ShellSnapshot {
            protocol_version: xbar_core::desktop_shell::SHELL_PROTOCOL_VERSION,
            session: 19,
            revision: 1,
            config: Default::default(),
            source: xbar_core::desktop_shell::DesktopShellSource::Default,
            outputs: vec![xbar_core::desktop_shell::ShellOutput {
                id: 0,
                identity: 7,
                connector: None,
                geometry: xbar_core::MonitorGeometry {
                    x: 0,
                    y: 0,
                    width: 1280,
                    height: 800,
                },
                scale: 1.0,
                visible: true,
                snapshot: view,
                tasks: vec![],
            }],
        };
        let mut queue = LatestActions::default();
        assert!(queue.push(level(42)));
        assert!(queue.push((
            0,
            7,
            Action::Bound {
                session: 19,
                generation: 27,
                action: Box::new(Action::Model(UserAction::SetDockGeometry {
                    window: None,
                    wm_session_id: 19,
                    minimized_generation: 27,
                    geometry: Default::default()
                }))
            }
        )));
        queue.retain_current(&snapshot);
        assert_eq!(queue.pop(), Some(level(42)));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn transient_retry_does_not_replace_newer_target() {
        let mut queue = LatestActions::default();
        assert!(queue.push(level(80)));
        queue.retry(level(20));
        assert_eq!(queue.pop(), Some(level(80)));
    }
}
