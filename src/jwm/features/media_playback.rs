//! Native library entry points; decoding and D-Bus stay in jwm-player.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::backend::api::{Backend, ToastNotification};
use crate::config::CONFIG;
use crate::jwm::{Jwm, WMArgEnum};

use super::connectivity::{BackgroundJob, job_in_flight};
use super::media_library::{self, MediaKind};
use super::system_ui::SystemUiState;
use super::toggles::SystemUiPointerGrab;

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn find_program(
    name: &str,
    sibling: bool,
    environment: &crate::child_environment::ChildEnvironment,
) -> Option<PathBuf> {
    if sibling && let Ok(exe) = std::env::current_exe() {
        let path = exe.with_file_name(name);
        if executable(&path) {
            return Some(path);
        }
    }
    std::env::split_paths(&environment.var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|path| executable(path))
}

fn prepare_player(
    mut args: Vec<String>,
    environment: crate::child_environment::ChildEnvironment,
) -> Result<Command, String> {
    let helper = find_program("jwm-player", true, &environment)
        .ok_or("jwm-player is not installed; rebuild/install JWM's player helper")?;
    if find_program("mpv", false, &environment).is_none() {
        return Err("mpv is not installed; install the mpv package to play music and video".into());
    }
    // Filesystem work stays on this existing bounded worker. Use the same
    // canonical UTF-8 path that the helper publishes in MPRIS xesam:url, so
    // symlink spellings cannot make a different resource look verified.
    let first_file = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or("missing player file boundary")?
        + 1;
    for file in &mut args[first_file..] {
        let canonical =
            std::fs::canonicalize(&*file).map_err(|_| "media file cannot be resolved")?;
        if !canonical.is_file() {
            return Err("media path is not a regular file".into());
        }
        *file = canonical
            .into_os_string()
            .into_string()
            .map_err(|_| "media file path is not UTF-8")?;
    }
    let mut command = Command::new(helper);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    environment.apply(&mut command);
    command.env_remove("JWM_EVA_MEDIA_REQUEST");
    super::external_command::unblock_sigchld_in_child(&mut command);
    // Return the prepared command, not an unsupervised child: an interrupted
    // scan/restart can discard this value without leaking a process. The
    // event thread performs the ordinary application spawn and immediately
    // registers its exact Child handle with JWM's existing supervisor.
    Ok(command)
}

impl Jwm {
    pub(crate) fn music_player(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.open_media_library(backend, MediaKind::Music)
    }

    pub(crate) fn video_player(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.open_media_library(backend, MediaKind::Video)
    }

    fn open_media_library(
        &mut self,
        backend: &mut dyn Backend,
        kind: MediaKind,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mine = match kind {
            MediaKind::Music => SystemUiState::is_music_library,
            MediaKind::Video => SystemUiState::is_video_library,
        };
        if self.toggle_off_system_ui(backend, mine) {
            return Ok(());
        }
        self.prepare_system_ui(backend, "the media library", SystemUiPointerGrab::Buttons)?;
        self.features.system_ui_return_to_hub = false;
        self.features.system_ui = self.media_library_state(kind, false);
        self.sync_system_ui(backend);
        Ok(())
    }

    pub(crate) fn media_library_state(&mut self, kind: MediaKind, force: bool) -> SystemUiState {
        self.poll_media_library_jobs();
        let index = kind.index();
        let stale = self.features.media_library.refreshed_at[index]
            .is_none_or(|time| time.elapsed() >= media_library::CACHE_AGE);
        if (force || stale) && !job_in_flight(self.features.media_library.scans[index].as_ref()) {
            let config = CONFIG.load();
            let directory = match kind {
                MediaKind::Music => config.behavior().audio_recording_output_dir.clone(),
                MediaKind::Video => config.behavior().recording_output_dir.clone(),
            };
            let job = BackgroundJob::spawn(move || {
                media_library::scan(kind, &media_library::roots(kind, &directory))
            });
            self.features.media_library.scans[index] =
                job.started().then(|| self.track_background_job(job));
        }
        let scanning = job_in_flight(self.features.media_library.scans[index].as_ref());
        SystemUiState::media_library(
            kind,
            self.features.media_library.catalogs[index].as_ref(),
            scanning,
        )
    }

    pub(crate) fn refresh_media_library(&mut self, backend: &mut dyn Backend) {
        let Some(kind) = self.features.system_ui.media_library_kind() else {
            return;
        };
        let state = self.media_library_state(kind, true);
        self.features.system_ui.refresh_media_library(state);
        self.sync_system_ui(backend);
    }

