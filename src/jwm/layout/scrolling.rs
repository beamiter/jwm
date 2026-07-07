// Scrolling layout operations

use crate::backend::api::Backend;
use crate::core::layout::LayoutEnum;
use crate::core::models::{MonitorKey, ScrollingState};
use crate::jwm::Jwm;
use crate::jwm::types::WMArgEnum;

impl Jwm {
    /// Get the current monitor's scrolling state (if in scrolling layout)
    fn get_scrolling_state_for_sel_mon(&self) -> Option<(MonitorKey, &ScrollingState)> {
        let mon_key = self.state.sel_mon?;
        let monitor = self.state.monitors.get(mon_key)?;
        let layout = &monitor.lt[monitor.sel_lt];
        if **layout != LayoutEnum::SCROLLING {
            return None;
        }
        let state = self.scrolling_state_for_monitor(mon_key)?;
        Some((mon_key, state))
    }

    pub(crate) fn scrolling_default_column_width_for_client(
        &self,
        client_key: crate::core::models::ClientKey,
    ) -> Option<f32> {
        let client = self.state.clients.get(client_key)?;
        scrolling_column_width_rule_for_window(
            &client.name,
            &client.class,
            &client.instance,
            &crate::config::CONFIG
                .load()
                .behavior()
                .scrolling_column_width_rules,
        )
    }

    /// Focus the column to the left/right of the current one
    pub(crate) fn scrolling_focus_column(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let direction = match *arg {
            WMArgEnum::Int(i) => i,
            _ => return Ok(()),
        };

        let mon_key = match self.state.sel_mon {
            Some(k) => k,
            None => return Ok(()),
        };

        let sel = self.state.monitors.get(mon_key).and_then(|m| m.sel);
        let target = {
            let state = match self.scrolling_state_for_monitor_mut(mon_key) {
                Some(s) => s,
                None => return Ok(()),
            };
            if let Some(sel_key) = sel {
                state.remember_focus(sel_key);
            }

            let cur_col = sel
                .and_then(|k| state.columns.iter().position(|col| col.contains(&k)))
                .unwrap_or(0);

            let n_cols = state.columns.len();
            if n_cols == 0 {
                return Ok(());
            }

            let new_col = if direction > 0 {
                (cur_col + 1).min(n_cols - 1)
            } else {
                cur_col.saturating_sub(1)
            };

            if new_col == cur_col {
                None
            } else {
                state.target_for_column(new_col)
            }
        };

        if let Some(target) = target {
            if let Some(state) = self.scrolling_state_for_monitor_mut(mon_key) {
                state.remember_focus(target);
            }
            self.focus(backend, Some(target))?;
            self.arrange(backend, Some(mon_key));
        }
        Ok(())
    }

    /// Move the current column left/right
    pub(crate) fn scrolling_move_column(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let direction = match *arg {
            WMArgEnum::Int(i) => i,
            _ => return Ok(()),
        };

        let mon_key = match self.state.sel_mon {
            Some(k) => k,
            None => return Ok(()),
        };

        let sel = self.state.monitors.get(mon_key).and_then(|m| m.sel);
        let state = match self.scrolling_state_for_monitor_mut(mon_key) {
            Some(s) => s,
            None => return Ok(()),
        };
        let cur_col = match sel.and_then(|k| state.columns.iter().position(|col| col.contains(&k)))
        {
            Some(c) => c,
            None => return Ok(()),
        };

        let n_cols = state.columns.len();
        let new_col = if direction > 0 {
            if cur_col + 1 >= n_cols {
                return Ok(());
            }
            cur_col + 1
        } else {
            if cur_col == 0 {
                return Ok(());
            }
            cur_col - 1
        };

        state.ensure_column_metadata();
        state.columns.swap(cur_col, new_col);
        state.column_width_factors.swap(cur_col, new_col);
        state.focused_clients.swap(cur_col, new_col);
        state.set_focused_column(new_col);
        self.arrange(backend, Some(mon_key));
        Ok(())
    }

