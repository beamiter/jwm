//! One bus name serializes startup and forwards further launches. Interfaces
//! are registered before claiming the name, and mpv starts only for its owner.

use std::collections::HashMap;
use std::time::Duration;

use zbus::{Connection, interface};
use zvariant::{ObjectPath, OwnedValue, Str, Value};

use super::backend::{Action, Controller};
use super::cli::{Mode, Open, file_uri, normalize_files};
use super::eva;
use super::model::State;

pub const NAME: &str = "org.mpris.MediaPlayer2.jwm";
pub const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const NATIVE: &str = "org.jwm.Player";
// A filesystem mounted on an unavailable server can outlive its async timeout.
// Hold admission inside the blocking closure, so abandoned calls cannot grow
// the blocking pool without limit.
static FILE_VALIDATION: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
static WINDOW_FOCUS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

struct Root(Controller, bool);
struct Player(Controller);
struct Native(Controller, bool);

fn string(value: impl Into<String>) -> OwnedValue {
    OwnedValue::from(Str::from(value.into()))
}

fn metadata(state: &State) -> HashMap<String, OwnedValue> {
    let mut result = HashMap::new();
    let path = ObjectPath::try_from(state.track_path()).expect("generated track path");
    result.insert("mpris:trackid".into(), OwnedValue::from(path));
    if let Some(length) = state.duration_us {
        result.insert("mpris:length".into(), length.into());
    }
    if !state.path.is_empty() {
        result.insert("xesam:url".into(), string(file_uri(&state.path)));
    }
    let title = if state.title.is_empty() {
        std::path::Path::new(&state.path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("JWM Player")
    } else {
        &state.title
    };
    result.insert("xesam:title".into(), string(title));
    if !state.artist.is_empty()
        && let Ok(artists) = OwnedValue::try_from(Value::from(vec![state.artist.clone()]))
    {
        result.insert("xesam:artist".into(), artists);
    }
    if !state.album.is_empty() {
        result.insert("xesam:album".into(), string(state.album.clone()));
    }
    result
}

async fn normalized_open(
    files: Vec<String>,
    enqueue: bool,
    mode: String,
) -> zbus::fdo::Result<Open> {
    let mode = Mode::parse(&mode).map_err(zbus::fdo::Error::InvalidArgs)?;
    let permit = FILE_VALIDATION.try_acquire().map_err(|_| {
        zbus::fdo::Error::Failed(
            "file validation is busy; retry when the current file opens".into(),
        )
    })?;
    let files = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            normalize_files(&files)
        }),
    )
    .await
    .map_err(|_| zbus::fdo::Error::Failed("file validation timed out".into()))?
    .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
    .map_err(zbus::fdo::Error::InvalidArgs)?;
    Ok(Open {
        files,
        enqueue,
        mode,
    })
}

