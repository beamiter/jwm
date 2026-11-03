//! Ownership of a temporary theme change. A later configuration publication
//! retires the preview, even when it happens to choose the same theme again.
use std::sync::Arc;

pub(crate) struct ThemePreview {
    original: String,
    applied: Arc<crate::config::Config>,
}

impl std::fmt::Debug for ThemePreview {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ThemePreview")
            .field("original", &self.original)
            .field("applied_theme", &self.applied.ui_theme())
            .finish_non_exhaustive()
    }
}

impl ThemePreview {
    pub(crate) fn new(original: String, applied: Arc<crate::config::Config>) -> Self {
        Self { original, applied }
    }

    /// Keep the original configuration allocation alive to prevent pointer
    /// reuse from making a later publication look like our own revision.
    pub(crate) fn rollback_theme(&self, current: &Arc<crate::config::Config>) -> Option<&str> {
        Arc::ptr_eq(&self.applied, current).then_some(self.original.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_restores_only_the_revision_owned_by_the_preview() {
        let applied = Arc::new(crate::config::Config::default());
        let preview = ThemePreview::new("paper".into(), Arc::clone(&applied));
        assert_eq!(preview.rollback_theme(&applied), Some("paper"));
        // Another writer can choose the same values. Its publication is still
        // newer and Escape must not undo it.
        let newer = Arc::new((*applied).clone());
        assert_eq!(preview.rollback_theme(&newer), None);
    }

    #[test]
    fn successive_previews_keep_the_first_origin() {
        let first = Arc::new(crate::config::Config::default());
        let first_preview = ThemePreview::new("nord".into(), Arc::clone(&first));
        let next = Arc::new((*first).clone());
        let second_preview = ThemePreview::new(
            first_preview.rollback_theme(&first).unwrap().to_owned(),
            Arc::clone(&next),
        );
        assert_eq!(second_preview.rollback_theme(&next), Some("nord"));
        assert_eq!(first_preview.rollback_theme(&next), None);
    }
}
