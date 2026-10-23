use crate::backend::api::{Backend, ToastNotification};
use crate::backend::common_define::{KeySym, Mods};
use crate::core::models::ClientKey;
use crate::jwm::{Jwm, WMArgEnum, WMFuncType};

impl Jwm {
    pub(crate) fn is_shortcut_passthrough_control(func: WMFuncType) -> bool {
        std::ptr::fn_addr_eq(func, Self::toggle_shortcut_passthrough as WMFuncType)
    }

    /// Only direct, first-match bindings can release passthrough. A chord
    /// cannot: its leader must be delivered to the remote desktop as well.
    pub(crate) fn shortcut_passthrough_controls(&self) -> Vec<(Mods, KeySym)> {
        let mut seen = std::collections::HashSet::new();
        self.key_bindings
            .iter()
            // Normal shortcut dispatch masks lock state out of each binding.
            // Preserve that identity for both first-match shadowing and the
            // escape grabs; otherwise a CapsLock binding can enter this mode
            // but never match its lock-free exit event.
            .map(|key| (key, key.mask & !(Mods::CAPS | Mods::NUMLOCK)))
            .filter(|(key, mask)| seen.insert((mask.bits(), key.key_sym)))
            .filter(|(key, mask)| {
                key.func_opt
                    .is_some_and(Self::is_shortcut_passthrough_control)
                    && self
                        .chord_compiled
                        .as_ref()
                        .is_none_or(|chord| chord.leader != (*mask, key.key_sym))
            })
            .map(|(key, mask)| (mask, key.key_sym))
            .collect()
    }

    pub fn toggle_shortcut_passthrough(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(mode) = self.grab_holding_mode_on_screen() {
            return Err(format!("close {mode} before changing shortcut passthrough").into());
        }
        let previous = self.shortcut_passthrough_owner;
        self.shortcut_passthrough_owner = if previous.is_some() {
            None
        } else {
            if self.shortcut_passthrough_controls().is_empty() {
                return Err(
                    "bind a direct toggle_shortcut_passthrough key before enabling it".into(),
                );
            }
            Some(
                self.get_selected_client_key()
                    .ok_or("focus a remote desktop window first")?,
            )
        };
        self.cancel_chord(backend);
        if let Err(error) = self.grabkeys(backend) {
            self.shortcut_passthrough_owner = previous;
            let _ = self.grabkeys(backend);
            return Err(error);
        }
        let enabled = self.shortcut_passthrough_owner.is_some();
        self.push_system_toast(
            backend,
            ToastNotification {
                title: if enabled {
                    "Remote shortcuts ON"
                } else {
                    "Local shortcuts restored"
                }
                .into(),
                body: if enabled {
                    "Use the same shortcut to exit, or click another window.".into()
                } else {
                    String::new()
                },
                timeout_ms: 3000,
                ..Default::default()
            },
        );
        log::info!(
            "[input] shortcut passthrough {}",
            if enabled { "on" } else { "off" }
        );
        Ok(())
    }

    pub(crate) fn sync_shortcut_passthrough_focus(
        &mut self,
        backend: &mut dyn Backend,
        focused: Option<ClientKey>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.shortcut_passthrough_owner.is_some() && self.shortcut_passthrough_owner != focused {
            self.shortcut_passthrough_owner = None;
            self.grabkeys(backend)?;
        }
        Ok(())
    }
}