async fn focus_jwm_window() {
    let Ok(permit) = WINDOW_FOCUS.try_acquire() else {
        return;
    };
    let _ = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let ipc = crate::jwm_ipc::JwmIpc::new();
        let windows = ipc.query("get_windows").ok()?;
        let window = windows
            .as_array()?
            .iter()
            .find(|window| window["class"] == "jwm-player" || window["instance"] == "jwm-player")?;
        let id = window.get("id")?.as_u64()?;
        ipc.command("focus_window", serde_json::json!({"id":id}))
            .ok()
    })
    .await;
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    async fn raise(&self) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Raise).await?;
        if self.1 {
            focus_jwm_window().await;
        }
        Ok(())
    }
    async fn quit(&self) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Quit).await
    }
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn identity(&self) -> &str {
        "JWM Player"
    }
    #[zbus(property)]
    fn desktop_entry(&self) -> &str {
        "jwm-player"
    }
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<&str> {
        vec!["file"]
    }
    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<&str> {
        vec![
            "audio/mpeg",
            "audio/flac",
            "audio/ogg",
            "audio/opus",
            "audio/x-wav",
            "audio/mp4",
            "audio/aac",
            "video/mp4",
            "video/x-matroska",
            "video/webm",
            "video/mpeg",
            "video/quicktime",
            "video/x-msvideo",
        ]
    }
    #[zbus(property)]
    fn fullscreen(&self) -> bool {
        self.0.snapshot().fullscreen
    }
    #[zbus(property)]
    async fn set_fullscreen(&self, value: bool) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Fullscreen(value)).await
    }
    #[zbus(property)]
    fn can_set_fullscreen(&self) -> bool {
        true
    }
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    async fn next(&self) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Next).await
    }
    async fn previous(&self) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Previous).await
    }
    async fn pause(&self) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Pause).await
    }
    async fn play_pause(&self) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Toggle).await
    }
    async fn stop(&self) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Stop).await
    }
    async fn play(&self) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Play).await
    }
    async fn seek(&self, offset: i64) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Seek(offset)).await
    }
    async fn set_position(&self, track_id: ObjectPath<'_>, position: i64) -> zbus::fdo::Result<()> {
        self.0
            .submit(Action::SetPosition {
                track: track_id.as_str().into(),
                position,
            })
            .await
    }
    async fn open_uri(&self, uri: String) -> zbus::fdo::Result<()> {
        let open = normalized_open(vec![uri], false, "auto".into()).await?;
        self.0.submit(Action::Open(open)).await
    }
    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.0.snapshot().status().into()
    }
    #[zbus(property)]
    fn loop_status(&self) -> String {
        self.0.snapshot().loop_status().into()
    }
    #[zbus(property)]
    async fn set_loop_status(&self, status: String) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Loop(status)).await
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        self.0.snapshot().rate
    }
    #[zbus(property)]
    async fn set_rate(&self, rate: f64) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Rate(rate)).await
    }
    #[zbus(property)]
    fn shuffle(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn set_shuffle(&self, shuffle: bool) -> zbus::fdo::Result<()> {
        if shuffle {
            Err(zbus::fdo::Error::NotSupported(
                "shuffle is not supported".into(),
            ))
        } else {
            Ok(())
        }
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        metadata(&self.0.snapshot())
    }
    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.0.snapshot().volume
    }
    #[zbus(property)]
    async fn set_volume(&self, volume: f64) -> zbus::fdo::Result<()> {
        self.0.submit(Action::Volume(volume)).await
    }
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.0.snapshot().position_us
    }
    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        0.25
    }
    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        4.0
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.0.snapshot().can_next()
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.0.snapshot().can_previous()
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        !self.0.snapshot().playlist.is_empty()
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.0.snapshot().loaded
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.0.snapshot().seekable
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        self.0.snapshot().alive
    }

    #[zbus(signal)]
    async fn seeked(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        position: i64,
    ) -> zbus::Result<()>;
}

#[interface(name = "org.jwm.Player")]
impl Native {
    #[zbus(property)]
    fn eva_open_receipts(&self) -> Vec<String> {
        self.0
            .eva_receipts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(serde_json::Value::to_string)
            .collect()
    }

