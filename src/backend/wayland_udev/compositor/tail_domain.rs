//! Frame-tail overlay color-domain table (HDR gap-queue P0-4).
//!
//! Everything the compositor draws after the window scene is classified here
//! exactly once. Each overlay class is either **common-linear-aware** — it is
//! drawn into the shared linear-sRGB target ahead of the per-output matrix +
//! OETF, so the frame-tail transform applies to it exactly once — or it keeps
//! a **named blocker** (a subdivision of the historical aggregate
//! `compositor_encoded_tail`), which converges the frame to the exact-sRGB
//! fallback where its encoded-space drawing is correct by construction.
//!
//! The same table drives every consumer, so the gate, the draw domain and the
//! diagnostic vocabulary cannot drift apart:
//!
//! - `WaylandCompositor::linear_tail_status` derives the frame's linear-tail
//!   safety and the typed blocker set from the visibility snapshot.
//! - `render_frame` draws the linear-aware classes into the bound linear
//!   target on deferred routes (the fallback/legacy routes keep the historical
//!   encoded draw, so the SDR picture is unchanged there).
//! - `KmsState::record_color_delivery_attempt` turns the typed set into the
//!   stable wire names published over IPC.
//!
//! Genie, close-fades, borders and the minimized-Dock passes are not listed:
//! they are scene passes drawn through the shared window shader with
//! `u_scene_linear` wired to the hardware-OETF state, i.e. they belong to the
//! window scene rather than the frame tail.
//!
//! Capture/readback is deliberately absent from this table as well: screenshots
//! and recording derive from an explicitly encoded independent view (the
//! capture view, see `render_frame` section 18c), so their presence never
//! constrains the physical scanout route.

use super::WaylandCompositor;

/// Where a frame-tail overlay is drawn relative to the output delivery point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TailOverlayStage {
    /// Drawn into whichever target the frame route leaves bound after the
    /// window scene: the common linear target on deferred routes, the encoded
    /// output target on legacy/fallback routes. Migrating such a class to
    /// common-linear-aware only requires its shaders to honor the bound
    /// target's domain.
    LinearTarget,
    /// Drawn into the encoded output target after the delivery point (debug
    /// HUD, annotation, screenshot toolbar, toasts, OSD, system UI, recording
    /// crop outline). Migrating such a class additionally requires moving its
    /// draw ahead of the delivery point, so these stay encoded-only for now.
    PostDelivery,
}

/// Whether the class can consume the common linear-sRGB working domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TailOverlayDomain {
    /// Drawn before the frame-tail output transform; the unified per-output
    /// matrix + OETF applies once. Never contributes a linear-tail blocker.
    CommonLinearAware,
    /// Written in the encoded sRGB domain. While visible, the frame takes the
    /// exact-sRGB fallback and the class is reported under its own stable
    /// blocker name.
    EncodedOnly,
}

/// Every compositor-owned frame-tail overlay class, in draw order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TailOverlayClass {
    /// Workspace transition (cube/coverflow/...) sampling two encoded
    /// workspace snapshots.
    WorkspaceTransition,
    /// Snap preview highlight rectangle (border program, domain-aware).
    SnapPreview,
    /// 3D overview: skydome/prism/title/strip bind their output domain
    /// explicitly, including the software-reentry path on fallback frames.
    Overview,
    /// Expose grid: dim scrim (domain-aware), black-only shadows (domain
    /// neutral), window thumbnails and hover ring (shared shaders).
    Expose,
    /// Peek spotlight: dim scrim plus the focused window redrawn on top.
    Peek,
    /// Tab bar: frosted variants sample the encoded backdrop.
    TabBar,
    /// Particle effects (dedicated shader without a linear-domain ingress).
    Particles,
    /// Edge glow (dedicated time-varying shader without linear ingress).
    EdgeGlow,
    /// Full-frame postprocess filter chain operating on encoded pixels.
    Postprocess,
    /// Debug HUD card.
    DebugHud,
    /// Screenshot annotations (shapes, strokes, labels).
    Annotation,
    /// Screenshot toolbar floating above the annotation surface.
    ScreenshotToolbar,
    /// Toast notification cards.
    Toast,
    /// Volume/brightness OSD.
    Osd,
    /// Modal system UI (launcher, lock shield, prompts, ...).
    SystemUi,
    /// Recording crop outline, deliberately kept out of the encoded stream.
    RecordingRegionOverlay,
}

