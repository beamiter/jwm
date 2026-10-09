use std::collections::HashMap;

use crate::backend::api::KeyOps;
use crate::backend::common_define::{KeySym, Mods, WindowId};
use crate::backend::error::BackendError;

use xkbcommon::xkb;

/// Keeps a nested backend's suppression snapshot in step with policy's grabs.
/// Publish only a successful replacement; clearing grabs during a reload must
/// not discard the last usable snapshot before the new bindings are installed.
pub struct BindingAwareKeyOps {
    inner: Box<dyn KeyOps>,
    bindings_changed: Box<dyn Fn(&[(Mods, KeySym)]) + Send>,
}

impl BindingAwareKeyOps {
    pub fn new(
        inner: Box<dyn KeyOps>,
        bindings_changed: impl Fn(&[(Mods, KeySym)]) + Send + 'static,
    ) -> Self {
        Self {
            inner,
            bindings_changed: Box::new(bindings_changed),
        }
    }
}

impl KeyOps for BindingAwareKeyOps {
    fn grab_keys(&self, root: WindowId, bindings: &[(Mods, KeySym)]) -> Result<(), BackendError> {
        self.inner.grab_keys(root, bindings)?;
        (self.bindings_changed)(bindings);
        Ok(())
    }

    fn clear_key_grabs(&self, root: WindowId) -> Result<(), BackendError> {
        self.inner.clear_key_grabs(root)
    }

    fn grab_keyboard(&self, root: WindowId) -> Result<(), BackendError> {
        self.inner.grab_keyboard(root)
    }

    fn ungrab_keyboard(&self) -> Result<(), BackendError> {
        self.inner.ungrab_keyboard()
    }

    fn clean_mods(&self, raw_state: u16) -> Mods {
        self.inner.clean_mods(raw_state)
    }

    fn keysym_from_keycode(&mut self, keycode: u8) -> Result<KeySym, BackendError> {
        self.inner.keysym_from_keycode(keycode)
    }

    fn clear_cache(&mut self) {
        self.inner.clear_cache();
    }
}

/// Keyboard helpers for the udev/libinput backend.
///
/// JWM's keybinding matching expects an *unmodified* keysym (like X11's "level 0" mapping),
/// while modifiers are matched separately via `Mods`.
pub struct UdevKeyOps {
    #[allow(dead_code)]
    context: xkb::Context,
    #[allow(dead_code)]
    keymap: xkb::Keymap,
    base_state: xkb::State,
    cache: HashMap<u8, KeySym>,
}

// NOTE: xkbcommon types are not marked Send due to raw pointers internally.
// JWM's udev backend is single-threaded and `UdevKeyOps` never crosses threads,
// so this is safe under that assumption.
unsafe impl Send for UdevKeyOps {}

impl UdevKeyOps {
    pub fn new() -> Result<Self, BackendError> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);

        // Build a keymap from the conventional environment variables used by wlroots/sway/etc.
        // This makes keyboard mapping work reliably on TTY + udev.
        //
        // If the user doesn't provide anything, fall back to common defaults.
        fn env_nonempty(key: &str) -> Option<String> {
            std::env::var(key).ok().and_then(|v| {
                let v = v.trim().to_string();
                if v.is_empty() { None } else { Some(v) }
            })
        }

        let rules = env_nonempty("XKB_DEFAULT_RULES").unwrap_or_else(|| "evdev".to_string());
        let model = env_nonempty("XKB_DEFAULT_MODEL").unwrap_or_else(|| "pc105".to_string());
        let layout = env_nonempty("XKB_DEFAULT_LAYOUT").unwrap_or_else(|| "us".to_string());
        let variant = env_nonempty("XKB_DEFAULT_VARIANT").unwrap_or_default();
        let options = env_nonempty("XKB_DEFAULT_OPTIONS");

        log::info!(
            "xkb keymap: rules={rules:?} model={model:?} layout={layout:?} variant={variant:?} options={options:?}"
        );

        let keymap = xkb::Keymap::new_from_names(
            &context,
            &rules,
            &model,
            &layout,
            &variant,
            options,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| BackendError::Message("xkb keymap creation failed".into()))?;

        let base_state = xkb::State::new(&keymap);

        Ok(Self {
            context,
            keymap,
            base_state,
            cache: HashMap::new(),
        })
    }

    fn keysym_from_xkb_keycode_uncached(&self, keycode: u8) -> KeySym {
        let kc = xkb::Keycode::new(keycode as u32);
        let sym = self.base_state.key_get_one_sym(kc);
        sym.raw()
    }
}

