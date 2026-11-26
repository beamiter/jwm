//! Independent tab records; one shared view reloads the newly active tab.
use crate::{navigation::Navigation, saved_session::SavedTabs};
use std::path::{Path, PathBuf};

const LIMIT: usize = crate::saved_session::MAX_TABS;
const CLOSED_LIMIT: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TabId(u64);

impl TabId {
    pub(super) fn action_parameter(self) -> u64 {
        self.0
    }

    pub(super) fn from_action_parameter(value: u64) -> Self {
        Self(value)
    }
}

pub(super) struct Tab {
    pub(super) path: PathBuf,
    pub(super) navigation: Navigation,
}

impl Tab {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            navigation: Navigation::default(),
        }
    }
}

pub(super) struct Tabs {
    records: Vec<(TabId, Tab)>,
    closed: Vec<(usize, Tab)>,
    active: usize,
    next_id: u64,
}

impl Tabs {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            records: vec![(TabId(1), Tab::new(path))],
            closed: Vec::new(),
            active: 0,
            next_id: 2,
        }
    }

    pub(super) fn snapshot(&self) -> SavedTabs {
        SavedTabs {
            paths: self.iter().map(|(_, path)| path.to_path_buf()).collect(),
            active: self.active,
        }
    }

    pub(super) fn from_saved(saved: SavedTabs) -> Option<Self> {
        if saved.paths.is_empty() || saved.paths.len() > LIMIT || saved.active >= saved.paths.len() {
            return None;
        }
        let records: Vec<_> = saved
            .paths
            .into_iter()
            .enumerate()
            .map(|(index, path)| (TabId(index as u64 + 1), Tab::new(path)))
            .collect();
        Some(Self {
            next_id: records.len() as u64 + 1,
            records,
            active: saved.active,
            closed: Vec::new(),
        })
    }

    pub(super) fn active_id(&self) -> TabId {
        self.records[self.active].0
    }

    pub(super) fn active(&self) -> &Tab {
        &self.records[self.active].1
    }

    pub(super) fn active_mut(&mut self) -> &mut Tab {
        &mut self.records[self.active].1
    }

    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = (TabId, &Path)> {
        self.records
            .iter()
            .map(|(id, tab)| (*id, tab.path.as_path()))
    }

    pub(super) fn new_next(&mut self) -> bool {
        let Some(id) = self.claim_id() else {
            return false;
        };
        let cloned = Tab::new(self.active().path.clone());
        self.active += 1;
        self.records.insert(self.active, (id, cloned));
        true
    }

    pub(super) fn is_full(&self) -> bool {
        self.records.len() >= LIMIT
    }

    fn claim_id(&mut self) -> Option<TabId> {
        if self.is_full() {
            return None;
        }
        let id = TabId(self.next_id);
        self.next_id = self.next_id.checked_add(1)?;
        Some(id)
    }

    pub(super) fn reopen_latest(&mut self) -> bool {
        if self.closed.is_empty() {
            return false;
        }
        let Some(id) = self.claim_id() else {
            return false;
        };
        let Some((position, tab)) = self.closed.pop() else {
            return false;
        };
        self.active = position.min(self.records.len());
        // A closed tab's button or queued action must never target this tab.
        self.records.insert(self.active, (id, tab));
        true
    }

    pub(super) fn activate(&mut self, id: TabId) -> bool {
        let Some(index) = self.records.iter().position(|page| page.0 == id) else {
            return false;
        };
        if self.active == index {
            return false;
        }
        self.active = index;
        true
    }

    pub(super) fn cycle(&mut self, backwards: bool) -> bool {
        let count = self.records.len();
        if count <= 1 {
            return false;
        }
        self.active = if backwards {
            self.active.checked_sub(1).unwrap_or(count - 1)
        } else {
            (self.active + 1) % count
        };
        true
    }

    pub(super) fn close_active(&mut self) -> bool {
        if self.records.len() <= 1 {
            return false;
        }
        let (_, tab) = self.records.remove(self.active);
        if self.closed.len() == CLOSED_LIMIT {
            self.closed.remove(0);
        }
        self.closed.push((self.active, tab));
        self.active = self.active.min(self.records.len() - 1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigation::Destination;

    fn navigate(tabs: &mut Tabs, path: &str) {
        let active = tabs.active_mut();
        assert!(active.navigation.accept(Destination::Visit(path.into())));
        active.path = path.into();
    }

    #[test]
    fn session_restores_order_but_not_navigation_or_closed_tabs() {
        let mut tabs = Tabs::new("/one".into());
        navigate(&mut tabs, "/one");
        navigate(&mut tabs, "/two");
        assert!(tabs.new_next());
        navigate(&mut tabs, "/other");
        assert!(tabs.new_next());
        assert!(tabs.close_active());
        let saved = tabs.snapshot();
        assert_eq!(saved.active, 1);
        let mut restored = Tabs::from_saved(saved).unwrap();
        assert_eq!(restored.active().path, Path::new("/other"));
        assert!(restored.active().navigation.back().is_none());
        assert!(!restored.reopen_latest());
        assert!(restored.cycle(true));
        assert_eq!(restored.active().path, Path::new("/two"));
        assert!(restored.active().navigation.back().is_none());
    }

    #[test]
    fn tabs_keep_separate_paths_and_history() {
        let mut tabs = Tabs::new("first".into());
        let first = tabs.active_id();
        navigate(&mut tabs, "first");
        navigate(&mut tabs, "shared");
        assert!(tabs.new_next());
        let second = tabs.active_id();
        assert_eq!(tabs.active().path, Path::new("shared"));
        assert!(tabs.active().navigation.back().is_none());
        navigate(&mut tabs, "shared");
        navigate(&mut tabs, "second-only");
        assert!(tabs.activate(first));
        assert_eq!(tabs.active().path, Path::new("shared"));
        assert_eq!(
            tabs.active().navigation.back().unwrap().path(),
            Path::new("first")
        );
        assert!(tabs.cycle(true));
        assert_eq!(tabs.active_id(), second);
        assert_eq!(tabs.active().path, Path::new("second-only"));
        assert_ne!(first, second);
    }

    #[test]
    fn reopen_keeps_both_history_directions_but_not_a_stale_id() {
        let mut tabs = Tabs::new("root".into());
        assert!(!tabs.reopen_latest());
        let first = tabs.active_id();
        assert!(tabs.new_next());
        let old = tabs.active_id();
        for path in ["root", "middle", "end"] {
            navigate(&mut tabs, path);
        }
        let tab = tabs.active_mut();
        let back = tab.navigation.back().unwrap();
        tab.path = back.path().into();
        assert!(tab.navigation.accept(back));
        assert!(tabs.close_active());
        assert_eq!(tabs.active_id(), first);
        assert!(tabs.reopen_latest());
        assert_eq!(tabs.active().path, Path::new("middle"));
        assert_eq!(
            tabs.active().navigation.back().unwrap().path(),
            Path::new("root")
        );
        assert_eq!(
            tabs.active().navigation.forward().unwrap().path(),
            Path::new("end")
        );
        assert_ne!(tabs.active_id(), old);
        assert!(!tabs.activate(old));
        assert!(!tabs.reopen_latest());
    }

    #[test]
    fn reopen_is_window_local_lifo_bounded_and_preserves_history_when_full() {
        let mut tabs = Tabs::new("root".into());
        for i in 0..CLOSED_LIMIT + 2 {
            assert!(tabs.new_next());
            navigate(&mut tabs, &i.to_string());
            assert!(tabs.close_active());
        }
        assert!(!Tabs::new("other".into()).reopen_latest());
        let first = tabs.active_id();
        for _ in 1..LIMIT {
            assert!(tabs.new_next());
        }
        assert!(!tabs.reopen_latest());
        assert_eq!(tabs.closed.len(), CLOSED_LIMIT);

        // Free room without creating new close records for the capacity test.
        tabs.records.truncate(1);
        assert!(tabs.activate(first));
        for expected in (3..CLOSED_LIMIT + 2).rev() {
            assert!(tabs.reopen_latest());
            assert_eq!(tabs.active().path, Path::new(&expected.to_string()));
        }
        assert!(tabs.is_full());
        assert!(!tabs.reopen_latest());
        assert_eq!(tabs.closed[0].1.path, Path::new("2"));
    }

    #[test]
    fn closing_never_reuses_an_id_or_removes_the_last_tab() {
        let mut tabs = Tabs::new("root".into());
        let first = tabs.active_id();
        assert_eq!(
            first,
            TabId::from_action_parameter(first.action_parameter())
        );
        assert!(!tabs.close_active());
        assert!(tabs.new_next());
        let stale = tabs.active_id();
        assert!(tabs.close_active());
        assert_eq!(tabs.active_id(), first);
        assert!(tabs.new_next());
        assert_ne!(tabs.active_id(), stale);
        assert!(!tabs.activate(stale));
        let count = tabs.iter().len();
        for _ in count..LIMIT {
            assert!(tabs.new_next());
        }
        assert_eq!(tabs.iter().len(), LIMIT);
        assert!(!tabs.new_next());
    }
}