impl TailOverlayClass {
    /// Stable report order: the visual draw order, linear-target passes first.
    pub(crate) const ALL: [Self; 16] = [
        Self::WorkspaceTransition,
        Self::SnapPreview,
        Self::Overview,
        Self::Expose,
        Self::Peek,
        Self::TabBar,
        Self::Particles,
        Self::EdgeGlow,
        Self::Postprocess,
        Self::DebugHud,
        Self::Annotation,
        Self::ScreenshotToolbar,
        Self::Toast,
        Self::Osd,
        Self::SystemUi,
        Self::RecordingRegionOverlay,
    ];

    pub(crate) const fn domain(self) -> TailOverlayDomain {
        match self {
            Self::SnapPreview | Self::Overview | Self::Expose | Self::Peek => {
                TailOverlayDomain::CommonLinearAware
            }
            Self::WorkspaceTransition
            | Self::TabBar
            | Self::Particles
            | Self::EdgeGlow
            | Self::Postprocess
            | Self::DebugHud
            | Self::Annotation
            | Self::ScreenshotToolbar
            | Self::Toast
            | Self::Osd
            | Self::SystemUi
            | Self::RecordingRegionOverlay => TailOverlayDomain::EncodedOnly,
        }
    }

    /// The stable linear-tail blocker wire name while the class is
    /// encoded-only. Common-linear-aware classes never block and therefore
    /// have no name; every emitted name must be listed in
    /// `api::LINEAR_TAIL_BLOCKER_NAMES` (a unit test enforces it).
    pub(crate) const fn blocker_wire_name(self) -> Option<&'static str> {
        match self {
            Self::SnapPreview | Self::Overview | Self::Expose | Self::Peek => None,
            Self::WorkspaceTransition => Some("workspace_transition_overlay"),
            Self::TabBar => Some("tab_bar_overlay"),
            Self::Particles => Some("particle_overlay"),
            Self::EdgeGlow => Some("edge_glow_overlay"),
            Self::Postprocess => Some("postprocess_filter"),
            Self::DebugHud => Some("debug_hud_overlay"),
            Self::Annotation => Some("annotation_overlay"),
            Self::ScreenshotToolbar => Some("screenshot_toolbar_overlay"),
            Self::Toast => Some("toast_overlay"),
            Self::Osd => Some("osd_overlay"),
            Self::SystemUi => Some("system_ui_overlay"),
            Self::RecordingRegionOverlay => Some("recording_region_overlay"),
        }
    }

    pub(crate) const fn stage(self) -> TailOverlayStage {
        match self {
            Self::WorkspaceTransition
            | Self::SnapPreview
            | Self::Overview
            | Self::Expose
            | Self::Peek
            | Self::TabBar
            | Self::Particles
            | Self::EdgeGlow
            | Self::Postprocess => TailOverlayStage::LinearTarget,
            Self::DebugHud
            | Self::Annotation
            | Self::ScreenshotToolbar
            | Self::Toast
            | Self::Osd
            | Self::SystemUi
            | Self::RecordingRegionOverlay => TailOverlayStage::PostDelivery,
        }
    }

    const fn bit(self) -> u32 {
        1u32 << (self as u32)
    }

    /// Whether an instance of this class can produce pixels this frame.
    /// Mirrored from the render-site predicates; both read the same snapshot
    /// fields so the gate cannot disagree with the draw loop.
    fn visible(self, visibility: &TailOverlayVisibility) -> bool {
        match self {
            Self::WorkspaceTransition => visibility.workspace_transition,
            Self::SnapPreview => visibility.snap_preview,
            Self::Overview => visibility.overview,
            Self::Expose => visibility.expose,
            Self::Peek => visibility.peek,
            Self::TabBar => visibility.tab_bar,
            Self::Particles => visibility.particles,
            Self::EdgeGlow => visibility.edge_glow,
            Self::Postprocess => visibility.postprocess,
            Self::DebugHud => visibility.debug_hud,
            Self::Annotation => visibility.annotation,
            Self::ScreenshotToolbar => visibility.screenshot_toolbar,
            Self::Toast => visibility.toast,
            Self::Osd => visibility.osd,
            Self::SystemUi => visibility.system_ui,
            Self::RecordingRegionOverlay => visibility.recording_region_overlay,
        }
    }
}

