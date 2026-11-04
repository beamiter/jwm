//! Optional, bounded semantic-theme IPC. Render/input threads only enqueue a
//! coalesced refresh and take the latest snapshot; socket IO stays on one worker.
use crate::shell_palette::SemanticTokens;
use serde_json::Value;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender},
};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct ThemeUpdate {
    pub name: String,
    pub tokens: SemanticTokens,
    pub font: Option<String>,
}
impl ThemeUpdate {
    pub fn parse(value: &Value) -> Option<Self> {
        if value["protocol_version"].as_u64()? != 1 {
            return None;
        }
        let name = value["theme"].as_str()?.to_owned();
        if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
            return None;
        }
        let tokens: SemanticTokens = serde_json::from_value(value["tokens"].clone()).ok()?;
        if [
            tokens.surface,
            tokens.text,
            tokens.muted,
            tokens.accent,
            tokens.focus,
            tokens.warning,
        ]
        .iter()
        .flatten()
        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            || !tokens.radius.is_finite()
            || !(0.0..=128.0).contains(&tokens.radius)
            || !tokens.spacing.is_finite()
            || !(0.0..=256.0).contains(&tokens.spacing)
            || !tokens.density.is_finite()
            || !(0.8..=1.25).contains(&tokens.density)
            || tokens.motion_ms > 60_000
            || tokens.glass_quality > 3
        {
            return None;
        }
        let font = value["font"]
            .as_str()
            .filter(|font| {
                !font.is_empty() && font.len() <= 1024 && !font.chars().any(char::is_control)
            })
            .map(str::to_owned);
        Some(Self { name, tokens, font })
    }
}

/// The shared Pango-style family/point-size boundary used by toolkit adapters.
/// Style suffixes are omitted from the family; each toolkit chooses the face.
pub fn font_spec(description: &str) -> (String, f32) {
    let mut words: Vec<_> = description.split_whitespace().collect();
    let size = words
        .last()
        .and_then(|word| word.parse::<f32>().ok())
        .filter(|size| size.is_finite() && (6.0..=48.0).contains(size));
    if size.is_some() {
        words.pop();
    }
    while words.last().is_some_and(|word| {
        [
            "regular", "bold", "italic", "book", "light", "medium", "semibold", "oblique",
        ]
        .contains(&word.to_ascii_lowercase().as_str())
    }) {
        words.pop();
    }
    let family = words.join(" ");
    (
        if family.is_empty() {
            "Sans".into()
        } else {
            family
        },
        size.unwrap_or(11.0),
    )
}

pub struct ThemeWatcher {
    refresh: SyncSender<()>,
    latest: Arc<Mutex<Option<ThemeUpdate>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl ThemeWatcher {
    pub fn start(override_name: Option<String>) -> Self {
        let (refresh, requests) = mpsc::sync_channel(1);
        let latest = Arc::new(Mutex::new(None));
        let output = Arc::clone(&latest);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("semantic-theme".into())
            .spawn(move || {
                let mut last = None;
                while requests.recv().is_ok() {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    if override_name.as_deref() == Some("standalone") {
                        continue;
                    }
                    if let Some(theme) = fetch_theme(override_name.as_deref())
                        && last.as_ref() != Some(&theme)
                    {
                        *output.lock().unwrap_or_else(|poison| poison.into_inner()) =
                            Some(theme.clone());
                        last = Some(theme);
                    }
                }
            })
            .ok();
        let watcher = Self {
            refresh,
            latest,
            stop,
            worker,
        };
        watcher.request_refresh();
        watcher
    }
    pub fn request_refresh(&self) {
        let _ = self.refresh.try_send(());
    }
    pub fn take(&self) -> Option<ThemeUpdate> {
        match self.latest.try_lock() {
            Ok(mut latest) => latest.take(),
            Err(std::sync::TryLockError::Poisoned(poison)) => poison.into_inner().take(),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
}
impl Drop for ThemeWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.refresh.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn fetch_theme(override_name: Option<&str>) -> Option<ThemeUpdate> {
    let path = std::env::var_os("JWM_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(std::path::PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(format!("/tmp/jwm-{}", unsafe { libc::geteuid() }))
                })
                .join("jwm-ipc.sock")
        });
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut stream = jwm_ipc_transport::connect(&path, Duration::from_millis(500)).ok()?;
    jwm_ipc_transport::verify_same_user(&stream).ok()?;
    let args = override_name
        .filter(|name| *name != "inherit")
        .map(|name| serde_json::json!({"theme":name}))
        .unwrap_or(Value::Null);
    let mut request =
        serde_json::to_vec(&serde_json::json!({"query":"get_ui_theme","args":args})).ok()?;
    request.push(b'\n');
    jwm_ipc_transport::write_all_with_timeout(
        &mut stream,
        &request,
        deadline.saturating_duration_since(Instant::now()),
    )
    .ok()?;
    let bytes = jwm_ipc_transport::read_frame_until(
        &mut std::io::BufReader::new(stream),
        32 * 1024,
        deadline,
    )
    .ok()?;
    let response: Value = serde_json::from_slice(&bytes).ok()?;
    if response["success"].as_bool() != Some(true) {
        return None;
    }
    ThemeUpdate::parse(response.get("data").unwrap_or(&response))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_thread_does_not_wait_for_the_producer_and_receives_only_latest_state() {
        let watcher = ThemeWatcher::start(Some("standalone".into()));
        let mut latest = watcher.latest.lock().unwrap();
        assert!(watcher.take().is_none());
        *latest = Some(ThemeUpdate {
            name: "nord".into(),
            tokens: crate::shell_palette::ShellPalette::DARK.tokens(),
            font: Some("Noto Sans CJK SC 14".into()),
        });
        latest.as_mut().unwrap().name = "paper".into();
        drop(latest);
        assert_eq!(watcher.take().unwrap().name, "paper");
        assert!(watcher.take().is_none());
        assert_eq!(
            font_spec("Noto Sans CJK SC Regular 14"),
            ("Noto Sans CJK SC".into(), 14.0)
        );
    }

    #[test]
    fn semantic_snapshots_keep_alpha_separate_and_reject_invalid_or_partial_values() {
        let tokens = crate::shell_palette::ShellPalette::DARK.tokens();
        let mut value = serde_json::json!({"protocol_version":1,"theme":"nord","tokens":tokens});
        assert_eq!(ThemeUpdate::parse(&value).unwrap().tokens.text[3], 1.0);
        assert!(ThemeUpdate::parse(&value).unwrap().tokens.surface[3] < 1.0);
        value["tokens"]["text"][0] = serde_json::json!(2.0);
        assert!(ThemeUpdate::parse(&value).is_none());
        value["tokens"] = serde_json::json!({"surface":[0.0,0.0,0.0,1.0]});
        assert!(ThemeUpdate::parse(&value).is_none());
    }
}
