//! Backend-independent compositor vsync mode selection.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VsyncMethod {
    /// Traditional global swap interval.
    Global,
    /// GLX_OML_sync_control style per-window MSC timing.
    OmlSyncControl,
    /// X11 Present-style independent presentation.
    Present,
}

impl Default for VsyncMethod {
    fn default() -> Self {
        VsyncMethod::Global
    }
}

#[cfg(test)]
mod tests {
    use super::VsyncMethod;

    #[test]
    fn default_is_global() {
        assert_eq!(VsyncMethod::default(), VsyncMethod::Global);
    }
}
