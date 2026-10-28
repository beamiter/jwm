//! The official desktop header has three deliberate zones. It reuses semantic
//! controls and hit targets, while the generic layouts remain untouched.
use super::*;

impl<M: TextMeasurer> LayoutPass<'_, M> {
    pub(super) fn build_desktop_header(
        &self,
        view: BarView<'_>,
        viewport: Size,
        interaction: &InteractionState,
    ) -> Scene {
        let projected = PresentationProjector::project(view, self.config);
        let viewport = viewport.normalized();
        let axis = BarAxis::new(self.config.edge, viewport, self.config.bar_height);
        let bar = axis.bounds;
        let palette = Palette::for_theme(projected.theme);
        let mut scene = Scene {
            viewport,
            clip: Rect::from_size(viewport),
            nodes: Vec::new(),
            hits: Vec::new(),
        };
        if bar.is_empty() {
            return scene;
        }
        scene.nodes.push(SceneNode::Background {
            id: NodeId::Background,
            bounds: bar,
            fill: palette.background,
        });
        let padding = finite_non_negative(self.config.horizontal_padding)
            .min((bar.width - 32.0).max(0.0) * 0.5);
        let gap = finite_non_negative(self.config.item_gap).min(12.0);
        let cross_pad = finite_non_negative(self.config.vertical_padding)
            .min((bar.height - 18.0).max(0.0) * 0.5);
        let y = bar.y + cross_pad;
        let height = (bar.height - 2.0 * cross_pad).max(1.0);
        let start = bar.x + padding;
        let end = bar.right() - padding;
        let available = (end - start).max(0.0);
        if available < 32.0 {
            return scene;
        }

        // Status values stay readable: yield low-priority controls before
        // collapsing every metric into a row of identical icon bubbles.
        let status_budget = available * 0.49;
        let mut status_used = 0.0;
        let mut status_cells = Vec::new();
        for id in [
            NodeId::Clock,
            NodeId::Battery,
            NodeId::Audio,
            NodeId::Cpu,
            NodeId::Memory,
            NodeId::Network,
        ] {
            let Some(control) = projected
                .status
                .iter()
                .find(|control| control.id == id)
                .cloned()
            else {
                continue;
            };
            let compact = id == NodeId::Clock && available < 1400.0;
            let width = if id == NodeId::Network {
                190.0
            } else {
                self.control_extent(&control, compact)
            };
            let width = width.max(42.0);
            let consumed = width + if status_cells.is_empty() { 0.0 } else { gap };
            if status_used + consumed > status_budget {
                continue;
            }
            status_used += consumed;
            status_cells.push((control, width, compact));
        }
        let mut trailing = end;
        let mut flat_palette = palette;
        flat_palette.pill = Rgba::new(1.0, 1.0, 1.0, 0.0);
        status_cells.sort_by_key(|(control, _, _)| match control.id {
            NodeId::Clock => 0,
            NodeId::Battery => 1,
            NodeId::Audio => 2,
            NodeId::Network => 3,
            NodeId::Memory => 4,
            _ => 5,
        });
        for (control, width, compact) in status_cells {
            trailing -= width;
            let mut spec = self.pill_spec(control, compact);
            // The generic network layout reserves enormous worst-case units.
            // This fixed slot uses the full semantic rate text and ellipsis.
            if spec.id == NodeId::Network {
                spec.network = None;
            }
            self.push_pill(
                &mut scene,
                Rect::new(trailing, y, width, height),
                spec,
                interaction,
                flat_palette,
                Some(bar),
            );
            trailing -= gap;
        }

        let mut leading = start;
        if let Some(launcher) = projected
            .status
            .iter()
            .find(|control| matches!(control.id, NodeId::ShellHub(_)))
            .cloned()
            .filter(|_| available >= 480.0)
        {
            let width = height.max(32.0).min(40.0);
            if width + gap <= available - status_used {
                let mut spec = self.pill_spec(launcher, true);
                spec.text = "▦".to_owned();
                spec.lines.clear();
                self.push_pill(
                    &mut scene,
                    Rect::new(leading, y, width, height),
                    spec,
                    interaction,
                    palette,
                    Some(bar),
                );
                leading += width + gap;
            }
        }
        let tag_palette = TagPalette::for_wallpaper(
            projected.theme,
            view.wallpaper_luminance,
            self.config.tag_contrast,
        );
        let tag_budget = (available * 0.40).min((trailing - leading - 80.0).max(32.0));
        let preferred = 88.0_f32.max(height * 2.0).min(100.0);
        let overflow_width = height.max(32.0).min(40.0);
        let max_tags = ((tag_budget - overflow_width - gap) / (preferred + gap))
            .floor()
            .clamp(1.0, 4.0) as usize;
        let mut visible: Vec<_> = (0..projected.tags.len().min(max_tags)).collect();
        let active = view.active_tag.and_then(|id| {
            projected
                .tags
                .iter()
                .position(|control| control.id == NodeId::Tag(id))
        });
        if let Some(active) = active
            && !visible.contains(&active)
        {
            if let Some(last) = visible.last_mut() {
                *last = active;
            }
            visible.sort_unstable();
        }
        let full_visible = visible.len();
        for &index in &visible {
            let control = projected.tags[index].clone();
            let width = preferred.min((trailing - leading).max(0.0));
            if width < 32.0 {
                break;
            }
            let spec = PillSpec {
                tag_palette: Some(tag_palette),
                ..self.pill_spec(control, false)
            };
            self.push_pill(
                &mut scene,
                Rect::new(leading, y, width, height),
                spec,
                interaction,
                palette,
                Some(bar),
            );
            leading += width + gap;
        }
        if full_visible < projected.tags.len() && trailing - leading >= overflow_width {
            let active = active.unwrap_or(0);
            let source = &projected.tags[active];
            let first_hidden = (1..=projected.tags.len())
                .map(|offset| (active + offset) % projected.tags.len())
                .find(|index| !visible.contains(index));
            let mut spec = self.pill_spec(source.clone(), true);
            spec.id = NodeId::WorkspaceOverflow;
            spec.text = "+".to_owned();
            spec.lines.clear();
            spec.state.selected = false;
            spec.state.urgent = projected
                .tags
                .iter()
                .enumerate()
                .any(|(index, control)| !visible.contains(&index) && control.state.urgent);
            spec.primary = first_hidden.and_then(|index| projected.tags[index].bindings.primary);
            spec.secondary = source.bindings.scroll_up;
            self.push_pill(
                &mut scene,
                Rect::new(leading, y, overflow_width, height),
                spec,
                interaction,
                palette,
                Some(bar),
            );
            leading += overflow_width + gap;
        }
        if trailing - leading >= 80.0 {
            let title_bounds = Rect::new(
                leading + gap,
                y,
                (trailing - leading - gap * 2.0).max(0.0),
                height,
            );
            let mut title_palette = palette;
            title_palette.muted_text = palette.text;
            self.push_client_title(
                &mut scene,
                title_bounds,
                projected
                    .client_name
                    .as_ref()
                    .map_or("JWM Desktop", |title| title.value.as_ref()),
                projected
                    .client_name
                    .as_ref()
                    .and(projected.client_icon.as_ref()),
                title_palette,
            );
        }
        scene
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Metrics;
    impl TextMeasurer for Metrics {
        fn measure(&self, text: &str, size: f32) -> Size {
            // Symbol glyphs can occupy a full em even when Latin text is narrow.
            Size::new(
                if text == "▦" {
                    size * 1.2
                } else {
                    text.chars().count() as f32 * size * 0.55
                },
                size,
            )
        }
    }
    #[test]
    fn small_header_keeps_the_launcher_glyph_inside_its_pill() {
        let model = crate::model::BarModel::default();
        let config = PresentationConfig {
            desktop_header: true,
            font_size: 12.0,
            pill_horizontal_padding: 12.0,
            vertical_padding: 7.0,
            ..Default::default()
        };
        let scene = build_scene(
            &config,
            &Metrics,
            model.view(),
            Size::new(1280.0, 42.0),
            &InteractionState::default(),
        );
        assert!(
            scene
                .nodes
                .iter()
                .any(|node| matches!(node,SceneNode::Text{text,..} if text=="▦"))
        );
    }
    #[test]
    fn header_has_four_workspace_pills_center_title_and_no_second_dock() {
        let mut model = crate::model::BarModel::default();
        let mut config = PresentationConfig {
            desktop_header: true,
            ..PresentationConfig::default()
        };
        config.labels.cpu = "CPU".into();
        config.labels.memory = "MEM".into();
        let update = crate::model::WmSnapshot {
            client_name: "Focused application".into(),
            tags: vec![crate::model::TagState::default(); 9],
            ..Default::default()
        };
        model
            .update(crate::model::BarEvent::WindowManager(update))
            .unwrap();
        let scene = build_scene(
            &config,
            &Metrics,
            model.view(),
            Size::new(1672.0, 50.0),
            &InteractionState::default(),
        );
        assert_eq!(
            scene
                .hits
                .iter()
                .filter(|hit| matches!(hit.id, NodeId::Tag(_)))
                .count(),
            4
        );
        assert!(
            scene
                .hits
                .iter()
                .any(|hit| hit.id == NodeId::WorkspaceOverflow && hit.primary.is_some())
        );
        assert!(scene.nodes.iter().any(|node| node.id() == NodeId::Client));
        assert!(!scene.nodes.iter().any(|node| matches!(
            node.id(),
            NodeId::DockShelf | NodeId::MinimizedWindow(_) | NodeId::LayoutButton | NodeId::Media
        )));
        for (i, a) in scene.hits.iter().enumerate() {
            for b in scene.hits.iter().skip(i + 1) {
                assert!(a.bounds.intersection(b.bounds).is_none());
            }
        }
    }
    #[test]
    fn active_workspace_survives_narrow_header_and_hidden_tags_stay_reachable() {
        let mut model = crate::model::BarModel::default();
        let active = TagId::new(8).unwrap();
        let mut tags = vec![crate::model::TagState::default(); 9];
        tags[8].selected = true;
        model
            .update(crate::model::BarEvent::WindowManager(
                crate::model::WmSnapshot {
                    tags,
                    ..Default::default()
                },
            ))
            .unwrap();
        let config = PresentationConfig {
            desktop_header: true,
            ..PresentationConfig::default()
        };
        let scene = build_scene(
            &config,
            &Metrics,
            model.view(),
            Size::new(800.0, 50.0),
            &InteractionState::default(),
        );
        assert!(scene.hits.iter().any(|hit| hit.id == NodeId::Tag(active)));
        assert!(
            scene
                .hits
                .iter()
                .any(|hit| hit.id == NodeId::WorkspaceOverflow && hit.scroll_down.is_some())
        );
        for hit in scene.hits {
            assert!(hit.bounds.x >= 0.0 && hit.bounds.right() <= 800.0);
        }
    }
    #[test]
    fn overflow_can_visit_every_hidden_workspace_instead_of_toggling_two() {
        let mut model = crate::model::BarModel::default();
        let config = PresentationConfig {
            desktop_header: true,
            ..Default::default()
        };
        let mut active = 0;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..12 {
            let mut tags = vec![crate::model::TagState::default(); 9];
            tags[active].selected = true;
            model
                .update(crate::model::BarEvent::WindowManager(
                    crate::model::WmSnapshot {
                        tags,
                        ..Default::default()
                    },
                ))
                .unwrap();
            let scene = build_scene(
                &config,
                &Metrics,
                model.view(),
                Size::new(1672.0, 50.0),
                &InteractionState::default(),
            );
            let action = scene
                .hits
                .iter()
                .find(|hit| hit.id == NodeId::WorkspaceOverflow)
                .unwrap()
                .primary
                .unwrap();
            let tag = match action {
                UserAction::ViewTag(tag) | UserAction::ViewTagOn { tag, .. } => tag,
                _ => panic!("overflow must navigate workspaces"),
            };
            active = tag.index();
            seen.insert(active);
        }
        assert!((4..9).all(|index| seen.contains(&index)));
    }
}
