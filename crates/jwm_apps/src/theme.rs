//! Opaque native-chrome adapter for compositor semantic tokens. Socket work
//! never runs on the native event thread; absent/older JWM retains the last
//! valid theme. Explicit app overrides are resolved without changing JWM.
use crate::ui::Color;
use serde_json::Value;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NativeTheme {
    pub colors: [Color; 8],
    pub font: String,
    pub radius: f64,
    pub density: f64,
}
impl Default for NativeTheme {
    fn default() -> Self {
        Self {
            colors: [
                (0.012, 0.016, 0.020),
                (0.067, 0.078, 0.090),
                (0.141, 0.161, 0.188),
                (0.184, 0.192, 0.208),
                (0.945, 0.941, 0.949),
                (0.588, 0.573, 0.588),
                (0.533, 0.682, 0.957),
                (0.204, 0.306, 0.463),
            ],
            font: "Sans".into(),
            radius: 6.0,
            density: 1.0,
        }
    }
}
impl NativeTheme {
    fn parse(value: &Value) -> Option<Self> {
        let value = value.get("data").unwrap_or(value);
        if value["protocol_version"] != 1 {
            return None;
        }
        let values = value["native_colors"].as_array()?;
        if values.len() != 8 {
            return None;
        }
        let mut colors = [(0., 0., 0.); 8];
        for (index, color) in values.iter().enumerate() {
            let color = color.as_array()?;
            if color.len() < 3 {
                return None;
            }
            let mut rgb = [0.; 3];
            for (channel, component) in rgb.iter_mut().enumerate() {
                let number = color[channel].as_f64()?;
                if !number.is_finite() || !(0.0..=1.0).contains(&number) {
                    return None;
                }
                *component = number;
            }
            colors[index] = (rgb[0], rgb[1], rgb[2]);
        }
        let font = value["font"].as_str()?;
        if font.is_empty() || font.len() > 1024 || font.chars().any(char::is_control) {
            return None;
        }
        let radius = value["radius"].as_f64()?;
        if !radius.is_finite() || !(0.0..=64.0).contains(&radius) {
            return None;
        }
        let density = value["tokens"]["density"].as_f64().unwrap_or(1.0);
        if !density.is_finite() || !(0.8..=1.25).contains(&density) {
            return None;
        }
        Some(Self {
            colors,
            font: font.to_owned(),
            radius,
            density,
        })
    }
}

pub(crate) struct ThemeWatcher {
    updates: Receiver<NativeTheme>,
    stop: SyncSender<()>,
    worker: Option<JoinHandle<()>>,
}
impl ThemeWatcher {
    pub fn start() -> Self {
        let (sender, updates) = mpsc::sync_channel(1);
        let (stop, stopped) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("app-theme".into())
            .spawn(move || {
                let ipc = jwm_ipc_client::JwmIpc::new();
                let theme_override = std::env::var("JWM_APP_THEME")
                    .ok()
                    .filter(|theme| !theme.is_empty() && theme != "inherit");
                let font_override = std::env::var("JWM_APP_FONT").ok().filter(|font| {
                    !font.is_empty() && font.len() <= 1024 && !font.chars().any(char::is_control)
                });
                let mut last = None;
                loop {
                    if theme_override.as_deref() != Some("standalone") {
                        let args = theme_override
                            .as_ref()
                            .map(|theme| serde_json::json!({"theme":theme}))
                            .unwrap_or(Value::Null);
                        if let Ok(value) = ipc.query_with_args("get_ui_theme", args)
                            && let Some(mut theme) = NativeTheme::parse(&value)
                        {
                            if let Some(font) = &font_override {
                                theme.font.clone_from(font);
                            }
                            if last.as_ref() != Some(&theme) {
                                match sender.try_send(theme.clone()) {
                                    Ok(()) => last = Some(theme),
                                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                                    Err(mpsc::TrySendError::Full(_)) => {}
                                }
                            }
                        }
                    } else if last.is_none() {
                        let mut theme = NativeTheme::default();
                        if let Some(font) = &font_override {
                            theme.font.clone_from(font);
                        }
                        if sender.try_send(theme.clone()).is_ok() {
                            last = Some(theme);
                        }
                    }
                    match stopped.recv_timeout(Duration::from_secs(2)) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                }
            })
            .ok();
        Self {
            updates,
            stop,
            worker,
        }
    }
    pub fn take(&self) -> Option<NativeTheme> {
        self.updates.try_recv().ok()
    }
}
impl Drop for ThemeWatcher {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_adapter_accepts_complete_tokens_and_rejects_partial_or_invalid_revisions() {
        let mut value = serde_json::json!({"protocol_version":1,"native_colors":vec![[0.5,0.3,0.2,0.1];8],"font":"Noto Sans CJK SC 12","radius":8.0});
        let theme = NativeTheme::parse(&value).unwrap();
        assert_eq!(theme.colors[0], (0.5, 0.3, 0.2)); // native surfaces are opaque
        assert_eq!(theme.font, "Noto Sans CJK SC 12");
        value["native_colors"][2][0] = serde_json::json!(2.0);
        assert!(NativeTheme::parse(&value).is_none());
        value["native_colors"][2][0] = serde_json::json!(0.2);
        value["protocol_version"] = serde_json::json!(2);
        assert!(NativeTheme::parse(&value).is_none());
    }
}
