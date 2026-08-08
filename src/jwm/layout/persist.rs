//! Per-tag layout persistence.
//!
//! Every monitor keeps a layout, a master count, a master fraction and a gap
//! *per tag* in its [`Pertag`](crate::core::models::Pertag) block. That state
//! lives entirely in memory, so before this module a restart put every tag
//! back on the built-in default however carefully the desktop had been
//! arranged.
//!
//! Here it becomes durable, through the config file rather than a private
//! state file: the same `[[layout.tags]]` entries JWM writes are the ones a
//! user can write by hand, so "the layout my tags start in" is one concept
//! with one place to look. Two directions:
//!
//! * **Restore** — [`seed_pertag_from_config`] runs when a monitor is created,
//!   filling its pertag block from the entries that match its index.
//! * **Save** — a layout change marks the state dirty, and the update loop
//!   flushes it a couple of seconds later. Dragging the master fraction fires
//!   a change per motion event, so the debounce is what keeps a drag from
//!   turning into a hundred file writes.
//!
//! The write is a surgical edit of the `[[layout.tags]]` block
//! ([`Config::persist_layout_tags`]), not a re-serialization of the file, so
//! the comments and formatting around it survive.

use crate::config::{CONFIG, Config, ConfigError, LayoutTagConfig};
use crate::core::layout::LayoutEnum;
use crate::core::models::WMMonitor;
use crate::jwm::Jwm;
use log::{debug, warn};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the layout has to hold still before it is written out. Long
/// enough that a `setmfact` drag is one write, short enough that a session
/// ended by pulling the plug loses at most a couple of seconds of arranging.
const PERSIST_DEBOUNCE: Duration = Duration::from_secs(2);

/// Bounds saved values are held to on the way back in. They match what the
/// interactive commands enforce, so a hand-edited file cannot put a monitor
/// into a state the keybindings could not have produced.
const MIN_M_FACT: f32 = 0.05;
const MAX_M_FACT: f32 = 0.95;
const MAX_N_MASTER: u32 = 32;
const MAX_GAP: i32 = 100;

fn settle_layout_persist_dirty(dirty: &mut Option<Instant>, retry_at: Instant, succeeded: bool) {
    *dirty = if succeeded { None } else { Some(retry_at) };
}

/// Fill `monitor`'s pertag block from the config entries matching monitor
/// index `mon_index`, then re-apply the current tag so the monitor shows what
/// was restored rather than the defaults it was created with.
///
/// Unknown layout names and out-of-range numbers are skipped field by field:
/// a typo in one entry costs that one value, not the whole restore.
pub(crate) fn seed_pertag_from_config(monitor: &mut WMMonitor, mon_index: i32, config: &Config) {
    if monitor.pertag.is_none() || config.layout_tags().is_empty() {
        return;
    }
    let tags_length = config.tags_length();
    let mut restored = 0usize;

    for tag in 0..=tags_length {
        let Some(entry) = config.layout_for_tag(mon_index, tag).cloned() else {
            continue;
        };
        let Some(pertag) = monitor.pertag.as_mut() else {
            return;
        };
        if pertag.lts.len() <= tag {
            continue;
        }

        if let Some(layout) = LayoutEnum::from_name(&entry.layout) {
            pertag.lts[tag] = Rc::new(layout.clone());
            restored += 1;
        }
        // `alt` is the layout the tag was on before its last change, kept so
        // `lastlayout` still has somewhere to go after a restart.
        if let Some(prev) = LayoutEnum::from_name(&entry.alt) {
            pertag.prev_lts[tag] = Rc::new(prev.clone());
        }
        if let Some(n_master) = entry.n_master {
            pertag.n_masters[tag] = n_master.min(MAX_N_MASTER);
        }
        if let Some(m_fact) = entry.m_fact.filter(|value| value.is_finite()) {
            pertag.m_facts[tag] = m_fact.clamp(MIN_M_FACT, MAX_M_FACT);
        }
        if let Some(gap) = entry.gap {
            pertag.gaps[tag] = gap.clamp(0, MAX_GAP);
        }
    }

    if restored > 0 {
        debug!("[layout] restored {restored} saved tag layouts for monitor {mon_index}");
    }
    monitor.reload_current_tag_context();
}

impl Jwm {
    /// Note that a tag's layout changed. The write itself waits for
    /// [`Jwm::flush_layout_persistence`] on a later update tick.
    pub(crate) fn mark_layout_dirty(&mut self) {
        if !CONFIG.load().layout_persist_tags() {
            return;
        }
        self.layout_persist_dirty = Some(Instant::now());
    }

