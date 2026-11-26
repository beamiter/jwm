//! Independent tab records; one shared view reloads the newly active tab.
use crate::navigation::Navigation;
use std::path::{Path, PathBuf};

const LIMIT: usize = 16;

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
    active: usize,
    next_id: u64,
}

impl Tabs {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            records: vec![(TabId(1), Tab::new(path))],
            active: 0,
            next_id: 2,
        }
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
        if self.records.len() == LIMIT {
            return false;
        }
        let id = TabId(self.next_id);
        let Some(next) = self.next_id.checked_add(1) else {
            return false;
        };
        self.next_id = next;
        let cloned = Tab::new(self.active().path.clone());
        self.active += 1;
        self.records.insert(self.active, (id, cloned));
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
        self.records.remove(self.active);
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