/// Per-frame visibility snapshot of every tail overlay class. Kept as plain
/// data so the classification matrix is exhaustively testable without a GL
/// context.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TailOverlayVisibility {
    pub workspace_transition: bool,
    pub snap_preview: bool,
    pub overview: bool,
    pub expose: bool,
    pub peek: bool,
    pub tab_bar: bool,
    pub particles: bool,
    pub edge_glow: bool,
    pub postprocess: bool,
    pub debug_hud: bool,
    pub annotation: bool,
    pub screenshot_toolbar: bool,
    pub toast: bool,
    pub osd: bool,
    pub system_ui: bool,
    pub recording_region_overlay: bool,
}

/// Typed set of visible encoded-only tail overlay classes. Membership is only
/// built by `tail_overlay_blockers`, so a contained class always carries a
/// blocker wire name.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(crate) struct TailOverlayBlockers {
    bits: u32,
}

impl TailOverlayBlockers {
    fn insert(&mut self, class: TailOverlayClass) {
        self.bits |= class.bit();
    }

    pub(crate) fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// Iterate the contained classes in stable `TailOverlayClass::ALL` order.
    pub(crate) fn iter(self) -> impl Iterator<Item = TailOverlayClass> {
        TailOverlayClass::ALL
            .into_iter()
            .filter(move |class| self.bits & class.bit() != 0)
    }
}

impl std::fmt::Debug for TailOverlayBlockers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_set().entries(self.iter()).finish()
    }
}

/// The compositor's frame-tail color-domain verdict for one frame.
pub(crate) struct LinearTailStatus {
    /// The common linear-sRGB target is allocated and requested. Without it
    /// every route is the legacy encoded domain regardless of overlay state.
    pub linear_target_ready: bool,
    /// Visible encoded-only overlay classes, each carrying its own blocker
    /// wire name.
    pub overlay_blockers: TailOverlayBlockers,
}

impl LinearTailStatus {
    /// Whether the frame tail can ride the deferred common-linear delivery.
    pub(crate) fn linear_tail_safe(&self) -> bool {
        self.linear_target_ready && self.overlay_blockers.is_empty()
    }
}

/// Classify one frame's tail overlay visibility: every visible encoded-only
/// class contributes its named blocker, common-linear-aware classes never do.
pub(crate) fn tail_overlay_blockers(visibility: &TailOverlayVisibility) -> TailOverlayBlockers {
    let mut blockers = TailOverlayBlockers::default();
    for class in TailOverlayClass::ALL {
        if class.visible(visibility) && class.domain() == TailOverlayDomain::EncodedOnly {
            blockers.insert(class);
        }
    }
    blockers
}

impl WaylandCompositor {
    /// Bind the encoded output target for a post-delivery overlay draw. The
    /// domain table pins these classes as post-delivery encoded-only, so the
    /// bind site and the classification cannot drift apart.
    pub(crate) fn bind_post_delivery_overlay_target(
        &self,
        gl: &smithay::backend::renderer::gles::ffi::Gles2,
        class: TailOverlayClass,
    ) {
        debug_assert_eq!(class.stage(), TailOverlayStage::PostDelivery);
        debug_assert_eq!(class.domain(), TailOverlayDomain::EncodedOnly);
        unsafe {
            gl.BindFramebuffer(
                smithay::backend::renderer::gles::ffi::FRAMEBUFFER,
                self.output_fbo,
            );
        }
    }

