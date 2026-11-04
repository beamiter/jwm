//! Theme observation is independent from playback and D-Bus dispatch. A slow
//! compositor cannot hold a media method or grow a blocking-worker queue.
use super::backend::{Action, Controller};
use serde_json::{Value, json};
use std::time::Duration;

pub(super) struct Subscription(tokio::task::JoinHandle<()>);
impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn payload(value: Value, font_override: Option<&str>) -> Option<Value> {
    if value["protocol_version"] != 1 {
        return None;
    }
    let colors = value["native_colors"].as_array()?;
    if colors.len() != 8
        || colors.iter().any(|color| {
            color.as_array().is_none_or(|channels| {
                channels.len() < 3
                    || channels.iter().take(3).any(|channel| {
                        channel
                            .as_f64()
                            .is_none_or(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
                    })
            })
        })
    {
        return None;
    }
    let revision = value["config_revision"].as_str()?;
    if revision.is_empty()
        || revision.len() > 128
        || !revision
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_-:".contains(ch))
    {
        return None;
    }
    let font = font_override.or_else(|| value["font"].as_str())?;
    if font.is_empty() || font.len() > 1024 || font.chars().any(char::is_control) {
        return None;
    }
    let opacity = value["surface_opacity"].as_f64().unwrap_or(1.0);
    if !opacity.is_finite() || !(0.1..=1.0).contains(&opacity) {
        return None;
    }
    Some(
        json!({"protocol_version":1,"native_colors":colors,"font":font,"surface_opacity":opacity,"config_revision":revision}),
    )
}

pub(super) fn start(controller: Controller) -> Option<Subscription> {
    let selected = std::env::var("JWM_PLAYER_THEME")
        .ok()
        .filter(|name| !name.is_empty() && name != "inherit");
    if selected.as_deref() == Some("standalone") {
        return None;
    }
    let font = std::env::var("JWM_PLAYER_FONT").ok();
    Some(Subscription(tokio::spawn(async move {
        let mut previous = None;
        loop {
            let args = selected
                .as_ref()
                .map(|theme| json!({"theme":theme}))
                .unwrap_or(Value::Null);
            let response = tokio::task::spawn_blocking(move || {
                crate::jwm_ipc::JwmIpc::new().query_with_args("get_ui_theme", args)
            })
            .await;
            if let Ok(Ok(value)) = response
                && let Some(value) = payload(value, font.as_deref())
                && previous.as_ref() != Some(&value)
                && controller
                    .submit(Action::Theme(value.clone()))
                    .await
                    .is_ok()
            {
                previous = Some(value);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn player_accepts_complete_style_and_preserves_explicit_font_override() {
        let value = json!({"protocol_version":1,"native_colors":vec![[0.2,0.3,0.4,0.5];8],"font":"Sans 18","surface_opacity":0.85,"config_revision":"abc123"});
        assert_eq!(
            payload(value.clone(), Some("Noto Sans CJK SC 14")).unwrap()["font"],
            "Noto Sans CJK SC 14"
        );
        let mut invalid = value;
        invalid["native_colors"][0][0] = json!(2.0);
        assert!(payload(invalid, None).is_none());
    }
}