impl KeyOps for UdevKeyOps {
    fn grab_keys(&self, _root: WindowId, _bindings: &[(Mods, KeySym)]) -> Result<(), BackendError> {
        // No global key grabbing in the udev backend. Its libinput path
        // atomically refreshes suppression/repeat metadata from CONFIG on the
        // first physical key after an ArcSwap generation change.
        Ok(())
    }

    fn clear_key_grabs(&self, _root: WindowId) -> Result<(), BackendError> {
        Ok(())
    }

    fn clean_mods(&self, raw_state: u16) -> Mods {
        // For the udev backend, `raw_state` is already stored as JWM's `Mods` bitflags.
        Mods::from_bits_truncate(raw_state)
    }

    fn keysym_from_keycode(&mut self, keycode: u8) -> Result<KeySym, BackendError> {
        if let Some(&ks) = self.cache.get(&keycode) {
            return Ok(ks);
        }

        let sym = self.keysym_from_xkb_keycode_uncached(keycode);
        self.cache.insert(keycode, sym);
        Ok(sym)
    }

    fn clear_cache(&mut self) {
        self.cache.clear();
    }
}

#[cfg(test)]
mod binding_observer_tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    struct TestKeyOps(Arc<AtomicBool>);
    impl KeyOps for TestKeyOps {
        fn grab_keys(
            &self,
            _root: WindowId,
            _bindings: &[(Mods, KeySym)],
        ) -> Result<(), BackendError> {
            if self.0.load(Ordering::Relaxed) {
                Err(BackendError::Message("test binding update failed".into()))
            } else {
                Ok(())
            }
        }
        fn clear_key_grabs(&self, _root: WindowId) -> Result<(), BackendError> {
            Ok(())
        }
        fn clean_mods(&self, raw: u16) -> Mods {
            Mods::from_bits_truncate(raw)
        }
        fn keysym_from_keycode(&mut self, keycode: u8) -> Result<KeySym, BackendError> {
            Ok(keycode as KeySym)
        }
        fn clear_cache(&mut self) {}
    }

    #[test]
    fn successful_regrabs_replace_added_and_removed_bindings() {
        let snapshot = Arc::new(Mutex::new(Vec::new()));
        let observed = snapshot.clone();
        let keys = BindingAwareKeyOps::new(
            Box::new(TestKeyOps(Arc::new(AtomicBool::new(false)))),
            move |bindings| *observed.lock().unwrap() = bindings.to_vec(),
        );
        let root = WindowId::from_raw(0);
        let old = [(Mods::ALT, 1)];
        let new = [(Mods::ALT | Mods::SHIFT, 2)];
        keys.grab_keys(root, &old).unwrap();
        assert_eq!(*snapshot.lock().unwrap(), old);
        keys.clear_key_grabs(root).unwrap();
        assert_eq!(*snapshot.lock().unwrap(), old);
        keys.grab_keys(root, &new).unwrap();
        assert_eq!(*snapshot.lock().unwrap(), new);
        keys.grab_keys(root, &[]).unwrap();
        assert!(snapshot.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_regrab_keeps_the_previous_snapshot() {
        let fail = Arc::new(AtomicBool::new(false));
        let snapshot = Arc::new(Mutex::new(Vec::new()));
        let observed = snapshot.clone();
        let mut keys =
            BindingAwareKeyOps::new(Box::new(TestKeyOps(fail.clone())), move |bindings| {
                *observed.lock().unwrap() = bindings.to_vec()
            });
        let root = WindowId::from_raw(0);
        let old = [(Mods::ALT, 1)];
        keys.grab_keys(root, &old).unwrap();
        fail.store(true, Ordering::Relaxed);
        keys.clear_key_grabs(root).unwrap();
        assert!(keys.grab_keys(root, &[(Mods::SUPER, 2)]).is_err());
        assert_eq!(*snapshot.lock().unwrap(), old);
        keys.clear_cache();
        assert_eq!(keys.keysym_from_keycode(42).unwrap(), 42);
        assert_eq!(*snapshot.lock().unwrap(), old);
    }
}