    /// Snapshot every tail overlay class's visibility from live state. The
    /// gate predicates are the conservative superset of the render sites' draw
    /// conditions (a class may count as visible here while its draw skips a
    /// zero-opacity frame, never the other way around).
    pub(crate) fn tail_overlay_visibility(&self) -> TailOverlayVisibility {
        TailOverlayVisibility {
            workspace_transition: self.transition_active,
            snap_preview: self.snap_preview.is_some() || self.snap_preview_opacity > 0.0,
            overview: self.overview_active || self.overview_opacity > 0.0,
            expose: self.expose_active || !self.expose_entries.is_empty(),
            peek: self.peek_active || self.peek_opacity > 0.0,
            tab_bar: self.window_tabs_enabled && !self.window_groups.is_empty(),
            particles: !self.particle_systems.is_empty(),
            edge_glow: self.edge_glow_enabled
                && self.edge_glow_width > 0.0
                && self.edge_glow_active
                && !self.edge_glow_suppressed,
            postprocess: self.postprocess_active,
            debug_hud: self.debug_hud_enabled || self.debug_hud_extended,
            annotation: self.annotation_active,
            screenshot_toolbar: self.screenshot_toolbar.is_some(),
            toast: !self.toast_stack.is_empty(),
            osd: !self.osd_slot.is_empty(),
            system_ui: self.system_ui.is_some(),
            recording_region_overlay: self.recording_region_overlay.is_some(),
        }
    }

