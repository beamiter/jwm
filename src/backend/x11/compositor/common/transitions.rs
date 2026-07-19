#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TransitionMode {
    None,
    Slide,
    Cube,
    Fade,
    Flip,
    Zoom,
    Stack,
    Blinds,
    CoverFlow,
    Helix,
    Portal,
}

impl TransitionMode {
    pub fn from_name(mode: &str) -> Self {
        match mode {
            "cube" => Self::Cube,
            "fade" => Self::Fade,
            "flip" => Self::Flip,
            "zoom" => Self::Zoom,
            "stack" => Self::Stack,
            "blinds" => Self::Blinds,
            "coverflow" => Self::CoverFlow,
            "helix" => Self::Helix,
            "portal" => Self::Portal,
            _ => Self::Slide,
        }
    }

    pub fn from_name_or_none(mode: &str) -> Self {
        match mode {
            "none" => Self::None,
            "" => Self::None,
            _ => Self::from_name(mode),
        }
    }

    pub fn needs_new_scene_fbo(self) -> bool {
        matches!(
            self,
            Self::Cube | Self::Flip | Self::Blinds | Self::CoverFlow | Self::Helix | Self::Portal
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_mode_from_name_defaults_to_slide() {
        assert!(matches!(
            TransitionMode::from_name("cube"),
            TransitionMode::Cube
        ));
        assert!(matches!(
            TransitionMode::from_name("portal"),
            TransitionMode::Portal
        ));
        assert!(matches!(
            TransitionMode::from_name("unknown"),
            TransitionMode::Slide
        ));
        assert!(matches!(
            TransitionMode::from_name_or_none("unknown"),
            TransitionMode::Slide
        ));
        assert!(matches!(
            TransitionMode::from_name_or_none("none"),
            TransitionMode::None
        ));
    }

    #[test]
    fn explicit_none_disables_transitions() {
        assert_eq!(
            TransitionMode::from_name_or_none("none"),
            TransitionMode::None
        );
        assert_eq!(TransitionMode::from_name_or_none(""), TransitionMode::None);
    }

    #[test]
    fn transition_mode_new_scene_fbo_requirement() {
        assert!(TransitionMode::Cube.needs_new_scene_fbo());
        assert!(TransitionMode::Portal.needs_new_scene_fbo());
        assert!(!TransitionMode::Slide.needs_new_scene_fbo());
        assert!(!TransitionMode::None.needs_new_scene_fbo());
        assert!(!TransitionMode::Fade.needs_new_scene_fbo());
        assert!(!TransitionMode::Stack.needs_new_scene_fbo());
    }
}