    /// Write the per-tag layouts back to the config file once they have held
    /// still for [`PERSIST_DEBOUNCE`]. Called from the periodic update.
    pub(crate) fn flush_layout_persistence(&mut self, now: Instant) {
        let Some(changed_at) = self.layout_persist_dirty else {
            return;
        };
        if now.saturating_duration_since(changed_at) < PERSIST_DEBOUNCE {
            return;
        }
        if !CONFIG.load().layout_persist_tags() {
            self.layout_persist_dirty = None;
            return;
        }
        // An edit of the user's is waiting to be reloaded. Writing now would
        // stamp a new revision over it and the reload would never happen, so
        // the save waits for the next tick instead.
        if self.config_reload_is_pending() {
            return;
        }
        if let Err(error) = self.save_layout_tags() {
            // Keep the write pending, but restart the debounce so a read-only
            // filesystem or transient rename failure does not turn the update
            // loop into a tight I/O retry loop.
            settle_layout_persist_dirty(&mut self.layout_persist_dirty, now, false);
            warn!("[layout] could not save per-tag layouts: {error}");
        }
    }

    /// Write out anything still pending, debounce or not.
    ///
    /// Called on the way out, so quitting or restarting inside the debounce
    /// window keeps the arrangement that was on screen — which is the whole
    /// point on the restart path, where the next process reads it straight
    /// back in.
    pub(crate) fn flush_layout_persistence_on_exit(&mut self) -> Result<(), ConfigError> {
        if self.layout_persist_dirty.is_none() || !CONFIG.load().layout_persist_tags() {
            return Ok(());
        }
        match self.save_layout_tags() {
            Ok(()) => Ok(()),
            Err(error) => {
                // Preserve an explicit retryable marker. Restart preparation
                // propagates the error and resumes this same event loop; a
                // later periodic flush therefore gets another chance.
                settle_layout_persist_dirty(&mut self.layout_persist_dirty, Instant::now(), false);
                Err(error)
            }
        }
    }

    fn save_layout_tags(&mut self) -> Result<(), ConfigError> {
        let entries = self.layout_tag_entries();
        let config = CONFIG.load_full();
        let revision = config.persist_layout_tags(&entries)?;

        // Keep the live config in step with the file, so a later whole-file
        // write does not resurrect the previous entries. Clear dirty only
        // after the durable write succeeded.
        let mut updated = (*config).clone();
        updated.set_layout_tags(entries);
        CONFIG.store(Arc::new(updated));
        self.note_config_written_by_us(revision);
        settle_layout_persist_dirty(&mut self.layout_persist_dirty, Instant::now(), true);
        debug!("[layout] saved per-tag layouts");
        Ok(())
    }