    /// Resize only the focused column. This gives scrolling layout an extra
    /// dimension of control beyond the global layout ratio.
    pub(crate) fn scrolling_set_column_width(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let delta = match *arg {
            WMArgEnum::Float(f) => f,
            _ => return Ok(()),
        };

        let mon_key = match self.state.sel_mon {
            Some(k) => k,
            None => return Ok(()),
        };

        let sel = self.state.monitors.get(mon_key).and_then(|m| m.sel);
        let state = match self.scrolling_state_for_monitor_mut(mon_key) {
            Some(s) => s,
            None => return Ok(()),
        };

        let cur_col = match sel.and_then(|k| state.columns.iter().position(|col| col.contains(&k)))
        {
            Some(c) => c,
            None => return Ok(()),
        };

        state.ensure_column_metadata();

        let current = state.column_width_factors[cur_col];
        let new_factor = if delta.abs() < 0.0001 {
            1.0
        } else {
            (current + delta).clamp(0.25, 2.5)
        };

        if (new_factor - current).abs() > 0.0001 {
            state.column_width_factors[cur_col] = new_factor;
            self.arrange(backend, Some(mon_key));
        }

        Ok(())
    }

    /// Toggle whether newly-opened windows attach to the focused column.
    pub(crate) fn scrolling_toggle_attach_mode(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mon_key = match self.state.sel_mon {
            Some(k) => k,
            None => return Ok(()),
        };

        let sel = self.state.monitors.get(mon_key).and_then(|m| m.sel);
        let state = match self.scrolling_state_for_monitor_mut_or_default(mon_key) {
            Some(state) => state,
            None => return Ok(()),
        };
        if let Some(sel_key) = sel {
            state.remember_focus(sel_key);
        }
        state.attach_new_windows_to_focused_column = !state.attach_new_windows_to_focused_column;
        let enabled = state.attach_new_windows_to_focused_column;

        self.broadcast_ipc_event(
            "scrolling/attach_mode",
            serde_json::json!({
                "enabled": enabled,
            }),
        );

        Ok(())
    }

    /// Consume: merge the focused window into the adjacent column
    pub(crate) fn scrolling_consume(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let direction = match *arg {
            WMArgEnum::Int(i) => i,
            _ => return Ok(()),
        };

        let mon_key = match self.state.sel_mon {
            Some(k) => k,
            None => return Ok(()),
        };

        let sel_key = match self.state.monitors.get(mon_key).and_then(|m| m.sel) {
            Some(k) => k,
            None => return Ok(()),
        };

        let state = match self.scrolling_state_for_monitor_mut(mon_key) {
            Some(s) => s,
            None => return Ok(()),
        };
        state.remember_focus(sel_key);

        // Find column and position of selected client
        let (cur_col, cur_pos) = match state
            .columns
            .iter()
            .enumerate()
            .find_map(|(ci, col)| col.iter().position(|&k| k == sel_key).map(|pos| (ci, pos)))
        {
            Some(v) => v,
            None => return Ok(()),
        };

        let target_col = if direction > 0 {
            if cur_col + 1 >= state.columns.len() {
                return Ok(());
            }
            cur_col + 1
        } else {
            if cur_col == 0 {
                return Ok(());
            }
            cur_col - 1
        };

        // Remove from current column
        state.columns[cur_col].remove(cur_pos);
        // Add to target column
        state.columns[target_col].push(sel_key);
        state.remember_focus(sel_key);
        state.retain_non_empty_columns();

        self.arrange(backend, Some(mon_key));
        Ok(())
    }

