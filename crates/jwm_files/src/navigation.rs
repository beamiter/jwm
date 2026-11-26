use std::path::{Path, PathBuf};

const MAX_VISITS: usize = 128;

#[derive(Clone)]
pub(crate) enum Destination {
    Visit(PathBuf),
    History { index: usize, path: PathBuf },
}

impl Destination {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Visit(path) | Self::History { path, .. } => path,
        }
    }
}

#[derive(Default)]
pub(crate) struct Navigation {
    visits: Vec<PathBuf>,
    selected: Option<usize>,
}

impl Navigation {
    pub(crate) fn back(&self) -> Option<Destination> {
        self.at(self.selected?.checked_sub(1)?)
    }

    pub(crate) fn forward(&self) -> Option<Destination> {
        self.at(self.selected?.checked_add(1)?)
    }

    fn at(&self, index: usize) -> Option<Destination> {
        Some(Destination::History {
            index,
            path: self.visits.get(index)?.clone(),
        })
    }

    /// Failed or superseded attempts never call accept.
    pub(crate) fn accept(&mut self, destination: Destination) -> bool {
        match destination {
            Destination::History { index, path } => {
                if self.visits.get(index) != Some(&path) {
                    return false;
                }
                self.selected = Some(index);
            }
            Destination::Visit(path) => {
                if let Some(selected) = self.selected {
                    if self.visits[selected] == path {
                        return true;
                    }
                    self.visits.truncate(selected + 1);
                }
                if self.visits.len() == MAX_VISITS {
                    self.visits.remove(0);
                }
                self.visits.push(path);
                self.selected = Some(self.visits.len() - 1);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visit(history: &mut Navigation, path: &str) {
        assert!(history.accept(Destination::Visit(path.into())));
    }

    #[test]
    fn back_forward_only_changes_on_success_and_refresh_keeps_the_forward_branch() {
        let mut history = Navigation::default();
        visit(&mut history, "alpha");
        visit(&mut history, "beta");
        let back = history.back().unwrap();
        assert_eq!(back.path(), Path::new("alpha"));
        assert!(history.forward().is_none());
        assert!(history.accept(back));
        visit(&mut history, "alpha");
        assert_eq!(history.forward().unwrap().path(), Path::new("beta"));
        visit(&mut history, "gamma");
        assert!(history.forward().is_none());
        assert_eq!(history.back().unwrap().path(), Path::new("alpha"));
    }

    #[test]
    fn stale_history_target_does_not_move_the_cursor() {
        let mut history = Navigation::default();
        visit(&mut history, "alpha");
        visit(&mut history, "beta");
        assert!(history.accept(history.back().unwrap()));
        let stale = history.forward().unwrap();
        visit(&mut history, "gamma");
        assert!(!history.accept(stale));
        assert_eq!(history.back().unwrap().path(), Path::new("alpha"));
    }

    #[test]
    fn history_is_bounded_and_keeps_the_most_recent_visits() {
        let mut history = Navigation::default();
        for n in 0..MAX_VISITS + 5 {
            visit(&mut history, &n.to_string());
        }
        assert_eq!(history.visits.len(), MAX_VISITS);
        assert_eq!(history.visits[0], Path::new("5"));
        assert!(history.forward().is_none());
    }
}