    /// The frame's tail verdict: a live linear target plus the table-driven
    /// blocker set.
    pub(crate) fn linear_tail_status(&self) -> LinearTailStatus {
        LinearTailStatus {
            linear_target_ready: self.scene_linear_requested && self.linear_fbo != 0,
            overlay_blockers: tail_overlay_blockers(&self.tail_overlay_visibility()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set exactly one class visible in an otherwise clear snapshot.
    fn visibility_with(class: TailOverlayClass) -> TailOverlayVisibility {
        let mut visibility = TailOverlayVisibility::default();
        match class {
            TailOverlayClass::WorkspaceTransition => visibility.workspace_transition = true,
            TailOverlayClass::SnapPreview => visibility.snap_preview = true,
            TailOverlayClass::Overview => visibility.overview = true,
            TailOverlayClass::Expose => visibility.expose = true,
            TailOverlayClass::Peek => visibility.peek = true,
            TailOverlayClass::TabBar => visibility.tab_bar = true,
            TailOverlayClass::Particles => visibility.particles = true,
            TailOverlayClass::EdgeGlow => visibility.edge_glow = true,
            TailOverlayClass::Postprocess => visibility.postprocess = true,
            TailOverlayClass::DebugHud => visibility.debug_hud = true,
            TailOverlayClass::Annotation => visibility.annotation = true,
            TailOverlayClass::ScreenshotToolbar => visibility.screenshot_toolbar = true,
            TailOverlayClass::Toast => visibility.toast = true,
            TailOverlayClass::Osd => visibility.osd = true,
            TailOverlayClass::SystemUi => visibility.system_ui = true,
            TailOverlayClass::RecordingRegionOverlay => {
                visibility.recording_region_overlay = true;
            }
        }
        visibility
    }

    #[test]
    fn every_encoded_only_blocker_name_is_stable_valid_and_recognized() {
        let mut names = Vec::new();
        for class in TailOverlayClass::ALL {
            match class.domain() {
                TailOverlayDomain::EncodedOnly => {
                    let name = class
                        .blocker_wire_name()
                        .expect("encoded-only classes must carry a named blocker");
                    assert!(
                        crate::backend::api::linear_tail_blocker_name_is_valid(name),
                        "{name} must satisfy the wire-name grammar"
                    );
                    assert!(
                        crate::backend::api::is_known_linear_tail_blocker_name(name),
                        "{name} must be listed in api::LINEAR_TAIL_BLOCKER_NAMES"
                    );
                    names.push(name);
                }
                TailOverlayDomain::CommonLinearAware => {
                    assert_eq!(
                        class.blocker_wire_name(),
                        None,
                        "common-linear-aware classes never emit a blocker name"
                    );
                }
            }
        }
        let mut dedup = names.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(
            names.len(),
            dedup.len(),
            "blocker names must be unique: {names:?}"
        );
    }

    #[test]
    fn common_linear_aware_classes_all_draw_into_the_linear_target_stage() {
        for class in TailOverlayClass::ALL {
            if class.domain() == TailOverlayDomain::CommonLinearAware {
                assert_eq!(
                    class.stage(),
                    TailOverlayStage::LinearTarget,
                    "{class:?}: a common-linear draw must happen ahead of the delivery point"
                );
            }
        }
    }

    #[test]
    fn classification_matrix_matches_domain_table_per_class() {
        assert!(tail_overlay_blockers(&TailOverlayVisibility::default()).is_empty());

        for class in TailOverlayClass::ALL {
            let blockers = tail_overlay_blockers(&visibility_with(class));
            match class.domain() {
                TailOverlayDomain::EncodedOnly => {
                    assert_eq!(
                        blockers.iter().count(),
                        1,
                        "{class:?} must block on its own"
                    );
                    assert_eq!(blockers.iter().next(), Some(class));
                }
                TailOverlayDomain::CommonLinearAware => {
                    assert!(
                        blockers.is_empty(),
                        "{class:?} is common-linear-aware and must not block"
                    );
                }
            }
        }
    }

    #[test]
    fn blocker_set_iterates_in_draw_order_and_reports_every_encoded_only_class() {
        let mut visibility = TailOverlayVisibility::default();
        visibility.workspace_transition = true;
        visibility.expose = true;
        visibility.peek = true;
        visibility.toast = true;
        visibility.system_ui = true;
        visibility.recording_region_overlay = true;

        let blockers = tail_overlay_blockers(&visibility);
        // Expose and Peek are common-linear-aware: visible but absent here.
        assert_eq!(
            blockers.iter().collect::<Vec<_>>(),
            [
                TailOverlayClass::WorkspaceTransition,
                TailOverlayClass::Toast,
                TailOverlayClass::SystemUi,
                TailOverlayClass::RecordingRegionOverlay,
            ]
        );

        let all_visible = TailOverlayVisibility {
            workspace_transition: true,
            snap_preview: true,
            overview: true,
            expose: true,
            peek: true,
            tab_bar: true,
            particles: true,
            edge_glow: true,
            postprocess: true,
            debug_hud: true,
            annotation: true,
            screenshot_toolbar: true,
            toast: true,
            osd: true,
            system_ui: true,
            recording_region_overlay: true,
        };
        let blockers = tail_overlay_blockers(&all_visible);
        let expected: Vec<TailOverlayClass> = TailOverlayClass::ALL
            .into_iter()
            .filter(|class| class.domain() == TailOverlayDomain::EncodedOnly)
            .collect();
        assert_eq!(blockers.iter().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn linear_tail_safety_requires_a_live_target_and_no_blockers() {
        let status = LinearTailStatus {
            linear_target_ready: false,
            overlay_blockers: TailOverlayBlockers::default(),
        };
        assert!(!status.linear_tail_safe());
        let status = LinearTailStatus {
            linear_target_ready: true,
            overlay_blockers: tail_overlay_blockers(&visibility_with(TailOverlayClass::Toast)),
        };
        assert!(!status.linear_tail_safe());
        let status = LinearTailStatus {
            linear_target_ready: true,
            overlay_blockers: tail_overlay_blockers(&visibility_with(TailOverlayClass::Expose)),
        };
        assert!(status.linear_tail_safe());
    }
}