    pub(crate) fn poll_media_library_jobs(&mut self) {
        for kind in MediaKind::ALL {
            let index = kind.index();
            let Some(catalog) = self.features.media_library.scans[index]
                .as_ref()
                .and_then(BackgroundJob::take)
            else {
                continue;
            };
            self.features.media_library.scans[index] = None;
            self.features.media_library.refreshed_at[index] = Some(Instant::now());
            if self.features.system_ui.media_library_kind() == Some(kind) {
                let next = SystemUiState::media_library(kind, Some(&catalog), false);
                self.features.system_ui.refresh_media_library(next);
                self.mark_system_ui_dirty();
            }
            self.features.media_library.catalogs[index] = Some(catalog);
        }
    }

    pub(crate) fn queue_media_playback(
        &mut self,
        backend: &dyn Backend,
        kind: Option<MediaKind>,
        paths: Vec<String>,
        enqueue: bool,
    ) -> Result<(), String> {
        // Keep a completed result until the render tick consumes it as well:
        // replacing it here could silently discard an earlier launch error.
        if self.features.media_library.launch.is_some() {
            return Err("a player launch is already pending; try again shortly".into());
        }
        let args = media_library::player_arguments(kind, &paths, enqueue)?;
        let environment = backend.child_environment();
        let job = BackgroundJob::spawn(move || prepare_player(args, environment));
        if !job.started() {
            return Err("could not start the player worker; try again shortly".into());
        }
        self.eva_note_media_launch_sequence(job.eva_identity(), job.eva_keepalive());
        self.features.media_library.launch = Some(self.track_background_job(job));
        Ok(())
    }

    pub(crate) fn poll_media_launch_job(&mut self, backend: &mut dyn Backend) {
        let completion_id = self
            .features
            .media_library
            .launch
            .as_ref()
            .map(BackgroundJob::eva_identity);
        let Some(result) = self
            .features
            .media_library
            .launch
            .as_ref()
            .and_then(BackgroundJob::take)
        else {
            return;
        };
        self.features.media_library.launch = None;
        self.eva_tick(backend);
        match result {
            Ok(mut command) => {
                if let Some(id) = completion_id {
                    let mut args = command.get_args();
                    if args.by_ref().any(|arg| arg == "--")
                        && let Some(path) = args.next().and_then(|path| path.to_str())
                    {
                        let mut uri = String::from("file://");
                        for byte in path.bytes() {
                            if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
                                uri.push(byte as char);
                            } else {
                                use std::fmt::Write;
                                let _ = write!(uri, "%{byte:02X}");
                            }
                        }
                        match self.eva_media_launch_prepared(id, &uri) {
                            Ok(Some(request)) => {
                                command.env("JWM_EVA_MEDIA_REQUEST", request.to_string());
                            }
                            Ok(None) => {}
                            Err(error) => {
                                self.eva_control_completion("media_launch",id,Some(false),serde_json::json!({"error":error,"completion":"cancelled_before_player_spawn"}));
                                return;
                            }
                        }
                    }
                }
                match command.spawn() {
                    Ok(child) => {
                        let pid = child.id();
                        self.supervise_transient_child(child);
                        if let Some(id) = completion_id {
                            self.eva_control_completion("media_launch",id,Some(true),serde_json::json!({"pid":pid,"completion":"player_process_spawned","playback_verified":false}));
                        }
                    }
                    Err(error) => {
                        if let Some(id) = completion_id {
                            self.eva_control_completion(
                                "media_launch",
                                id,
                                Some(false),
                                serde_json::json!({"error":"player_spawn_failed"}),
                            );
                        }
                        self.media_playback_error(
                            backend,
                            format!("could not start JWM Player: {error}"),
                        );
                    }
                }
            }
            Err(error) => {
                if let Some(id) = completion_id {
                    self.eva_control_completion(
                        "media_launch",
                        id,
                        Some(false),
                        serde_json::json!({"error":"player_prepare_failed"}),
                    );
                }
                self.media_playback_error(backend, error);
            }
        }
    }

    fn media_playback_error(&mut self, backend: &mut dyn Backend, body: String) {
        self.push_system_toast(
            backend,
            ToastNotification {
                title: "JWM Player unavailable".into(),
                body,
                urgency: 2,
                timeout_ms: 8000,
                ..Default::default()
            },
        );
    }

    pub(crate) fn play_selected_media(
        &mut self,
        backend: &mut dyn Backend,
        enqueue: bool,
        all: bool,
    ) {
        let Some(kind) = self.features.system_ui.media_library_kind() else {
            return;
        };
        let paths = self.features.system_ui.media_library_paths(all);
        if paths.is_empty() {
            return;
        }
        match self.queue_media_playback(backend, Some(kind), paths, enqueue) {
            Ok(()) => self.close_system_ui(backend),
            Err(error) => self.media_playback_error(backend, error),
        }
    }
}