    async fn open_eva(
        &self,
        files: Vec<String>,
        enqueue: bool,
        mode: String,
        request_json: String,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<()> {
        let request = eva::parse(&request_json).map_err(zbus::fdo::Error::InvalidArgs)?;
        let owner = connection
            .unique_name()
            .ok_or_else(|| zbus::fdo::Error::Failed("target_gone".into()))?;
        eva::check_owner(&request, owner.as_str(), std::process::id())
            .map_err(zbus::fdo::Error::Failed)?;
        let open = normalized_open(files, enqueue, mode).await?;
        eva::check_open(&request, &open).map_err(zbus::fdo::Error::InvalidArgs)?;
        self.0.submit(Action::OpenEva { open, request }).await
    }

    /// A read-only projection of mpv's observed playlist, including forwarded
    /// enqueue requests only after mpv has actually reported them.
    #[zbus(property)]
    fn queued_urls(&self) -> Vec<String> {
        let mut bytes = 0usize;
        self.0
            .snapshot()
            .playlist
            .iter()
            .filter_map(|track| {
                let uri = file_uri(&track.path);
                bytes = bytes.saturating_add(uri.len());
                (bytes <= 128 * 1024).then_some(uri)
            })
            .take(512)
            .collect()
    }
    async fn open(&self, files: Vec<String>, enqueue: bool, mode: String) -> zbus::fdo::Result<()> {
        let open = normalized_open(files, enqueue, mode).await?;
        let raise = open.files.is_empty() || !open.enqueue;
        self.0.submit(Action::Open(open)).await?;
        if raise && self.1 {
            focus_jwm_window().await;
        }
        Ok(())
    }
}

fn player_properties(state: &State) -> HashMap<String, OwnedValue> {
    HashMap::from([
        ("PlaybackStatus".into(), string(state.status())),
        ("LoopStatus".into(), string(state.loop_status())),
        ("Rate".into(), state.rate.into()),
        ("Volume".into(), state.volume.into()),
        ("Metadata".into(), metadata(state).into()),
        ("CanGoNext".into(), state.can_next().into()),
        ("CanGoPrevious".into(), state.can_previous().into()),
        ("CanPlay".into(), (!state.playlist.is_empty()).into()),
        ("CanPause".into(), state.loaded.into()),
        ("CanSeek".into(), state.seekable.into()),
        ("CanControl".into(), state.alive.into()),
    ])
}

async fn changed(
    connection: &Connection,
    interface: &str,
    properties: HashMap<String, OwnedValue>,
) -> Result<(), String> {
    tokio::time::timeout(
        Duration::from_secs(2),
        connection.emit_signal(
            None::<&str>,
            PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(interface, properties, Vec::<String>::new()),
        ),
    )
    .await
    .map_err(|_| "D-Bus property notification timed out".to_string())?
    .map_err(|e| e.to_string())
}

pub async fn run(open: Open, request: Option<serde_json::Value>) -> Result<(), String> {
    run_on_with_eva(open, None, false, request).await
}

#[cfg(test)]
async fn run_on(open: Open, address: Option<&str>, headless: bool) -> Result<(), String> {
    run_on_with_eva(open, address, headless, None).await
}

async fn run_on_with_eva(
    open: Open,
    address: Option<&str>,
    headless: bool,
    request: Option<serde_json::Value>,
) -> Result<(), String> {
    if let Some(request) = &request {
        eva::check_open(request, &open)?;
    }
    let (controller, worker) = Controller::new();
    let builder = match address {
        Some(address) => zbus::connection::Builder::address(address),
        None => zbus::connection::Builder::session(),
    }
    .map_err(|e| format!("D-Bus session unavailable: {e}"))?;
    let builder = builder
        .serve_at(PATH, Root(controller.clone(), !headless))
        .map_err(|e| e.to_string())?
        .serve_at(PATH, Player(controller.clone()))
        .map_err(|e| e.to_string())?
        .serve_at(PATH, Native(controller.clone(), !headless))
        .map_err(|e| e.to_string())?;
    let connection = tokio::time::timeout(Duration::from_secs(5), builder.build())
        .await
        .map_err(|_| "connecting to D-Bus timed out".to_string())?
        .map_err(|e| format!("D-Bus session unavailable: {e}"))?;
    let ownership = tokio::time::timeout(
        Duration::from_secs(5),
        connection.request_name_with_flags(NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into()),
    )
    .await
    .map_err(|_| "claiming player service timed out".to_string())?;
    match ownership {
        Ok(_) => {}
        Err(zbus::Error::NameTaken) => {
            // Only the bus-name owner starts a decoder. Simultaneous first
            // launches therefore become one owner plus forwarded requests.
            if let Some(request) = request {
                return tokio::time::timeout(Duration::from_secs(15), async {
                    let bus = zbus::fdo::DBusProxy::new(&connection)
                        .await
                        .map_err(|e| e.to_string())?;
                    let owner = bus
                        .get_name_owner(NAME.try_into().map_err(|_| "invalid player name")?)
                        .await
                        .map_err(|_| "target_gone")?;
                    let uid = bus
                        .get_connection_unix_user(owner.clone().into())
                        .await
                        .map_err(|_| "target_gone")?;
                    let pid = bus
                        .get_connection_unix_process_id(owner.clone().into())
                        .await
                        .map_err(|_| "target_gone")?;
                    if uid != unsafe { libc::geteuid() } {
                        return Err("target_gone".to_string());
                    }
                    eva::check_owner(&request, owner.as_str(), pid)?;
                    connection
                        .call_method(
                            Some(owner.as_str()),
                            PATH,
                            Some(NATIVE),
                            "OpenEva",
                            &(
                                open.files,
                                open.enqueue,
                                open.mode.as_str(),
                                request.to_string(),
                            ),
                        )
                        .await
                        .map_err(|e| format!("existing player could not open the files: {e}"))?;
                    Ok(())
                })
                .await
                .map_err(|_| "existing player did not answer".to_string())?;
            }
            tokio::time::timeout(
                Duration::from_secs(15),
                connection.call_method(
                    Some(NAME),
                    PATH,
                    Some(NATIVE),
                    "Open",
                    &(open.files, open.enqueue, open.mode.as_str()),
                ),
            )
            .await
            .map_err(|_| "existing player did not answer".to_string())?
            .map_err(|e| format!("existing player could not open the files: {e}"))?;
            return Ok(());
        }
        Err(error) => return Err(error.to_string()),
    }
    let mut done = if let Some(request) = request {
        let owner = connection.unique_name().ok_or("target_gone")?;
        eva::check_owner(&request, owner.as_str(), std::process::id())?;
        worker.spawn_eva(open, headless, Some(request))
    } else {
        worker.spawn_inner(open, headless)
    };
    let mut previous = controller.snapshot();
    let mut previous_properties = player_properties(&previous);
    let mut ticker = tokio::time::interval(Duration::from_millis(150));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = &mut done => {
                return result.map_err(|_| "playback worker stopped unexpectedly".to_string())?;
            }
            _ = ticker.tick() => {
                if connection.is_closed() {
                    let _ = controller.submit(Action::Quit).await;
                    let _ = tokio::time::timeout(Duration::from_secs(4), &mut done).await;
                    return Err("D-Bus session disconnected".into());
                }
                let state = controller.snapshot();
                let properties = player_properties(&state);
                if properties != previous_properties {
                    if let Err(error) = changed(&connection, PLAYER, properties).await {
                        let _ = controller.submit(Action::Quit).await;
                        let _ = tokio::time::timeout(Duration::from_secs(4), &mut done).await;
                        return Err(error);
                    }
                    previous_properties = player_properties(&state);
                }
                if state.fullscreen != previous.fullscreen {
                    let _ = changed(&connection, ROOT, HashMap::from([("Fullscreen".into(), state.fullscreen.into())])).await;
                }
                if state.seek_serial != previous.seek_serial {
                    let _ = tokio::time::timeout(Duration::from_secs(2), connection.emit_signal(None::<&str>, PATH, PLAYER, "Seeked", &(state.position_us,))).await;
                }
                if state.error_serial != previous.error_serial
                    && let Some(error) = state.error.clone() {
                        eprintln!("jwm-player: {error}");
                        if !headless {
                            tokio::spawn(async move { super::notify_error(&error).await; });
                        }
                }
                previous = state;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_has_object_path_track_id_and_microsecond_length() {
        let state = State {
            track_id: Some(17),
            duration_us: Some(2_500_000),
            path: "/tmp/歌曲 #?.flac".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            ..State::default()
        };
        let map = metadata(&state);
        let path = <&ObjectPath>::try_from(map.get("mpris:trackid").unwrap()).unwrap();
        assert_eq!(path.as_str(), "/org/jwm/Player/track_17");
        assert_eq!(
            i64::try_from(map.get("mpris:length").unwrap()).unwrap(),
            2_500_000
        );
        assert_eq!(
            <&str>::try_from(map.get("xesam:url").unwrap()).unwrap(),
            file_uri(&state.path)
        );
        assert!(!player_properties(&state).contains_key("Position"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_private_bus_single_instance_and_mpris_control() {
        if std::env::var_os("RUN_JWM_MPV_TESTS").is_none() {
            return;
        }
        use std::io::{BufRead, BufReader};
        use std::process::{Child, Command, Stdio};
        struct Bus(Child);
        impl Drop for Bus {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut bus = Bus(Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap());
        let mut address = String::new();
        BufReader::new(bus.0.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim().to_owned();
        let wave = std::env::temp_dir().join(format!("jwm-mpris-{}.wav", std::process::id()));
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=10"
                ])
                .arg(&wave)
                .status()
                .unwrap()
                .success()
        );
        let open = Open {
            files: vec![wave.to_str().unwrap().into()],
            enqueue: true,
            mode: Mode::Music,
        };
        let first_address = address.clone();
        let first_open = open.clone();
        let first =
            tokio::spawn(async move { run_on(first_open, Some(&first_address), true).await });
        let second_address = address.clone();
        let second = tokio::spawn(async move { run_on(open, Some(&second_address), true).await });
        let client = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            let ready = property(&client, "CanGoNext")
                .await
                .ok()
                .and_then(|v| bool::try_from(v).ok())
                .unwrap_or(false);
            if ready
                && property(&client, "PlaybackStatus")
                    .await
                    .ok()
                    .and_then(|v| String::try_from(v).ok())
                    .as_deref()
                    == Some("Playing")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "single-instance player failed to load both queued requests"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        // Exactly one launch stays alive. The other forwarded its request
        // and exited without spawning a second long-lived decoder.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_ne!(first.is_finished(), second.is_finished());
        client
            .call_method(Some(NAME), PATH, Some(PLAYER), "Pause", &())
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if String::try_from(property(&client, "PlaybackStatus").await.unwrap()).unwrap()
                == "Paused"
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let metadata: HashMap<String, OwnedValue> = property(&client, "Metadata")
            .await
            .unwrap()
            .try_into()
            .unwrap();
        client
            .call_method(Some(NAME), PATH, Some(PLAYER), "Play", &())
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if String::try_from(property(&client, "PlaybackStatus").await.unwrap()).unwrap()
                == "Playing"
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "explicit Play must resume rather than toggle an already playing player"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(metadata.contains_key("mpris:trackid"));
        assert!(metadata.contains_key("mpris:length"));
        let properties = zbus::fdo::PropertiesProxy::builder(&client)
            .destination(NAME)
            .unwrap()
            .path(PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        let queued: Vec<String> = properties
            .get(NATIVE.try_into().unwrap(), "QueuedUrls")
            .await
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            queued
                .iter()
                .filter(|uri| **uri == file_uri(wave.to_str().unwrap()))
                .count(),
            2,
            "queue proof must describe both actual mpv entries after the second process forwards its enqueue"
        );
        // Same-resource concurrent requests must each own a new native entry;
        // a pre-existing matching URL and the other request's receipt prove
        // nothing about this operation.
        let bus_proxy = zbus::fdo::DBusProxy::new(&client).await.unwrap();
        let owner = bus_proxy
            .get_name_owner(NAME.try_into().unwrap())
            .await
            .unwrap();
        let pid = bus_proxy
            .get_connection_unix_process_id(owner.clone().into())
            .await
            .unwrap();
        let request = |operation: &str, sequence: u64, enqueue: bool| {
            serde_json::json!({
                "operation_id":operation,"wm_instance_id":"native-test-wm","sequence":sequence,
                "resource_uri":file_uri(wave.to_str().unwrap()),"enqueue":enqueue,
                "target_player":"jwm","target_bus_owner":owner.as_str(),"target_pid":pid
            })
        };
        let a = request("enqueue-a", 1, true);
        let b = request("enqueue-b", 2, true);
        let files = vec![wave.to_str().unwrap().to_owned()];
        let call = |request: serde_json::Value| {
            let client = client.clone();
            let files = files.clone();
            let owner = owner.clone();
            async move {
                client
                    .call_method(
                        Some(owner.as_str()),
                        PATH,
                        Some(NATIVE),
                        "OpenEva",
                        &(files, true, "music", request.to_string()),
                    )
                    .await
            }
        };
        let forwarded = Open {
            files: files.clone(),
            enqueue: true,
            mode: Mode::Music,
        };
        let (a_result, b_result) = tokio::join!(
            run_on_with_eva(forwarded.clone(), Some(&address), true, Some(a.clone())),
            run_on_with_eva(forwarded, Some(&address), true, Some(b.clone()))
        );
        a_result.unwrap();
        b_result.unwrap();
        let receipts: Vec<String> = properties
            .get(NATIVE.try_into().unwrap(), "EvaOpenReceipts")
            .await
            .unwrap()
            .try_into()
            .unwrap();
        let receipts: Vec<serde_json::Value> = receipts
            .iter()
            .map(|receipt| serde_json::from_str(receipt).unwrap())
            .collect();
        assert_eq!(receipts.len(), 2);
        for expected in [&a, &b] {
            let receipt = receipts
                .iter()
                .find(|receipt| eva::same_operation(receipt, expected))
                .unwrap();
            assert_eq!(receipt["verified"], true, "{receipt}");
            assert_eq!(receipt["mpv_command_ack"], true);
            assert!(receipt["player_request_id"].as_u64().unwrap() > 0);
            assert!(receipt["entry_id"].as_u64().unwrap() > 0);
            assert_eq!(receipt["resource_uri"], expected["resource_uri"]);
            assert_eq!(receipt["completion"], "resource_queued_observed");
        }
        assert_ne!(receipts[0]["entry_id"], receipts[1]["entry_id"]);
        assert_ne!(
            receipts[0]["player_request_id"],
            receipts[1]["player_request_id"]
        );
        call(a).await.unwrap();
        let mut wrong_owner = b.clone();
        wrong_owner["sequence"] = serde_json::json!(3);
        wrong_owner["target_bus_owner"] = serde_json::json!(":1.999999");
        assert!(call(wrong_owner).await.is_err());
        let queued: Vec<String> = properties
            .get(NATIVE.try_into().unwrap(), "QueuedUrls")
            .await
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            queued.len(),
            4,
            "replay and replaced-owner requests must add zero entries"
        );
        let replacement = request("replace-c", 4, false);
        client
            .call_method(
                Some(owner.as_str()),
                PATH,
                Some(NATIVE),
                "OpenEva",
                &(files, false, "music", replacement.to_string()),
            )
            .await
            .unwrap();
        let receipts: Vec<String> = properties
            .get(NATIVE.try_into().unwrap(), "EvaOpenReceipts")
            .await
            .unwrap()
            .try_into()
            .unwrap();
        let replacement: serde_json::Value = receipts
            .iter()
            .map(|text| serde_json::from_str::<serde_json::Value>(text).unwrap())
            .find(|receipt| receipt["operation_id"] == "replace-c")
            .unwrap();
        assert_eq!(replacement["verified"], true, "{replacement}");
        assert_eq!(replacement["status"], "Playing");
        assert_eq!(replacement["track_url"], file_uri(wave.to_str().unwrap()));
        assert_eq!(replacement["completion"], "resource_playing_observed");
        let remote = client
            .call_method(
                Some(NAME),
                PATH,
                Some(PLAYER),
                "OpenUri",
                &("https://example.invalid/video",),
            )
            .await;
        assert!(remote.is_err());
        client
            .call_method(Some(NAME), PATH, Some(ROOT), "Quit", &())
            .await
            .unwrap();
        for task in [first, second] {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        // No previous owner: the same metadata accompanies the first native
        // mpv load, and the new owner's receipt remains readable afterward.
        let mut initial = request("new-player-d", 5, false);
        initial["target_bus_owner"] = serde_json::Value::Null;
        initial["target_pid"] = serde_json::Value::Null;
        let new_open = Open {
            files: vec![wave.to_str().unwrap().into()],
            enqueue: false,
            mode: Mode::Music,
        };
        let new_address = address.clone();
        let new_player = tokio::spawn(async move {
            run_on_with_eva(new_open, Some(&new_address), true, Some(initial)).await
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            let receipt = properties
                .get(NATIVE.try_into().unwrap(), "EvaOpenReceipts")
                .await
                .ok()
                .and_then(|value| Vec::<String>::try_from(value).ok())
                .and_then(|receipts| {
                    receipts
                        .into_iter()
                        .filter_map(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                        .find(|receipt| receipt["operation_id"] == "new-player-d")
                });
            if let Some(receipt) = receipt {
                assert_eq!(receipt["verified"], true, "{receipt}");
                assert_eq!(receipt["status"], "Playing");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "new owner failed to expose initial causal receipt"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        client
            .call_method(Some(NAME), PATH, Some(ROOT), "Quit", &())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), new_player)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let _ = std::fs::remove_file(wave);
    }

    async fn property(connection: &Connection, name: &str) -> zbus::Result<OwnedValue> {
        let reply = connection
            .call_method(
                Some(NAME),
                PATH,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(PLAYER, name),
            )
            .await?;
        reply.body().deserialize()
    }
}