    /// Every monitor's per-tag layout, as config entries.
    ///
    /// One entry per (monitor, tag) including tag 0, the slot a monitor uses
    /// while showing all its tags at once — it is as much a place windows get
    /// arranged in as the numbered ones.
    pub(crate) fn layout_tag_entries(&self) -> Vec<LayoutTagConfig> {
        let tags_length = CONFIG.load().tags_length();
        let mut entries = Vec::with_capacity(self.state.monitor_order.len() * (tags_length + 1));

        for (index, &mon_key) in self.state.monitor_order.iter().enumerate() {
            let Some(monitor) = self.state.monitors.get(mon_key) else {
                continue;
            };
            let Some(pertag) = monitor.pertag.as_ref() else {
                continue;
            };
            for tag in 0..=tags_length {
                let Some(layout) = pertag.lts.get(tag) else {
                    continue;
                };
                entries.push(LayoutTagConfig {
                    tag,
                    monitor: index as i32,
                    layout: layout.0.to_owned(),
                    alt: pertag
                        .prev_lts
                        .get(tag)
                        .map(|prev| prev.0.to_owned())
                        .unwrap_or_default(),
                    n_master: pertag.n_masters.get(tag).copied(),
                    m_fact: pertag.m_facts.get(tag).copied(),
                    gap: pertag.gaps.get(tag).copied(),
                });
            }
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::Pertag;

    fn monitor_with_tags(tags: usize) -> WMMonitor {
        let mut monitor = WMMonitor::new();
        monitor.pertag = Some(Pertag::new(true, tags));
        if let Some(pertag) = monitor.pertag.as_mut() {
            pertag.cur_tag = 1;
            pertag.prev_tag = 1;
            for tag in 0..=tags {
                pertag.lts[tag] = Rc::new(LayoutEnum::FIBONACCI);
                pertag.prev_lts[tag] = Rc::new(LayoutEnum::TILE);
                pertag.n_masters[tag] = 1;
                pertag.m_facts[tag] = 0.55;
            }
        }
        monitor
    }

    fn entry(tag: usize, monitor: i32, layout: &str) -> LayoutTagConfig {
        LayoutTagConfig {
            tag,
            monitor,
            layout: layout.to_owned(),
            alt: String::new(),
            n_master: None,
            m_fact: None,
            gap: None,
        }
    }

    fn config_with(tags: Vec<LayoutTagConfig>) -> Config {
        let mut config = Config::default();
        config.set_layout_tags(tags);
        config
    }

    #[test]
    fn failed_layout_write_stays_dirty_while_success_commits_it() {
        let original = Instant::now();
        let retry_at = original + Duration::from_secs(7);
        let mut dirty = Some(original);

        settle_layout_persist_dirty(&mut dirty, retry_at, false);
        assert_eq!(dirty, Some(retry_at));

        settle_layout_persist_dirty(&mut dirty, retry_at, true);
        assert_eq!(dirty, None);
    }

    #[test]
    fn a_saved_layout_comes_back_selected_on_its_tag() {
        let mut monitor = monitor_with_tags(9);
        let mut saved = entry(1, 0, "monocle");
        saved.alt = "grid".to_owned();
        saved.n_master = Some(3);
        saved.m_fact = Some(0.7);
        saved.gap = Some(12);
        seed_pertag_from_config(&mut monitor, 0, &config_with(vec![saved]));

        let pertag = monitor.pertag.as_ref().expect("pertag");
        assert_eq!(*pertag.lts[1], LayoutEnum::MONOCLE);
        assert_eq!(*pertag.prev_lts[1], LayoutEnum::GRID);
        // The monitor itself must show the restored tag, not the default it
        // was built with.
        assert_eq!(*monitor.lt, LayoutEnum::MONOCLE);
        assert_eq!(monitor.lt_symbol, LayoutEnum::MONOCLE.symbol());
        assert_eq!(monitor.layout.n_master, 3);
        assert!((monitor.layout.m_fact - 0.7).abs() < 1e-6);
        assert_eq!(monitor.layout.gap, 12);
    }

    #[test]
    fn untouched_tags_keep_their_defaults() {
        let mut monitor = monitor_with_tags(9);
        seed_pertag_from_config(&mut monitor, 0, &config_with(vec![entry(2, 0, "grid")]));
        let pertag = monitor.pertag.as_ref().expect("pertag");
        assert_eq!(*pertag.lts[2], LayoutEnum::GRID);
        assert_eq!(*pertag.lts[3], LayoutEnum::FIBONACCI);
    }

    #[test]
    fn a_monitor_specific_entry_wins_over_the_any_monitor_one() {
        let mut monitor = monitor_with_tags(9);
        seed_pertag_from_config(
            &mut monitor,
            1,
            &config_with(vec![entry(1, -1, "grid"), entry(1, 1, "deck")]),
        );
        assert_eq!(*monitor.lt, LayoutEnum::DECK);

        // ...and a monitor with no entry of its own still gets the shared one.
        let mut other = monitor_with_tags(9);
        seed_pertag_from_config(
            &mut other,
            2,
            &config_with(vec![entry(1, -1, "grid"), entry(1, 1, "deck")]),
        );
        assert_eq!(*other.lt, LayoutEnum::GRID);
    }

    /// A hand-edited file is the normal way these entries get written, so one
    /// bad field must cost that field only.
    #[test]
    fn nonsense_values_are_clamped_or_skipped_field_by_field() {
        let mut monitor = monitor_with_tags(9);
        let mut saved = entry(1, 0, "no-such-layout");
        saved.n_master = Some(9_999);
        saved.m_fact = Some(f32::NAN);
        saved.gap = Some(-40);
        seed_pertag_from_config(&mut monitor, 0, &config_with(vec![saved]));

        // The unknown name left the tag on its default...
        assert_eq!(*monitor.lt, LayoutEnum::FIBONACCI);
        // ...while the numbers that could be salvaged were.
        assert_eq!(monitor.layout.n_master, MAX_N_MASTER);
        assert!((monitor.layout.m_fact - 0.55).abs() < 1e-6);
        assert_eq!(monitor.layout.gap, 0);
    }

    #[test]
    fn an_entry_for_a_tag_that_does_not_exist_is_ignored() {
        let mut monitor = monitor_with_tags(4);
        seed_pertag_from_config(&mut monitor, 0, &config_with(vec![entry(30, 0, "grid")]));
        assert_eq!(*monitor.lt, LayoutEnum::FIBONACCI);
    }
}