    /// Expel: take the focused window out of its column into a new standalone column
    pub(crate) fn scrolling_expel(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let direction = match *arg {
            WMArgEnum::Int(i) => i,
            _ => return Ok(()),
        };

        let mon_key = match self.state.sel_mon {
            Some(k) => k,
            None => return Ok(()),
        };

        let sel_key = match self.state.monitors.get(mon_key).and_then(|m| m.sel) {
            Some(k) => k,
            None => return Ok(()),
        };

        let state = match self.scrolling_state_for_monitor_mut(mon_key) {
            Some(s) => s,
            None => return Ok(()),
        };

        // Find column of selected client
        let (cur_col, cur_pos) = match state
            .columns
            .iter()
            .enumerate()
            .find_map(|(ci, col)| col.iter().position(|&k| k == sel_key).map(|pos| (ci, pos)))
        {
            Some(v) => v,
            None => return Ok(()),
        };

        // Only expel if the column has more than one window
        if state.columns[cur_col].len() <= 1 {
            return Ok(());
        }

        state.ensure_column_metadata();

        // Remove from current column
        state.columns[cur_col].remove(cur_pos);
        let cur_width = state
            .column_width_factors
            .get(cur_col)
            .copied()
            .unwrap_or(1.0);

        // Insert as new column in the given direction
        let insert_idx = if direction > 0 { cur_col + 1 } else { cur_col };
        state.columns.insert(insert_idx, vec![sel_key]);
        state.column_width_factors.insert(insert_idx, cur_width);
        state.focused_clients.insert(insert_idx, Some(sel_key));
        state.set_focused_column(insert_idx);

        self.arrange(backend, Some(mon_key));
        Ok(())
    }

    /// Focus the window above/below within the current column
    pub(crate) fn scrolling_focus_window(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let direction = match *arg {
            WMArgEnum::Int(i) => i,
            _ => return Ok(()),
        };

        let (mon_key, state) = match self.get_scrolling_state_for_sel_mon() {
            Some(v) => v,
            None => return Ok(()),
        };

        let sel = self.state.monitors.get(mon_key).and_then(|m| m.sel);
        let sel_key = match sel {
            Some(k) => k,
            None => return Ok(()),
        };

        // Find the column containing the selected window
        let (col_idx, pos) = match state
            .columns
            .iter()
            .enumerate()
            .find_map(|(ci, col)| col.iter().position(|&k| k == sel_key).map(|pos| (ci, pos)))
        {
            Some(v) => v,
            None => return Ok(()),
        };

        let col = &state.columns[col_idx];
        let n = col.len();
        if n <= 1 {
            return Ok(());
        }

        let new_pos = if direction > 0 {
            (pos + 1).min(n - 1)
        } else {
            pos.saturating_sub(1)
        };

        if new_pos != pos {
            let target = col[new_pos];
            if let Some(state) = self.scrolling_state_for_monitor_mut(mon_key) {
                state.remember_focus(target);
            }
            self.focus(backend, Some(target))?;
            self.arrange(backend, Some(mon_key));
        }
        Ok(())
    }
}

pub(crate) fn scrolling_column_width_rule_for_window(
    name: &str,
    class: &str,
    instance: &str,
    rules: &[String],
) -> Option<f32> {
    rules.iter().find_map(|rule| {
        let (factor, pattern) = rule.split_once(':')?;
        let factor = factor.trim().parse::<f32>().ok()?;
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return None;
        }
        if name.contains(pattern) || class.contains(pattern) || instance.contains(pattern) {
            Some(factor.clamp(0.25, 2.5))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::scrolling_column_width_rule_for_window;

    #[test]
    fn scrolling_column_width_rule_matches_class_name_or_instance() {
        let rules = vec![
            "1.35:Firefox".to_string(),
            "0.75:scratch".to_string(),
            "bad:Alacritty".to_string(),
        ];

        assert_eq!(
            scrolling_column_width_rule_for_window("", "Firefox", "", &rules),
            Some(1.35)
        );
        assert_eq!(
            scrolling_column_width_rule_for_window("scratch term", "Alacritty", "term", &rules),
            Some(0.75)
        );
        assert_eq!(
            scrolling_column_width_rule_for_window("Terminal", "Alacritty", "term", &rules),
            None
        );
    }

    #[test]
    fn scrolling_column_width_rule_clamps_factor() {
        assert_eq!(
            scrolling_column_width_rule_for_window("", "Huge", "", &["8.0:Huge".to_string()]),
            Some(2.5)
        );
        assert_eq!(
            scrolling_column_width_rule_for_window("", "Tiny", "", &["0.01:Tiny".to_string()]),
            Some(0.25)
        );
    }
}
