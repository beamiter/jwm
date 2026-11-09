//! JWM-to-JWM remote desktop helper for trusted X11 LANs.

use clap::{Args, Parser, Subcommand};
use jwm::remote::client::{ClientOptions, run_client_reconnecting};
use jwm::remote::clipboard_files::{parse_size, staging_root};
use jwm::remote::clipboard_policy::{ClipboardDirection, ClipboardMode};
use jwm::remote::clipboard_staging_budget::{self, Status as StagingBudgetStatus};
use jwm::remote::host::{HostOptions, run_host};
use jwm::remote::key::{KeySource, generate_key_file};
use jwm::remote::messages::FrameLimits;
use jwm::remote::profiles::{Connection, Profile, Store, Viewer};
use jwm::remote::session_env;
use jwm::remote::ssh::{SshOptions, run_ssh};
use jwm::remote::usability::{
    default_key_path, key_metadata, normalize_address, normalize_listen_address,
};
use jwm::remote::viewer_backend::ViewerBackend;
use jwm::remote::window_position::WindowPosition;
use jwm::remote::window_size::WindowSize;
use jwm::remote::x11_capture::{CaptureArea, CaptureRegion, CaptureSource, ScalingPolicy};
use jwm::remote::x11_viewer::DEFAULT_ESCAPE_KEY;
use jwm::remote::{RemoteResult, messages::APPLICATION_VERSION, protocol::TRANSPORT_VERSION};
use std::path::{Path, PathBuf};

const DEFAULT_JPEG_QUALITY_FLOOR: u8 = 40;
/// Default `--clipboard-file-limit`; matches `clipboard_sync::DEFAULT_FILE_LIMIT`.
const DEFAULT_CLIPBOARD_FILE_LIMIT: &str = "2G";

/// Decoded-frame safety limits, independent of the viewport width policy.
#[derive(Debug, Clone, Args)]
struct FrameLimitArgs {
    /// Maximum decoded frame width (1..=16384; default 16384).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=16384))]
    frame_max_width: Option<u16>,
    /// Maximum decoded frame height (1..=16384; default 16384).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=16384))]
    frame_max_height: Option<u16>,
    /// Maximum decoded frame pixels (1..=67108864; default 67108864).
    /// SSH applies these limits to both the viewer and remote host.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=67108864))]
    frame_max_pixels: Option<u64>,
}

impl FrameLimitArgs {
    fn limits(&self) -> FrameLimits {
        let defaults = FrameLimits::default();
        FrameLimits {
            max_width: self.frame_max_width.unwrap_or(defaults.max_width),
            max_height: self.frame_max_height.unwrap_or(defaults.max_height),
            max_pixels: self.frame_max_pixels.unwrap_or(defaults.max_pixels),
        }
    }
}

/// Additional immutable source grants; never inferred from viewer requests.
#[derive(Debug, Clone, Default, Args)]
struct ShareArgs {
    /// Also grant this RandR monitor (repeatable; max 32 sources total).
    #[arg(long, value_name = "NAME", value_parser = parse_monitor)]
    share_monitor: Vec<String>,
    /// Also grant this fixed root rectangle (repeatable).
    #[arg(long, value_name = "WxH+X+Y", value_parser = parse_region)]
    share_region: Vec<CaptureRegion>,
    /// Also grant the entire root, including every monitor.
    #[arg(long)]
    share_root: bool,
}

impl ShareArgs {
    fn is_empty(&self) -> bool {
        self.share_monitor.is_empty() && self.share_region.is_empty() && !self.share_root
    }

    fn areas(&self, initial: &CaptureArea) -> RemoteResult<Vec<CaptureArea>> {
        // Bound raw repetitions too, before building the unique catalog.
        if self.share_monitor.len() + self.share_region.len() + usize::from(self.share_root) > 31 {
            return Err(profile_error("at most 31 additional source grants are allowed").into());
        }
        let mut result = Vec::new();
        for area in self
            .share_monitor
            .iter()
            .cloned()
            .map(CaptureArea::Monitor)
            .chain(self.share_region.iter().copied().map(CaptureArea::Region))
            .chain(self.share_root.then_some(CaptureArea::Root))
        {
            if &area != initial && !result.contains(&area) {
                result.push(area);
            }
        }
        Ok(result)
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "jwm-remote",
    version,
    about = "JWM-to-JWM remote viewing and control for trusted X11 LANs",
    after_help = "Quickest, if you can SSH to the other machine:\n  jwm-remote ssh [USER@]HOST --grab-input\nIt starts a loopback-only host there for one session, with a fresh key, through SSH.\n\nOr set up a LAN host yourself. Start here: jwm-remote doctor\nCreate a key explicitly: jwm-remote keygen (never overwrites; see keygen --help).\nShare the same private key securely with your other machine.\nHost: jwm-remote host --listen 0.0.0.0 --allow-lan\nViewer: jwm-remote connect HOST --view-only\nInput needs host --allow-input; clipboard requires explicit consent on both ends.\nDefault key: $XDG_CONFIG_HOME/jwm/remote.key, otherwise $HOME/.config/jwm/remote.key.\nNo command silently creates a key, opens LAN access, or enables clipboard sharing."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Also report a failure as a desktop notification.
    ///
    /// For launching from a keyboard shortcut, where nobody sees the terminal.
    #[arg(long, global = true)]
    notify: bool,

    /// Emit one content-free terminal JSON record to stderr (human errors are unchanged).
    #[arg(long, global = true)]
    diagnostics_json: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect or explicitly cap received-file staging on this machine only.
    #[command(
        after_help = "No cap is enabled by default. SIZE is a positive byte count or binary K/M/G/T size, such as 512M or 1.5G.\nThis is a logical archive-byte admission cap shared by cooperating processes using the same private cache root, not a disk quota.\nstatus never creates or deletes files; set-limit never evicts copies; disable preserves accounting and files.\nNo setting is forwarded over SSH or saved in a connection profile."
    )]
    ClipboardStaging {
        #[command(subcommand)]
        action: ClipboardStagingAction,
    },
    /// Developer-only offline synthetic tile codec replay; emits JSON, opens no display.
    ReplayBenchmark {
        #[arg(long, default_value_t = 320, value_parser = clap::value_parser!(u16).range(1..=1920))]
        width: u16,
        #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u16).range(1..=1080))]
        height: u16,
        /// Measured frames per scene; total generated pixels are also bounded.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=300))]
        frames: u32,
        /// Separate warmup frames per scene; measured codec state starts fresh.
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(0..=30))]
        warmup: u32,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value_t = 75, value_parser = clap::value_parser!(u8).range(1..=100))]
        quality: u8,
    },
    /// Export exact offline replay RGB fixtures and tile baseline to a NEW directory.
    ReplayFixture {
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 320, value_parser = clap::value_parser!(u16).range(1..=1920))]
        width: u16,
        #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u16).range(1..=1080))]
        height: u16,
        #[arg(long, default_value_t = 12, value_parser = clap::value_parser!(u32).range(1..=300))]
        frames: u32,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value_t = 75, value_parser = clap::value_parser!(u8).range(1..=100))]
        quality: u8,
    },
    /// Save, inspect and explicitly run named connection settings (no credentials).
    Profile {
        /// Existing private directory. Default: JWM config directory/remote-profiles.
        #[arg(long, global = true, value_name = "DIRECTORY")]
        directory: Option<PathBuf>,
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// Generate a new private pre-shared key file (never overwrites).
    #[command(
        after_help = "First create the private parent directory yourself if needed. For the HOME default:\n  mkdir -p -m 700 \"$HOME/.config/jwm\"\n  jwm-remote keygen\nIf XDG_CONFIG_HOME is set, use \"$XDG_CONFIG_HOME/jwm\" instead.\nExisting directories are not repaired or chmodded. Copy the generated key only through a secure channel."
    )]
    Keygen {
        /// Default: the private JWM key path. Parent directory must already exist.
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,
    },
    /// Offline checks only: no X11/network connection and no key-content reads.
    Doctor {
        /// Private key; defaults to the JWM key path shown in top-level help.
        #[arg(long, value_name = "PATH")]
        key_file: Option<PathBuf>,
        /// Local X11 display (defaults to DISPLAY); presence is not connectivity.
        #[arg(long, value_name = "DISPLAY")]
        display: Option<String>,
    },
    /// Share the current JWM X11 desktop.
    Host {
        /// Local host IP/name, optionally with port (default 48221).
        ///
        /// Bare IPv6 uses the default port; use [IPv6]:PORT for another port.
        /// Non-loopback addresses still require --allow-lan. Explicit port 0
        /// asks the OS for a free port; use the printed listener port to connect.
        #[arg(
            long,
            default_value = "127.0.0.1:48221",
            value_name = "LOCAL_HOST[:PORT]",
            value_parser = normalize_listen_address
        )]
        listen: String,

        /// Private key; defaults to the JWM key path shown in top-level help.
        #[arg(long, value_name = "PATH", conflicts_with = "key_stdin")]
        key_file: Option<PathBuf>,

        /// Read one key line from standard input, and stop when it closes.
        ///
        /// This is how `jwm-remote ssh` starts a host: the key never touches
        /// a file, and the host ends with the SSH session that started it.
        #[arg(long)]
        key_stdin: bool,

        /// X11 display to share (defaults to DISPLAY).
        ///
        /// `auto` uses the display of your running JWM session, which is what
        /// a host started over SSH needs; it is also used when DISPLAY is unset.
        #[arg(long, value_name = "DISPLAY|auto")]
        display: Option<String>,

        /// Interactive upper frame-rate limit; unchanged areas are still suppressed.
        #[arg(long, default_value = "30", value_parser = parse_fps)]
        fps: u16,

        /// Adaptive quality ceiling; the default floor stays 40 for congestion recovery.
        #[arg(long, default_value = "85", value_parser = parse_quality)]
        jpeg_quality: u8,

        /// Lowest quality used by ACK-driven adaptation (defaults to min(40, quality)).
        #[arg(
            long,
            value_parser = parse_quality,
            conflicts_with = "fixed_jpeg_quality"
        )]
        jpeg_quality_floor: Option<u8>,

        /// Disable ACK-driven quality adaptation and always use --jpeg-quality.
        #[arg(long)]
        fixed_jpeg_quality: bool,

        /// Ceiling on encoded width; default 0 fits the viewer up to native width.
        ///
        /// A viewer reports its window size and the host narrows below this
        /// automatically. A peer can never raise it.
        #[arg(long, default_value = "0", value_parser = parse_max_width)]
        max_width: u16,

        /// auto prefers JWM's Composite overlay and falls back to root.
        #[arg(long, default_value = "auto", value_parser = parse_capture_source)]
        capture_source: CaptureSource,

        /// Where frames are shrunk to the viewer's size: auto, gpu or cpu.
        ///
        /// gpu uses XRender in the X server and reads back only what is sent.
        /// auto does that except on NVIDIA's proprietary driver, where such
        /// reads can return a half-drawn frame (a flash of bare wallpaper or
        /// blocky noise); there it reads the screen directly and scales on
        /// the CPU.
        #[arg(long, default_value = "auto", value_parser = parse_scaling)]
        scaling: ScalingPolicy,

        /// Share one RandR monitor instead of the whole root, by name.
        ///
        /// The root spans every monitor, so on a dual 1920x1080 desk it is
        /// 3840x1080 and each display arrives at less than half the requested
        /// width. Re-resolved by name when the layout changes.
        #[arg(long, value_name = "NAME", conflicts_with = "region", value_parser = parse_monitor)]
        monitor: Option<String>,

        /// View one opaque X11 window instance; requires --once, without input or clipboard.
        #[arg(long, value_name = "XID", value_parser = parse_window_id, conflicts_with_all = ["monitor", "region", "share_monitor", "share_region", "share_root", "allow_input", "allow_clipboard"], requires = "once")]
        window: Option<u32>,

        #[command(flatten)]
        shares: ShareArgs,

        /// Share a fixed rectangle of the root, as WxH+X+Y.
        #[arg(long, value_name = "WxH+X+Y", value_parser = parse_region)]
        region: Option<CaptureRegion>,

        /// Permit binding beyond loopback.
        ///
        /// Traffic is encrypted and authenticated, but without forward secrecy:
        /// a leaked key file decrypts recorded sessions.
        #[arg(long)]
        allow_lan: bool,

        /// Permit the authenticated peer to inject keyboard and pointer input.
        #[arg(long)]
        allow_input: bool,

        /// Permit one live window instance to resize through native JWM authority.
        #[arg(long, requires_all = ["window", "once"])]
        allow_window_resize: bool,

        /// Select a live Wayland toplevel locally on one persistent connection.
        /// Read-only source; requires a remote-wayland build and one session.
        #[arg(long, requires = "once", conflicts_with_all = ["display", "window", "monitor", "region", "share_monitor", "share_region", "share_root", "allow_input", "allow_clipboard", "allow_window_resize", "key_stdin", "capture_source"])]
        wayland_select: bool,

        /// Permit a single explicitly requested, scoped Wayland keyboard lease.
        #[arg(long, requires_all = ["wayland_select", "once"], conflicts_with_all = ["allow_input", "allow_clipboard", "allow_window_resize"])]
        allow_wayland_keyboard: bool,

        /// Permit one scoped Wayland pointer lease (motion or atomic left click).
        #[arg(long, requires_all = ["wayland_select", "once"], conflicts_with_all = ["allow_wayland_keyboard", "allow_input", "allow_clipboard", "allow_window_resize"])]
        allow_wayland_pointer: bool,

        /// Experimentally encode exact bounded vertical screen shifts as copy plus changed pixels.
        /// Codec optimization only; grants no input or scrolling permission.
        #[arg(long)]
        scroll_copy: bool,

        /// Separately permit discrete vertical scroll on the scoped pointer lease.
        #[arg(long, requires = "allow_wayland_pointer")]
        allow_wayland_scroll: bool,

        /// Native Wayland socket for local selection (defaults to WAYLAND_DISPLAY).
        #[arg(long, value_name = "SOCKET", requires = "wayland_select")]
        wayland_display: Option<String>,

        /// Share the clipboard with the authenticated peer, both directions:
        /// text, images, and files copied in a file manager.
        ///
        /// Text marked secret by a password manager is never shared.
        #[arg(long)]
        allow_clipboard: bool,

        /// Narrow shared content; requires explicit clipboard consent.
        #[arg(long, default_value = "all", requires = "allow_clipboard")]
        clipboard_mode: ClipboardMode,

        /// Permitted direction, relative to the viewer and host.
        #[arg(long, default_value = "both", requires = "allow_clipboard")]
        clipboard_direction: ClipboardDirection,

        /// Largest file copy shared through the clipboard, such as 512M or
        /// 10G; 0 shares text and images only.
        #[arg(long, default_value = DEFAULT_CLIPBOARD_FILE_LIMIT, value_name = "SIZE", value_parser = parse_size)]
        clipboard_file_limit: u64,

        #[command(flatten)]
        frame_limits: FrameLimitArgs,

        /// Exit after the first connection ends (useful for tests/scripts).
        #[arg(long)]
        once: bool,

        /// Emit host pipeline statistics as JSON lines on stderr.
        #[arg(long)]
        stats_json: bool,
    },
    /// Control another JWM desktop through SSH, with nothing set up there.
    ///
    /// Starts a host on the other machine over SSH -- loopback only, for one
    /// session, with a fresh key handed over the SSH channel -- and opens it
    /// here through an SSH tunnel. Needs `jwm-remote` installed on both.
    Ssh {
        /// `[USER@]HOST` or a Host alias from your SSH configuration.
        #[arg(value_name = "[USER@]HOST")]
        destination: String,

        /// Host frame-rate ceiling (same range as host --fps).
        #[arg(long, value_parser = parse_fps)]
        fps: Option<u16>,
        /// Host adaptive JPEG quality ceiling (same range as host).
        #[arg(long, value_parser = parse_quality)]
        jpeg_quality: Option<u8>,
        /// Host encoded width ceiling; 0 fits the viewer to native width.
        #[arg(long, value_parser = parse_max_width)]
        max_width: Option<u16>,

        /// Share one monitor of the other machine by RandR name (see
        /// `xrandr --listmonitors` there), instead of all of them.
        #[arg(long, value_name = "NAME", value_parser = parse_monitor)]
        monitor: Option<String>,

        #[command(flatten)]
        shares: ShareArgs,

        /// Watch only: the host is started without input control.
        #[arg(long)]
        view_only: bool,

        /// Click the viewer to grab local input; Ctrl+Super+F12 releases it.
        #[arg(long, conflicts_with = "view_only")]
        grab_input: bool,

        /// Base X keysym of the Ctrl+Super grab-release shortcut.
        #[arg(long, default_value = DEFAULT_ESCAPE_KEY, value_name = "KEYSYM")]
        escape_key: String,

        /// Share the clipboard with the other machine, both directions:
        /// text, images, and files copied in a file manager.
        #[arg(long)]
        clipboard: bool,

        /// Narrow shared content; requires explicit clipboard consent.
        #[arg(long, default_value = "all", requires = "clipboard")]
        clipboard_mode: ClipboardMode,

        /// Permitted direction, relative to the viewer and host.
        #[arg(long, default_value = "both", requires = "clipboard")]
        clipboard_direction: ClipboardDirection,

        /// Largest file copy shared through the clipboard, such as 512M or
        /// 10G; 0 shares text and images only.
        #[arg(long, default_value = DEFAULT_CLIPBOARD_FILE_LIMIT, value_name = "SIZE", value_parser = parse_size)]
        clipboard_file_limit: u64,

        #[command(flatten)]
        frame_limits: FrameLimitArgs,

        /// Explicit local client size (max 8192 per axis, 16777216 pixels).
        /// The window manager may override this startup request.
        #[arg(long, value_name = "WIDTHxHEIGHT")]
        window_size: Option<WindowSize>,

        /// Best-effort local client origin X,Y; off-screen positions fall back.
        #[arg(long, value_name = "X,Y", allow_hyphen_values = true)]
        window_position: Option<WindowPosition>,

        /// Local X11 display for the viewer window (defaults to DISPLAY).
        #[arg(long, value_name = "DISPLAY")]
        display: Option<String>,

        /// Path of jwm-remote on the other machine (default: PATH, then the
        /// managed bundle). A leading ~/ is that machine's home.
        #[arg(long, value_name = "PATH")]
        remote_command: Option<String>,

        /// Extra ssh -o option, such as Port=2222; may be repeated.
        #[arg(long = "ssh-option", value_name = "OPTION")]
        ssh_options: Vec<String>,
    },
    /// Open a remote JWM desktop in an explicitly selected local viewer.
    #[command(group(clap::ArgGroup::new("native_viewer_mode")
        .args(["view_only", "wayland_keyboard"]).multiple(false)))]
    Connect {
        /// Local backend; Wayland requires explicit view-only or scoped keyboard-only.
        #[arg(long, default_value_t=ViewerBackend::X11, requires_if("wayland", "native_viewer_mode"))]
        viewer_backend: ViewerBackend,

        /// Native viewer Wayland socket; never an X11 display.
        #[arg(long, value_name = "SOCKET")]
        viewer_wayland_display: Option<String>,
        /// Hostname/IP, optionally with port (default 48221). Use [IPv6]:PORT.
        #[arg(value_name = "HOST[:PORT]", value_parser = normalize_address)]
        address: String,

        /// Additional attempts after a proven transport drop and clean teardown.
        /// Direct connect only; no retries on startup, escape/close or host refusal.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=5))]
        reconnect_attempts: u8,

        /// Private key; defaults to the JWM key path shown in top-level help.
        #[arg(long, value_name = "PATH")]
        key_file: Option<PathBuf>,

        /// Explicit local client size (max 8192 per axis, 16777216 pixels).
        /// The window manager may override this startup request.
        #[arg(long, value_name = "WIDTHxHEIGHT")]
        window_size: Option<WindowSize>,

        /// Explicitly resize the authorized source once, after first presentation.
        #[arg(long, value_name = "WIDTHxHEIGHT", requires = "view_only", conflicts_with_all = ["reconnect_attempts", "clipboard", "grab_input"])]
        resize_source: Option<WindowSize>,

        /// Resize the real remote window to follow this X11 viewer's client size.
        /// Explicit view-only mode, at most 4 requests/s; stops on refusal or timeout.
        #[arg(long, requires = "view_only", conflicts_with_all = ["resize_source", "wayland_keyboard", "wayland_pointer", "clipboard", "grab_input", "reconnect_attempts", "viewer_wayland_display"])]
        follow_source_size: bool,

        /// Request keyboard-only control of the locally selected Wayland surface.
        /// No pointer, clipboard, input grab, reconnect, or saved authorization.
        #[arg(long, conflicts_with_all = ["view_only", "resize_source", "clipboard", "grab_input", "reconnect_attempts"])]
        wayland_keyboard: bool,

        /// Request a fresh-frame scoped pointer lease, not legacy pointer input.
        #[arg(long, conflicts_with_all = ["wayland_keyboard", "view_only", "resize_source", "clipboard", "grab_input", "reconnect_attempts"])]
        wayland_pointer: bool,

        /// Separately request single-notch vertical scroll; never smooth or horizontal.
        #[arg(long, requires = "wayland_pointer")]
        wayland_scroll: bool,

        /// Best-effort local client origin X,Y; off-screen positions fall back.
        #[arg(long, value_name = "X,Y", allow_hyphen_values = true)]
        window_position: Option<WindowPosition>,

        /// Local X11 display for the viewer window (defaults to DISPLAY).
        #[arg(long, value_name = "DISPLAY")]
        display: Option<String>,

        /// Never send local keyboard or pointer events.
        #[arg(long)]
        view_only: bool,

        /// Click the viewer to grab local input; Ctrl+Super+F12 releases it.
        #[arg(long, conflicts_with = "view_only")]
        grab_input: bool,

        /// Base X keysym of the Ctrl+Super grab-release shortcut.
        ///
        /// Without Ctrl+Super, this key is forwarded to the remote as usual.
        #[arg(long, default_value = DEFAULT_ESCAPE_KEY, value_name = "KEYSYM")]
        escape_key: String,

        /// Share the clipboard with the host, both directions: text,
        /// images, and files copied in a file manager.
        ///
        /// The host must also be started with --allow-clipboard.
        #[arg(long)]
        clipboard: bool,

        /// Narrow shared content; requires explicit clipboard consent.
        #[arg(long, default_value = "all", requires = "clipboard")]
        clipboard_mode: ClipboardMode,

        /// Permitted direction, relative to the viewer and host.
        #[arg(long, default_value = "both", requires = "clipboard")]
        clipboard_direction: ClipboardDirection,

        /// Largest file copy shared through the clipboard, such as 512M or
        /// 10G; 0 shares text and images only.
        #[arg(long, default_value = DEFAULT_CLIPBOARD_FILE_LIMIT, value_name = "SIZE", value_parser = parse_size)]
        clipboard_file_limit: u64,

        #[command(flatten)]
        frame_limits: FrameLimitArgs,
    },
}

#[derive(Debug, Subcommand)]
enum ClipboardStagingAction {
    /// Read local cap and accounting without creating, deleting or repairing state.
    Status,
    /// Enable or replace the cap; refuses a limit below current reservations.
    SetLimit {
        /// Positive bytes or binary K/M/G/T size; use disable to remove the cap.
        #[arg(value_name = "SIZE", value_parser = parse_staging_limit)]
        size: u64,
    },
    /// Remove the cap, preserving accounting and all staged files.
    Disable,
}

#[derive(Debug, Subcommand)]
enum ProfileAction {
    /// Create the private profile directory explicitly; never changes existing permissions.
    Init,
    /// Save settings without connecting. Example: save desk -- ssh user@desk --view-only
    Save {
        name: String,
        #[arg(long)]
        replace: bool,
        /// A connect or ssh invocation, following --. No SSH options/commands are stored.
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        connection: Vec<String>,
    },
    List,
    Show {
        name: String,
    },
    /// Start exactly the saved settings; optional flags only reduce permissions.
    Run {
        name: String,
        #[arg(long)]
        view_only: bool,
        #[arg(long)]
        no_clipboard: bool,
    },
    Remove {
        name: String,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let command = diagnostic_command(&cli.command);
    let result = run(cli.command);
    if cli.diagnostics_json {
        // Best effort only: diagnostics must never change the command outcome.
        let _ = emit_diagnostic(&mut std::io::stderr().lock(), command, &result);
    }
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("jwm-remote: {error}");
            if cli.notify {
                notify_failure(&error.to_string());
            }
            std::process::ExitCode::FAILURE
        }
    }
}

fn diagnostic_command(command: &Command) -> &'static str {
    match command {
        Command::Host { .. } => "host",
        Command::Connect { .. } => "connect",
        Command::Ssh { .. } => "ssh",
        Command::Keygen { .. } => "keygen",
        Command::Doctor { .. } => "doctor",
        Command::ReplayBenchmark { .. } => "replay_benchmark",
        Command::ReplayFixture { .. } => "replay_fixture",
        Command::ClipboardStaging { action } => match action {
            ClipboardStagingAction::Status => "clipboard_staging_status",
            ClipboardStagingAction::SetLimit { .. } => "clipboard_staging_set_limit",
            ClipboardStagingAction::Disable => "clipboard_staging_disable",
        },
        Command::Profile { action, .. } => match action {
            ProfileAction::Init => "profile_init",
            ProfileAction::Save { .. } => "profile_save",
            ProfileAction::List => "profile_list",
            ProfileAction::Show { .. } => "profile_show",
            ProfileAction::Run { .. } => "profile_run",
            ProfileAction::Remove { .. } => "profile_remove",
        },
    }
}

fn emit_diagnostic(
    writer: &mut impl std::io::Write,
    command: &'static str,
    result: &RemoteResult<()>,
) -> std::io::Result<()> {
    #[derive(serde::Serialize)]
    struct Record {
        #[serde(rename = "type")]
        record_type: &'static str,
        schema_version: u8,
        command: &'static str,
        outcome: &'static str,
        #[serde(flatten)]
        failure: jwm::remote::client::FailureDiagnostic,
    }
    let failure = match result {
        Err(error) => jwm::remote::client::classify_failure(error.as_ref()),
        Ok(()) => jwm::remote::client::FailureDiagnostic {
            stage: None,
            category: "success",
            io_kind: None,
            peer_close_code: None,
            cleanup: "unknown",
            reconnect_certified: false,
        },
    };
    let record = Record {
        record_type: "jwm_remote_command_result",
        schema_version: 2,
        command,
        outcome: if result.is_ok() { "success" } else { "error" },
        failure,
    };
    let mut bytes = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)
}

fn profile_error(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

fn profile_from_command(command: Command) -> RemoteResult<Profile> {
    let (connection, viewer) = match command {
        Command::Connect {
            viewer_backend,
            viewer_wayland_display,
            reconnect_attempts,
            address,
            key_file,
            display,
            window_size,
            resize_source,
            follow_source_size,
            wayland_keyboard,
            wayland_pointer,
            wayland_scroll,
            window_position,
            view_only,
            grab_input,
            escape_key,
            clipboard,
            clipboard_mode,
            clipboard_direction,
            clipboard_file_limit,
            frame_limits,
        } => {
            if viewer_backend == ViewerBackend::Wayland || viewer_wayland_display.is_some() {
                return Err(profile_error(
                    "native Wayland viewer selection cannot be saved in a profile",
                )
                .into());
            }
            if wayland_keyboard || wayland_pointer || wayland_scroll {
                return Err(profile_error(
                    "Wayland input authorization cannot be saved in a profile",
                )
                .into());
            }
            if follow_source_size {
                return Err(profile_error("source-size following cannot be saved in a profile; consent is direct-connect only").into());
            }
            if resize_source.is_some() {
                return Err(
                    profile_error("explicit source resize cannot be saved in a profile").into(),
                );
            }
            if reconnect_attempts != 0 {
                return Err(profile_error(
                    "reconnect consent cannot be saved in a profile; use connect directly",
                )
                .into());
            }
            if key_file.as_ref().is_some_and(|path| !path.is_absolute()) {
                return Err(profile_error(
                    "saved key paths must be absolute (contents are never stored)",
                )
                .into());
            }
            (
                Connection::Connect { address, key_file },
                Viewer {
                    display,
                    window_size,
                    window_position,
                    view_only,
                    grab_input,
                    escape_key,
                    clipboard,
                    clipboard_mode,
                    clipboard_direction,
                    clipboard_file_limit,
                    frame_max_width: frame_limits.frame_max_width,
                    frame_max_height: frame_limits.frame_max_height,
                    frame_max_pixels: frame_limits.frame_max_pixels,
                },
            )
        }
        Command::Ssh {
            destination,
            monitor,
            shares,
            fps,
            jpeg_quality,
            max_width,
            display,
            window_size,
            window_position,
            view_only,
            grab_input,
            escape_key,
            clipboard,
            clipboard_mode,
            clipboard_direction,
            clipboard_file_limit,
            frame_limits,
            remote_command,
            ssh_options,
        } => {
            jwm::remote::ssh::validate_destination(&destination)?;
            if !shares.is_empty() {
                return Err(profile_error("profiles do not store extra source grants; use ssh directly with --share-monitor/--share-region/--share-root").into());
            }
            if remote_command.is_some() || !ssh_options.is_empty() {
                return Err(profile_error("profiles do not store SSH commands or arbitrary options; use an SSH config alias").into());
            }
            (
                Connection::Ssh {
                    destination,
                    monitor,
                    fps,
                    jpeg_quality,
                    max_width,
                },
                Viewer {
                    display,
                    window_size,
                    window_position,
                    view_only,
                    grab_input,
                    escape_key,
                    clipboard,
                    clipboard_mode,
                    clipboard_direction,
                    clipboard_file_limit,
                    frame_max_width: frame_limits.frame_max_width,
                    frame_max_height: frame_limits.frame_max_height,
                    frame_max_pixels: frame_limits.frame_max_pixels,
                },
            )
        }
        _ => return Err(profile_error("only connect and ssh commands can be saved").into()),
    };
    jwm::remote::x11_viewer::validate_escape_key(&viewer.escape_key)?;
    let profile = Profile {
        version: 1,
        connection,
        viewer,
    };
    // Reject control characters and unbounded strings even in manually edited JSON.
    let text = serde_json::to_value(&profile)?;
    fn check(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(value) => {
                value.len() <= 4096 && !value.chars().any(char::is_control)
            }
            serde_json::Value::Array(values) => values.iter().all(check),
            serde_json::Value::Object(values) => values.values().all(check),
            _ => true,
        }
    }
    if !check(&text) {
        return Err(profile_error(
            "profile strings must be bounded and contain no control characters",
        )
        .into());
    }
    Ok(profile)
}

/// Reuse the real CLI parser on every load: ranges/conflicts cannot drift.
fn command_from_profile(profile: &Profile) -> RemoteResult<Command> {
    let mut args = vec!["jwm-remote".to_owned()];
    match &profile.connection {
        Connection::Connect { address, key_file } => {
            args.extend(["connect".to_owned(), address.clone()]);
            if let Some(path) = key_file {
                args.extend([
                    "--key-file".to_owned(),
                    path.to_str()
                        .ok_or_else(|| profile_error("profile key path must be UTF-8"))?
                        .to_owned(),
                ]);
            }
        }
        Connection::Ssh {
            destination,
            monitor,
            fps,
            jpeg_quality,
            max_width,
        } => {
            args.extend(["ssh".to_owned(), destination.clone()]);
            if let Some(monitor) = monitor {
                args.extend(["--monitor".to_owned(), monitor.clone()]);
            }
            for (flag, value) in [
                ("--fps", fps.map(u64::from)),
                ("--jpeg-quality", jpeg_quality.map(u64::from)),
                ("--max-width", max_width.map(u64::from)),
            ] {
                if let Some(value) = value {
                    args.extend([flag.to_owned(), value.to_string()]);
                }
            }
        }
    }
    let viewer = &profile.viewer;
    if let Some(position) = viewer.window_position {
        args.extend(["--window-position".to_owned(), position.to_string()]);
    }
    if let Some(size) = viewer.window_size {
        size.validate()?;
        args.extend(["--window-size".to_owned(), size.to_string()]);
    }
    for (flag, value) in [
        ("--frame-max-width", viewer.frame_max_width.map(u64::from)),
        ("--frame-max-height", viewer.frame_max_height.map(u64::from)),
        ("--frame-max-pixels", viewer.frame_max_pixels),
    ] {
        if let Some(value) = value {
            args.extend([flag.to_owned(), value.to_string()]);
        }
    }

    if let Some(display) = &viewer.display {
        args.extend(["--display".to_owned(), display.clone()]);
    }
    for (flag, enabled) in [
        ("--view-only", viewer.view_only),
        ("--grab-input", viewer.grab_input),
        ("--clipboard", viewer.clipboard),
    ] {
        if enabled {
            args.push(flag.to_owned());
        }
    }
    if viewer.clipboard {
        args.extend([
            "--clipboard-mode".to_owned(),
            viewer.clipboard_mode.to_string(),
            "--clipboard-direction".to_owned(),
            viewer.clipboard_direction.to_string(),
        ]);
    }
    args.extend([
        "--escape-key".to_owned(),
        viewer.escape_key.clone(),
        "--clipboard-file-limit".to_owned(),
        viewer.clipboard_file_limit.to_string(),
    ]);
    Ok(Cli::try_parse_from(args)?.command)
}

fn validated_profile(profile: Profile) -> RemoteResult<Profile> {
    if profile.version != 1 {
        return Err(profile_error("unsupported profile schema version").into());
    }
    let mut validated = profile_from_command(command_from_profile(&profile)?)?;
    // Disabled consent remains authoritative. Keep dormant narrowing without
    // synthesizing consent merely to round-trip through command-line parsing.
    if !profile.viewer.clipboard {
        validated.viewer.clipboard_mode = profile.viewer.clipboard_mode;
        validated.viewer.clipboard_direction = profile.viewer.clipboard_direction;
    }
    if validated.viewer.clipboard_file_limit != profile.viewer.clipboard_file_limit {
        return Err(profile_error(
            "profile clipboard limit cannot round-trip through the CLI size parser",
        )
        .into());
    }
    Ok(validated)
}

fn run_profile(directory: Option<PathBuf>, action: ProfileAction) -> RemoteResult<()> {
    let directory = match directory {
        Some(path) => path,
        None => resolve_key_path(None)?
            .parent()
            .ok_or_else(|| profile_error("missing config directory"))?
            .join("remote-profiles"),
    };
    if matches!(action, ProfileAction::Init) {
        Store::init(&directory)?;
        println!("Initialized private profile directory: {directory:?}");
        return Ok(());
    }
    let store = Store::open(&directory)?;
    match action {
        ProfileAction::Init => unreachable!("handled before opening store"),
        ProfileAction::Save {
            name,
            replace,
            connection,
        } => {
            let cli =
                Cli::try_parse_from(std::iter::once("jwm-remote".to_owned()).chain(connection))?;
            let profile = validated_profile(profile_from_command(cli.command)?)?;
            store.save(&name, &profile, replace)?;
            println!("Saved profile {name}; no connection was started.");
        }
        ProfileAction::List => {
            for name in store.list()? {
                validated_profile(store.load(&name)?)?;
                println!("{name}");
            }
        }
        ProfileAction::Show { name } => println!(
            "{}",
            serde_json::to_string_pretty(&validated_profile(store.load(&name)?)?)?
        ),
        ProfileAction::Remove { name } => {
            store.remove(&name)?;
            println!("Removed profile {name}.");
        }
        ProfileAction::Run {
            name,
            view_only,
            no_clipboard,
        } => {
            let mut profile = validated_profile(store.load(&name)?)?;
            if view_only {
                profile.viewer.view_only = true;
                profile.viewer.grab_input = false;
            }
            if no_clipboard {
                profile.viewer.clipboard = false;
            }
            let command = command_from_profile(&profile)?;
            // Do not retain the store lock for the session lifetime.
            drop(store);
            return run(command);
        }
    }
    Ok(())
}

/// Show a failure through the desktop's notification service, best effort.
fn notify_failure(message: &str) {
    // The first line is the cause; the rest is advice for a terminal reader.
    let body: String = message
        .lines()
        .next()
        .unwrap_or_default()
        .trim_end_matches("; the lines above say more")
        .chars()
        .take(300)
        .collect();
    let _ = std::process::Command::new("notify-send")
        .args([
            "--app-name=JWM Remote",
            "--icon=network-error",
            "Remote desktop failed",
            &body,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn resolve_key_path(explicit: Option<PathBuf>) -> std::io::Result<PathBuf> {
    explicit.map_or_else(
        || {
            default_key_path(
                std::env::var_os("XDG_CONFIG_HOME"),
                std::env::var_os("HOME"),
            )
        },
        Ok,
    )
}

/// Parse exact bytes without floating-point rounding at the u64 boundary.
fn parse_staging_limit(value: &str) -> Result<u64, String> {
    let invalid =
        || "expected positive whole bytes or a binary K/M/G/T size such as 512M or 1.5G".to_owned();
    let value = value.trim();
    if value.is_empty() || value.len() > 64 {
        return Err(invalid());
    }
    let split = value
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(value.len());
    let number = &value[..split];
    let unit = value[split..].trim().to_ascii_uppercase();
    let multiplier: u128 = match unit.as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        "T" | "TB" | "TIB" => 1 << 40,
        _ => return Err(invalid()),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 18
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || (number.contains('.') && fraction.is_empty())
    {
        return Err(invalid());
    }
    let whole = whole.parse::<u64>().map_err(|_| invalid())?;
    let denominator = 10_u128.pow(fraction.len() as u32);
    let numerator = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u128>().map_err(|_| invalid())? * multiplier
    };
    if !numerator.is_multiple_of(denominator) {
        return Err("staging limit must resolve to a whole number of bytes".to_owned());
    }
    let bytes = u128::from(whole) * multiplier + numerator / denominator;
    let bytes = u64::try_from(bytes).map_err(|_| "staging limit exceeds u64 bytes".to_owned())?;
    if bytes == 0 {
        return Err("staging limit must be greater than zero; use clipboard-staging disable to remove the cap".to_owned());
    }
    Ok(bytes)
}

fn write_staging_status(
    writer: &mut impl std::io::Write,
    root: &Path,
    status: &StagingBudgetStatus,
) -> std::io::Result<()> {
    // Debug formatting escapes control characters in the local cache path.
    writeln!(writer, "Local clipboard staging root: {root:?}")?;
    match status.limit_bytes {
        Some(limit) => writeln!(writer, "Admission cap: {limit} bytes")?,
        None => writeln!(writer, "Admission cap: disabled (uncapped)")?,
    }
    writeln!(writer, "Reserved archive bytes: {}", status.reserved_bytes)?;
    writeln!(writer, "Reservations: {}", status.reservations)?;
    writeln!(
        writer,
        "Accounting: {}",
        if status.initialized {
            "initialized"
        } else {
            "not initialized (read-only inventory)"
        }
    )?;
    writeln!(
        writer,
        "Scope: cooperating processes using this cache root; not a filesystem quota."
    )
}

fn run_clipboard_staging(
    action: ClipboardStagingAction,
    root: &Path,
    writer: &mut impl std::io::Write,
) -> std::io::Result<()> {
    let status = match action {
        ClipboardStagingAction::Status => clipboard_staging_budget::status(root)?,
        ClipboardStagingAction::SetLimit { size } => {
            let status = clipboard_staging_budget::set_limit(root, size)?;
            writeln!(
                writer,
                "Clipboard staging admission cap set; no files evicted."
            )?;
            status
        }
        ClipboardStagingAction::Disable => {
            let status = clipboard_staging_budget::disable(root)?;
            writeln!(
                writer,
                "Clipboard staging admission cap disabled; accounting and files preserved."
            )?;
            status
        }
    };
    write_staging_status(writer, root, &status)
}

/// Conditional backend conflicts must be checked before key lookup or connect.
fn validate_follow_source_cli(
    follow: bool,
    backend: ViewerBackend,
    wayland_display: Option<&str>,
) -> RemoteResult<()> {
    if follow && (backend != ViewerBackend::X11 || wayland_display.is_some()) {
        return Err(profile_error(
            "--follow-source-size requires the direct X11 viewer, never the Wayland viewer",
        )
        .into());
    }
    Ok(())
}

fn run(command: Command) -> RemoteResult<()> {
    match command {
        Command::ClipboardStaging { action } => {
            run_clipboard_staging(action, &staging_root()?, &mut std::io::stdout().lock())?;
            Ok(())
        }
        Command::ReplayBenchmark {
            width,
            height,
            frames,
            warmup,
            seed,
            quality,
        } => {
            let report =
                jwm::remote::replay_benchmark::run(jwm::remote::replay_benchmark::ReplayConfig {
                    width,
                    height,
                    frames,
                    warmup,
                    seed,
                    quality,
                })?;
            serde_json::to_writer_pretty(std::io::stdout().lock(), &report)?;
            println!();
            Ok(())
        }
        Command::ReplayFixture {
            output,
            width,
            height,
            frames,
            seed,
            quality,
        } => {
            jwm::remote::replay_benchmark::export_fixture(
                jwm::remote::replay_benchmark::ReplayConfig {
                    width,
                    height,
                    frames,
                    warmup: 0,
                    seed,
                    quality,
                },
                &output,
            )?;
            println!("{}", output.join("manifest.json").display());
            Ok(())
        }
        Command::Profile { directory, action } => run_profile(directory, action),
        Command::Keygen { output } => {
            let output = resolve_key_path(output)?;
            if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty())
                && !parent.is_dir()
            {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound,
                    format!("key parent directory does not exist: {:?}. Create a private directory first (see keygen --help); nothing was created", parent)).into());
            }
            generate_key_file(&output)?;
            println!("Generated private remote key: {:?}", output);
            Ok(())
        }
        Command::Doctor { key_file, display } => run_doctor(resolve_key_path(key_file)?, display),
        Command::Host {
            listen,
            key_file,
            key_stdin,
            display,
            fps,
            jpeg_quality,
            jpeg_quality_floor,
            fixed_jpeg_quality,
            max_width,
            capture_source,
            scaling,
            monitor,
            window,
            shares,
            region,
            allow_lan,
            allow_input,
            allow_window_resize,
            wayland_select,
            allow_wayland_keyboard,
            allow_wayland_pointer,
            allow_wayland_scroll,
            scroll_copy,
            wayland_display,
            allow_clipboard,
            clipboard_mode,
            clipboard_direction,
            clipboard_file_limit,
            frame_limits,
            once,
            stats_json,
        } => {
            let key = if key_stdin {
                KeySource::Stdin
            } else {
                KeySource::File(resolve_key_path(key_file)?)
            };
            let display = if wayland_select {
                None
            } else {
                resolve_host_display(display)?
            };
            let jpeg_quality_floor = resolve_quality_floor(jpeg_quality, jpeg_quality_floor)?;
            let capture_area = match (window, monitor, region) {
                (Some(id), _, _) => CaptureArea::Window(id),
                (None, Some(name), _) => CaptureArea::Monitor(name),
                (None, None, Some(region)) => CaptureArea::Region(region),
                (None, None, None) => CaptureArea::Root,
            };
            let shared_sources = shares.areas(&capture_area)?;
            run_host(HostOptions {
                listen,
                key,
                display,
                fps,
                jpeg_quality,
                jpeg_quality_floor,
                fixed_jpeg_quality,
                max_width,
                capture_source,
                capture_area,
                shared_sources,
                scaling,
                allow_lan,
                allow_input,
                allow_window_resize,
                wayland_select,
                allow_wayland_keyboard,
                allow_wayland_pointer,
                allow_wayland_scroll,
                scroll_copy,
                wayland_display,
                allow_clipboard,
                clipboard_mode,
                clipboard_direction,
                clipboard_file_limit,
                frame_limits: frame_limits.limits(),
                once,
                stats_json,
            })
        }
        Command::Ssh {
            destination,
            fps,
            jpeg_quality,
            max_width,
            monitor,
            shares,
            view_only,
            grab_input,
            escape_key,
            clipboard,
            clipboard_mode,
            clipboard_direction,
            clipboard_file_limit,
            frame_limits,
            display,
            window_size,
            window_position,
            remote_command,
            ssh_options,
        } => {
            let initial = monitor
                .clone()
                .map_or(CaptureArea::Root, CaptureArea::Monitor);
            let shared_sources = shares.areas(&initial)?;
            run_ssh(SshOptions {
                destination,
                fps,
                jpeg_quality,
                max_width,
                ssh_options,
                remote_command,
                monitor,
                shared_sources,
                display,
                window_size,
                window_position,
                view_only,
                grab_input,
                clipboard,
                clipboard_mode,
                clipboard_direction,
                clipboard_file_limit,
                frame_limits: frame_limits.limits(),
                escape_key,
            })
        }
        Command::Connect {
            viewer_backend,
            viewer_wayland_display,
            reconnect_attempts,
            address,
            key_file,
            display,
            window_size,
            resize_source,
            follow_source_size,
            wayland_keyboard,
            wayland_pointer,
            wayland_scroll,
            window_position,
            view_only,
            grab_input,
            clipboard,
            clipboard_mode,
            clipboard_direction,
            clipboard_file_limit,
            frame_limits,
            escape_key,
        } => {
            validate_follow_source_cli(
                follow_source_size,
                viewer_backend,
                viewer_wayland_display.as_deref(),
            )?;
            run_client_reconnecting(
                ClientOptions {
                    viewer_backend,
                    viewer_wayland_display,
                    peer_label: address.clone(),
                    address,
                    key: KeySource::File(resolve_key_path(key_file)?),
                    display,
                    window_size,
                    resize_source,
                    follow_source_size,
                    wayland_keyboard,
                    wayland_pointer,
                    wayland_scroll,
                    window_position,
                    view_only,
                    grab_input,
                    clipboard,
                    clipboard_mode,
                    clipboard_direction,
                    clipboard_file_limit,
                    frame_limits: frame_limits.limits(),
                    escape_key,
                },
                reconnect_attempts,
            )
        }
    }
}

/// The display a host shares: as given, else DISPLAY, else -- and always for
/// `auto` -- the display of the user's running JWM session.
fn resolve_host_display(display: Option<String>) -> RemoteResult<Option<String>> {
    let from_environment = || {
        std::env::var("DISPLAY")
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    match display {
        Some(display) if display != "auto" => return Ok(Some(display)),
        None if from_environment().is_some() => return Ok(None),
        _ => {}
    }
    let session = session_env::discover()?;
    eprintln!(
        "jwm-remote: using DISPLAY {} of your JWM session (pid {})",
        session.display, session.pid
    );
    if let Some(xauthority) = &session.xauthority {
        // x11rb reads the cookie path from the environment only.
        // SAFETY: still single-threaded here; no thread has been started
        // that could read the environment concurrently.
        unsafe { std::env::set_var("XAUTHORITY", xauthority) };
    }
    Ok(Some(session.display))
}

fn run_doctor(key_file: PathBuf, display: Option<String>) -> RemoteResult<()> {
    println!(
        "jwm-remote {} offline preflight (remote-x11 compiled)",
        env!("CARGO_PKG_VERSION")
    );
    println!("Local protocol: transport {TRANSPORT_VERSION}, application {APPLICATION_VERSION}");
    println!("Peer compatibility: NOT probed; update both helpers together");
    let display = display
        .or_else(|| std::env::var("DISPLAY").ok())
        .filter(|value| !value.trim().is_empty());
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into());
    println!(
        "Session type: {session:?}; DISPLAY: {:?} (not opened)",
        display.as_deref().unwrap_or("missing")
    );
    // Over SSH there is no DISPLAY, yet `host --display auto` (and so
    // `jwm-remote ssh` from the other machine) works from the running session.
    let session_display = if display.is_none() {
        session_env::discover().ok()
    } else {
        None
    };
    if let Some(found) = &session_display {
        println!(
            "JWM session: DISPLAY {:?} (pid {}); `host --display auto` and `jwm-remote ssh` use it",
            found.display, found.pid
        );
    }
    let mut blocked =
        (display.is_none() && session_display.is_none()) || session.eq_ignore_ascii_case("wayland");
    if blocked {
        println!(
            "FAIL: use a JWM X11 session on both machines; Wayland/Xwayland is outside this MVP"
        );
    }
    println!("Key path: {:?}", key_file);
    match key_metadata(&key_file, unsafe { libc::geteuid() }) {
        Ok(summary) => println!("Key metadata: {summary}"),
        Err(error) => {
            blocked = true;
            println!("FAIL: key metadata: {error}");
            println!(
                "If setting up for the first time, see keygen --help; use --key-file for an existing private key. No file was changed."
            );
        }
    }
    println!(
        "NOT probed: JWM ownership, X11 connectivity, Composite/XTEST/XKB, capture area, network reachability or peer key/version."
    );
    println!("Dependencies: this helper loaded; native display extensions remain unverified.");
    if blocked {
        Err(std::io::Error::other(
            "offline preflight found local setup problems; nothing was connected or changed",
        )
        .into())
    } else {
        println!("Offline checks passed. This is not an end-to-end readiness check.");
        Ok(())
    }
}

fn parse_fps(value: &str) -> Result<u16, String> {
    parse_range(value, 1, 60, "fps")
}

fn parse_quality(value: &str) -> Result<u8, String> {
    parse_range(value, 1, 100, "JPEG quality")
}

fn resolve_quality_floor(maximum: u8, explicit_floor: Option<u8>) -> Result<u8, String> {
    let floor = explicit_floor.unwrap_or(maximum.min(DEFAULT_JPEG_QUALITY_FLOOR));
    if floor > maximum {
        return Err(format!(
            "JPEG quality floor {floor} exceeds the configured maximum {maximum}"
        ));
    }
    Ok(floor)
}

fn parse_max_width(value: &str) -> Result<u16, String> {
    let parsed = value
        .parse::<u16>()
        .map_err(|error| format!("max width must be an integer: {error}"))?;
    if parsed == 0 || (320..=16_384).contains(&parsed) {
        Ok(parsed)
    } else {
        Err("max width must be 0 or between 320 and 16384".into())
    }
}

fn parse_capture_source(value: &str) -> Result<CaptureSource, String> {
    value.parse()
}

fn parse_scaling(value: &str) -> Result<ScalingPolicy, String> {
    value.parse()
}

fn parse_monitor(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err("monitor name must be 1..256 bytes without control characters".into());
    }
    Ok(value.to_owned())
}

fn parse_region(value: &str) -> Result<CaptureRegion, String> {
    value.parse()
}

fn parse_range<T>(value: &str, min: T, max: T, label: &str) -> Result<T, String>
where
    T: std::str::FromStr + PartialOrd + Copy + std::fmt::Display,
    T::Err: std::fmt::Display,
{
    let parsed = value
        .parse::<T>()
        .map_err(|error| format!("{label} must be an integer: {error}"))?;
    if parsed < min || parsed > max {
        return Err(format!("{label} must be between {min} and {max}"));
    }
    Ok(parsed)
}

/// Accept xwininfo-style hexadecimal IDs or unsigned decimal IDs.
fn parse_window_id(value: &str) -> Result<u32, String> {
    let parsed = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16)
    } else {
        value.parse::<u32>()
    };
    parsed
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| "window XID must be a nonzero decimal or hexadecimal integer".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn follow_source_size_is_explicit_narrow_and_never_profile_saved() {
        let plain = Cli::try_parse_from(["jwm-remote", "connect", "desk", "--view-only"])
            .unwrap()
            .command;
        assert!(matches!(
            &plain,
            Command::Connect {
                follow_source_size: false,
                ..
            }
        ));
        let profile = profile_from_command(plain).unwrap();
        assert!(matches!(
            command_from_profile(&profile).unwrap(),
            Command::Connect {
                follow_source_size: false,
                ..
            }
        ));
        let mut encoded = serde_json::to_value(profile).unwrap();
        encoded["viewer"]["follow_source_size"] = serde_json::json!(true);
        assert!(Profile::decode(&serde_json::to_vec(&encoded).unwrap()).is_err());
        let base = [
            "jwm-remote",
            "connect",
            "desk",
            "--view-only",
            "--follow-source-size",
        ];
        let enabled = Cli::try_parse_from(base).unwrap().command;
        assert!(matches!(
            &enabled,
            Command::Connect {
                follow_source_size: true,
                ..
            }
        ));
        assert!(
            profile_from_command(enabled)
                .unwrap_err()
                .to_string()
                .contains("cannot be saved")
        );
        assert!(
            Cli::try_parse_from(["jwm-remote", "connect", "desk", "--follow-source-size"]).is_err()
        );
        for extra in [
            vec!["--resize-source", "640x480"],
            vec!["--wayland-keyboard"],
            vec!["--wayland-pointer"],
            vec!["--clipboard"],
            vec!["--grab-input"],
            vec!["--reconnect-attempts", "0"],
            vec!["--reconnect-attempts", "1"],
            vec!["--viewer-wayland-display", "wayland-1"],
        ] {
            let mut args = base.to_vec();
            args.extend(extra);
            assert!(Cli::try_parse_from(args).is_err());
        }
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "ssh",
                "desk",
                "--view-only",
                "--follow-source-size"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["jwm-remote", "host", "--follow-source-size"]).is_err());
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "profile",
                "run",
                "desk",
                "--follow-source-size"
            ])
            .is_err()
        );
        assert!(validate_follow_source_cli(true, ViewerBackend::X11, None).is_ok());
        assert!(validate_follow_source_cli(true, ViewerBackend::Wayland, None).is_err());
        assert!(validate_follow_source_cli(true, ViewerBackend::X11, Some("wayland-1")).is_err());
        // Actual command dispatch rejects the backend before key-file access or
        // any display/network work. No socket is opened by this negative test.
        let invalid = Cli::try_parse_from([
            "jwm-remote",
            "connect",
            "desk",
            "--view-only",
            "--follow-source-size",
            "--viewer-backend",
            "wayland",
        ])
        .unwrap()
        .command;
        assert!(
            run(invalid)
                .unwrap_err()
                .to_string()
                .contains("requires the direct X11 viewer")
        );
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "connect",
                "desk",
                "--view-only",
                "--follow-source-size",
                "--viewer-backend",
                "x11"
            ])
            .is_ok()
        );
    }

    #[test]
    fn native_viewer_backend_requires_explicit_mode_and_is_not_profile_saved() {
        let cli = Cli::try_parse_from(["jwm-remote", "connect", "host"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Connect {
                viewer_backend: ViewerBackend::X11,
                ..
            }
        ));
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "connect",
                "host",
                "--viewer-backend",
                "wayland"
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from([
            "jwm-remote",
            "connect",
            "host",
            "--viewer-backend",
            "wayland",
            "--view-only",
            "--viewer-wayland-display",
            "wayland-owned",
        ])
        .unwrap();
        assert!(profile_from_command(cli.command).is_err());
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "connect",
                "host",
                "--viewer-backend",
                "unknown"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "ssh",
                "host",
                "--viewer-backend",
                "wayland",
                "--view-only"
            ])
            .is_err()
        );
    }

    #[test]
    fn native_wayland_keyboard_is_explicit_narrow_and_never_profile_saved() {
        let base = [
            "jwm-remote",
            "connect",
            "host",
            "--viewer-backend",
            "wayland",
            "--wayland-keyboard",
        ];
        let cli = Cli::try_parse_from(base).unwrap();
        assert!(matches!(
            &cli.command,
            Command::Connect {
                viewer_backend: ViewerBackend::Wayland,
                wayland_keyboard: true,
                view_only: false,
                wayland_pointer: false,
                wayland_scroll: false,
                grab_input: false,
                clipboard: false,
                reconnect_attempts: 0,
                ..
            }
        ));
        assert!(profile_from_command(cli.command).is_err());
        let explicit_socket = Cli::try_parse_from(
            base.into_iter()
                .chain(["--viewer-wayland-display", "owned-wayland"]),
        )
        .unwrap();
        assert!(profile_from_command(explicit_socket.command).is_err());
        for extra in [
            vec!["--view-only"],
            vec!["--wayland-pointer"],
            vec!["--wayland-pointer", "--wayland-scroll"],
            vec!["--clipboard"],
            vec!["--grab-input"],
            vec!["--resize-source", "640x480"],
            vec!["--follow-source-size"],
            vec!["--reconnect-attempts", "1"],
        ] {
            let args = base.into_iter().chain(extra).collect::<Vec<_>>();
            assert!(
                Cli::try_parse_from(&args).is_err(),
                "unsafe native keyboard CLI accepted: {args:?}"
            );
        }
        // Profile save captures trailing arguments before validation. Exercise
        // its real construction boundary without writing a profile or dialing.
        let outer = Cli::try_parse_from(
            ["jwm-remote", "profile", "save", "native", "--"]
                .into_iter()
                .chain(base.into_iter().skip(1)),
        )
        .unwrap();
        let Command::Profile {
            action: ProfileAction::Save { connection, .. },
            ..
        } = outer.command
        else {
            panic!("profile save expected")
        };
        let command =
            Cli::try_parse_from(std::iter::once("jwm-remote".to_owned()).chain(connection))
                .unwrap()
                .command;
        assert!(profile_from_command(command).is_err());
    }

    #[test]
    fn staging_limit_parser_is_exact_nonzero_and_checked() {
        for (text, expected) in [
            ("1", 1),
            ("512M", 512 << 20),
            ("1.5G", 3 << 29),
            (" 1.5 mib ", 3 << 19),
            ("1 KB", 1024),
            ("1.000000000000000000B", 1),
            ("0.0009765625K", 1),
            ("18446744073709551615", u64::MAX),
        ] {
            assert_eq!(parse_staging_limit(text), Ok(expected), "{text}");
        }
        for text in [
            "",
            "0",
            "0B",
            "0.0G",
            "0.1",
            "0.1K",
            "1.",
            ".5G",
            "-1",
            "+1",
            "NaN",
            "inf",
            "1e3",
            "1 2M",
            "1MiBB",
            "1P",
            "18446744073709551616",
            "16777216T",
            "16777215.999999999T",
            "18446744073709551615K",
            "1.0000000000000000000B",
        ] {
            assert!(parse_staging_limit(text).is_err(), "{text}");
        }
        assert!(
            parse_staging_limit("0")
                .unwrap_err()
                .contains("clipboard-staging disable")
        );
        assert!(
            parse_staging_limit("0.1K")
                .unwrap_err()
                .contains("whole number of bytes")
        );
        assert!(
            parse_staging_limit("16777216T")
                .unwrap_err()
                .contains("exceeds u64")
        );
    }

    #[test]
    fn staging_cli_requires_explicit_action_and_size() {
        assert!(matches!(
            Cli::try_parse_from(["jwm-remote", "clipboard-staging", "status"])
                .unwrap()
                .command,
            Command::ClipboardStaging {
                action: ClipboardStagingAction::Status
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["jwm-remote", "clipboard-staging", "disable"])
                .unwrap()
                .command,
            Command::ClipboardStaging {
                action: ClipboardStagingAction::Disable
            }
        ));
        let Command::ClipboardStaging {
            action: ClipboardStagingAction::SetLimit { size },
        } = Cli::try_parse_from(["jwm-remote", "clipboard-staging", "set-limit", "1.5G"])
            .unwrap()
            .command
        else {
            panic!("set-limit expected")
        };
        assert_eq!(size, 3 << 29);
        for args in [
            vec!["jwm-remote", "clipboard-staging"],
            vec!["jwm-remote", "clipboard-staging", "set-limit"],
            vec!["jwm-remote", "clipboard-staging", "set-limit", "0"],
            vec!["jwm-remote", "clipboard-staging", "status", "1G"],
            vec!["jwm-remote", "clipboard-staging", "disable", "1G"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
        let help = Cli::try_parse_from(["jwm-remote", "clipboard-staging", "--help"])
            .unwrap_err()
            .to_string();
        for expected in [
            "No cap is enabled by default",
            "set-limit",
            "disable",
            "not a disk quota",
            "No setting is forwarded over SSH",
        ] {
            assert!(help.contains(expected), "missing {expected}");
        }
        let help = Cli::try_parse_from(["jwm-remote", "clipboard-staging", "set-limit", "--help"])
            .unwrap_err()
            .to_string();
        assert!(help.contains("<SIZE>"));
        assert!(!help.contains("[default:"));
    }

    #[test]
    fn staging_commands_cannot_be_saved_or_overridden_per_connection() {
        for tail in [vec!["status"], vec!["set-limit", "1G"], vec!["disable"]] {
            let mut args = vec!["jwm-remote", "clipboard-staging"];
            args.extend(tail);
            let command = Cli::try_parse_from(args).unwrap().command;
            let error = profile_from_command(command).unwrap_err();
            assert!(error.to_string().contains("only connect and ssh"));
        }
        for args in [
            vec![
                "jwm-remote",
                "ssh",
                "desk",
                "--clipboard-staging-limit",
                "1G",
            ],
            vec!["jwm-remote", "host", "--clipboard-staging-limit", "1G"],
            vec![
                "jwm-remote",
                "connect",
                "desk",
                "--clipboard-staging-limit",
                "1G",
            ],
            vec![
                "jwm-remote",
                "profile",
                "run",
                "desk",
                "--clipboard-staging-limit",
                "1G",
            ],
            vec![
                "jwm-remote",
                "clipboard-staging",
                "status",
                "--directory",
                "/tmp/other",
            ],
            vec![
                "jwm-remote",
                "clipboard-staging",
                "status",
                "--display",
                ":0",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
        let profile = profile_from_command(
            Cli::try_parse_from(["jwm-remote", "ssh", "desk"])
                .unwrap()
                .command,
        )
        .unwrap();
        let mut value = serde_json::to_value(profile).unwrap();
        value["viewer"]["clipboard_staging_limit"] = 1024.into();
        assert!(Profile::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn staging_status_output_is_exact_and_escapes_the_local_path() {
        let mut output = Vec::new();
        let status = StagingBudgetStatus {
            initialized: false,
            limit_bytes: None,
            reserved_bytes: 123,
            reservations: 2,
        };
        write_staging_status(&mut output, Path::new("/cache\nFAKE"), &status).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            concat!(
                "Local clipboard staging root: \"/cache\\nFAKE\"\n",
                "Admission cap: disabled (uncapped)\n",
                "Reserved archive bytes: 123\n",
                "Reservations: 2\n",
                "Accounting: not initialized (read-only inventory)\n",
                "Scope: cooperating processes using this cache root; not a filesystem quota.\n",
            )
        );
        let mut output = Vec::new();
        let status = StagingBudgetStatus {
            initialized: true,
            limit_bytes: Some(u64::MAX),
            ..status
        };
        write_staging_status(&mut output, Path::new("/cache"), &status).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Admission cap: 18446744073709551615 bytes\n"));
        assert!(output.contains("Accounting: initialized\n"));
    }

    #[test]
    fn staging_status_on_missing_root_does_not_create_anything() {
        let absent_parent =
            std::env::temp_dir().join(format!("jwm-staging-cli-{:032x}", rand::random::<u128>()));
        let root = absent_parent.join("remote-clipboard");
        assert!(!absent_parent.exists());
        let mut output = Vec::new();
        run_clipboard_staging(ClipboardStagingAction::Status, &root, &mut output).unwrap();
        assert!(!absent_parent.exists());
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Admission cap: disabled (uncapped)\n"));
        assert!(output.contains("Reserved archive bytes: 0\n"));
    }

    #[test]
    fn staging_invalid_limit_emits_no_success_and_creates_no_root() {
        let root = std::env::temp_dir().join(format!(
            "jwm-staging-cli-invalid-{:032x}",
            rand::random::<u128>()
        ));
        assert!(!root.exists());
        let mut output = Vec::new();
        let error = run_clipboard_staging(
            ClipboardStagingAction::SetLimit { size: 0 },
            &root,
            &mut output,
        )
        .unwrap_err();
        assert!(error.to_string().contains("disable"));
        assert!(output.is_empty());
        assert!(!root.exists());
    }

    #[test]
    fn staging_command_diagnostics_are_fixed_and_content_free() {
        for (tail, expected) in [
            (vec!["status"], "clipboard_staging_status"),
            (
                vec!["set-limit", "123456789"],
                "clipboard_staging_set_limit",
            ),
            (vec!["disable"], "clipboard_staging_disable"),
        ] {
            let mut args = vec!["jwm-remote", "clipboard-staging", "--diagnostics-json"];
            args.extend(tail);
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(cli.diagnostics_json);
            assert_eq!(diagnostic_command(&cli.command), expected);
            let mut output = Vec::new();
            emit_diagnostic(&mut output, expected, &Ok(())).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(value["command"], expected);
            assert_eq!(value["schema_version"], 2);
            assert_eq!(value["outcome"], "success");
            assert!(value.get("limit_bytes").is_none());
            output.clear();
            let error = Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "PRIVATE /cache/user 123456789 reserved bytes",
            )
            .into());
            emit_diagnostic(&mut output, expected, &error).unwrap();
            let text = String::from_utf8(output).unwrap();
            assert!(!text.contains("PRIVATE"));
            assert!(!text.contains("/cache/user"));
            assert!(!text.contains("123456789"));
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["outcome"], "error");
            assert_eq!(value["category"], "io");
            assert_eq!(value["io_kind"], "would_block");
        }
    }

    #[test]
    fn replay_cli_is_offline_bounded_and_not_a_profile() {
        let command = Cli::try_parse_from([
            "jwm-remote",
            "replay-benchmark",
            "--width",
            "17",
            "--height",
            "9",
            "--frames",
            "2",
            "--warmup",
            "0",
        ])
        .unwrap()
        .command;
        run(command).unwrap();
        let command = Cli::try_parse_from(["jwm-remote", "replay-benchmark"])
            .unwrap()
            .command;
        assert!(profile_from_command(command).is_err());
        for args in [
            ["--width", "0"],
            ["--height", "1081"],
            ["--frames", "301"],
            ["--warmup", "31"],
            ["--quality", "101"],
        ] {
            assert!(
                Cli::try_parse_from(["jwm-remote", "replay-benchmark", args[0], args[1]]).is_err()
            );
        }
        let command = Cli::try_parse_from([
            "jwm-remote",
            "replay-benchmark",
            "--width",
            "1920",
            "--height",
            "1080",
            "--frames",
            "300",
        ])
        .unwrap()
        .command;
        assert!(run(command).is_err());
    }

    #[test]
    fn source_grants_are_explicit_bounded_and_deduplicated() {
        let Command::Host {
            monitor, shares, ..
        } = Cli::try_parse_from([
            "jwm-remote",
            "host",
            "--monitor",
            "DP-0",
            "--share-monitor",
            "DP-1",
            "--share-monitor",
            "DP-0",
            "--share-monitor",
            "DP-1",
            "--share-region",
            "100x80+0+0",
        ])
        .unwrap()
        .command
        else {
            panic!("host expected")
        };
        let initial = CaptureArea::Monitor(monitor.unwrap());
        let areas = shares.areas(&initial).unwrap();
        assert_eq!(areas.len(), 2);
        assert_eq!(areas[0], CaptureArea::Monitor("DP-1".into()));
        assert!(!areas.contains(&CaptureArea::Root));
        assert!(ShareArgs::default().areas(&initial).unwrap().is_empty());
        let root = ShareArgs {
            share_root: true,
            ..ShareArgs::default()
        };
        assert_eq!(root.areas(&initial).unwrap(), vec![CaptureArea::Root]);
        assert!(root.areas(&CaptureArea::Root).unwrap().is_empty());
        let too_many = ShareArgs {
            share_monitor: vec!["DP-1".into(); 32],
            ..ShareArgs::default()
        };
        assert!(too_many.areas(&initial).is_err());
        for name in ["".to_owned(), "a".repeat(257), "bad\nname".into()] {
            assert!(parse_monitor(&name).is_err());
        }
    }

    #[test]
    fn profiles_reject_extra_source_grants_instead_of_losing_them() {
        for flag in [
            vec!["--share-root"],
            vec!["--share-monitor", "DP-1"],
            vec!["--share-region", "10x10+0+0"],
        ] {
            let mut args = vec!["jwm-remote", "ssh", "desk", "--view-only"];
            args.extend(flag);
            assert!(profile_from_command(Cli::try_parse_from(args).unwrap().command).is_err());
        }
    }

    #[test]
    fn decoded_frame_limits_are_bounded_and_independent_of_viewport() {
        for command in ["host", "connect", "ssh"] {
            let base = if command == "host" {
                vec!["jwm-remote", command]
            } else {
                vec!["jwm-remote", command, "desk"]
            };
            for (flag, maximum) in [
                ("--frame-max-width", "16384"),
                ("--frame-max-height", "16384"),
                ("--frame-max-pixels", "67108864"),
            ] {
                for good in ["1", maximum] {
                    let mut args = base.clone();
                    args.extend([flag, good]);
                    assert!(Cli::try_parse_from(args).is_ok());
                }
                for bad in ["0", "-1", "18446744073709551616", "67108865"] {
                    let mut args = base.clone();
                    args.extend([flag, bad]);
                    assert!(Cli::try_parse_from(args).is_err());
                }
            }
            let mut args = base.clone();
            args.push("--help");
            let help = Cli::try_parse_from(args).unwrap_err().to_string();
            assert!(help.contains("--frame-max-width"));
            assert!(help.contains("--frame-max-height"));
            assert!(help.contains("--frame-max-pixels"));
        }
        let Command::Host {
            frame_limits,
            max_width,
            ..
        } = Cli::try_parse_from([
            "jwm-remote",
            "host",
            "--max-width",
            "1280",
            "--frame-max-width",
            "800",
            "--frame-max-height",
            "600",
            "--frame-max-pixels",
            "300000",
        ])
        .unwrap()
        .command
        else {
            panic!()
        };
        assert_eq!(max_width, 1280);
        assert_eq!(
            frame_limits.limits(),
            FrameLimits {
                max_width: 800,
                max_height: 600,
                max_pixels: 300000
            }
        );
    }

    #[test]
    fn profile_frame_limits_roundtrip_and_legacy_defaults_are_safe() {
        for transport in ["connect", "ssh"] {
            let profile = profile_from_command(
                Cli::try_parse_from([
                    "jwm-remote",
                    transport,
                    "desk",
                    "--frame-max-width",
                    "800",
                    "--frame-max-height",
                    "600",
                    "--frame-max-pixels",
                    "300000",
                ])
                .unwrap()
                .command,
            )
            .unwrap();
            assert_eq!(validated_profile(profile.clone()).unwrap(), profile);
            let mut value = serde_json::to_value(&profile).unwrap();
            for field in ["frame_max_width", "frame_max_height", "frame_max_pixels"] {
                value["viewer"].as_object_mut().unwrap().remove(field);
            }
            let legacy = Profile::decode(&serde_json::to_vec(&value).unwrap()).unwrap();
            let command = command_from_profile(&legacy).unwrap();
            let limits = match command {
                Command::Connect { frame_limits, .. } | Command::Ssh { frame_limits, .. } => {
                    frame_limits.limits()
                }
                _ => panic!(),
            };
            assert_eq!(limits, FrameLimits::default());
            for field in ["frame_max_width", "frame_max_height", "frame_max_pixels"] {
                let mut bad = value.clone();
                bad["viewer"][field] = 0.into();
                let bad = Profile::decode(&serde_json::to_vec(&bad).unwrap()).unwrap();
                assert!(validated_profile(bad).is_err());
            }
        }
    }

    #[test]
    fn reconnect_is_explicit_bounded_and_cannot_be_saved_or_used_by_ssh() {
        let Command::Connect {
            reconnect_attempts, ..
        } = Cli::try_parse_from(["jwm-remote", "connect", "host"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert_eq!(reconnect_attempts, 0);
        for n in ["0", "1", "5"] {
            assert!(
                Cli::try_parse_from(["jwm-remote", "connect", "host", "--reconnect-attempts", n])
                    .is_ok()
            );
        }
        for n in ["6", "256", "-1"] {
            assert!(
                Cli::try_parse_from(["jwm-remote", "connect", "host", "--reconnect-attempts", n])
                    .is_err()
            );
        }
        let cli =
            Cli::try_parse_from(["jwm-remote", "connect", "host", "--reconnect-attempts", "1"])
                .unwrap();
        assert!(profile_from_command(cli.command).is_err());
        assert!(
            Cli::try_parse_from(["jwm-remote", "ssh", "host", "--reconnect-attempts", "1"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "profile",
                "run",
                "host",
                "--reconnect-attempts",
                "1"
            ])
            .is_err()
        );
    }

    use super::*;

    #[test]
    fn clipboard_scope_requires_consent_and_roundtrips_profiles() {
        for transport in ["connect", "ssh", "host"] {
            let mut base = vec!["jwm-remote", transport];
            if transport != "host" {
                base.push("desk");
            }
            assert!(Cli::try_parse_from(base.clone()).is_ok());
            for (flag, value) in [
                ("--clipboard-mode", "text"),
                ("--clipboard-direction", "to-host"),
                ("--clipboard-mode", "all"),
                ("--clipboard-direction", "both"),
            ] {
                let mut args = base.clone();
                args.extend([flag, value]);
                assert!(Cli::try_parse_from(args).is_err());
            }
            base.push(if transport == "host" {
                "--allow-clipboard"
            } else {
                "--clipboard"
            });
            for mode in ["text", "text-images", "all"] {
                for direction in ["both", "to-host", "from-host"] {
                    let mut args = base.clone();
                    args.extend(["--clipboard-mode", mode, "--clipboard-direction", direction]);
                    let command = Cli::try_parse_from(args).unwrap().command;
                    if transport != "host" {
                        let mut profile = profile_from_command(command).unwrap();
                        assert!(profile.viewer.clipboard);
                        assert_eq!(profile.viewer.clipboard_mode.to_string(), mode);
                        assert_eq!(profile.viewer.clipboard_direction.to_string(), direction);
                        assert_eq!(validated_profile(profile.clone()).unwrap(), profile);
                        profile.viewer.clipboard = false;
                        assert_eq!(validated_profile(profile.clone()).unwrap(), profile);
                        match command_from_profile(&profile).unwrap() {
                            Command::Connect { clipboard, .. } | Command::Ssh { clipboard, .. } => {
                                assert!(!clipboard)
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
            for (flag, value) in [
                ("--clipboard-mode", "image"),
                ("--clipboard-direction", "outgoing"),
            ] {
                let mut args = base.clone();
                args.extend([flag, value]);
                assert!(Cli::try_parse_from(args).is_err());
            }
        }
        for flag in ["--clipboard-mode", "--clipboard-direction"] {
            assert!(
                Cli::try_parse_from(["jwm-remote", "profile", "run", "desk", flag, "all"]).is_err()
            );
        }
    }

    #[test]
    fn profile_cli_lifecycle_only_uses_an_explicit_temporary_store() {
        let directory =
            std::env::temp_dir().join(format!("jwm-profile-cli-{:032x}", rand::random::<u128>()));
        run_profile(Some(directory.clone()), ProfileAction::Init).unwrap();
        let save = |replace| ProfileAction::Save {
            name: "desk".into(),
            replace,
            connection: ["ssh", "user@desk", "--view-only"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        };
        run_profile(Some(directory.clone()), save(false)).unwrap();
        assert!(run_profile(Some(directory.clone()), save(false)).is_err());
        run_profile(Some(directory.clone()), save(true)).unwrap();
        run_profile(Some(directory.clone()), ProfileAction::List).unwrap();
        run_profile(
            Some(directory.clone()),
            ProfileAction::Show {
                name: "desk".into(),
            },
        )
        .unwrap();
        run_profile(
            Some(directory.clone()),
            ProfileAction::Remove {
                name: "desk".into(),
            },
        )
        .unwrap();
        assert!(Store::open(&directory).unwrap().list().unwrap().is_empty());
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn profile_cli_roundtrips_settings_without_starting_a_session() {
        for args in [
            vec![
                "jwm-remote",
                "profile",
                "save",
                "desk",
                "--",
                "ssh",
                "user@desk",
                "--monitor",
                "DP-0",
                "--fps",
                "24",
                "--jpeg-quality",
                "70",
                "--max-width",
                "1280",
                "--view-only",
            ],
            vec![
                "jwm-remote",
                "profile",
                "save",
                "desk",
                "--",
                "connect",
                "host",
                "--key-file",
                "/tmp/key",
                "--clipboard",
                "--clipboard-file-limit",
                "512M",
                "--grab-input",
                "--display",
                ":1",
            ],
        ] {
            let Command::Profile {
                action:
                    ProfileAction::Save {
                        name, connection, ..
                    },
                ..
            } = Cli::try_parse_from(args).unwrap().command
            else {
                panic!("save expected");
            };
            assert_eq!(name, "desk");
            let command =
                Cli::try_parse_from(std::iter::once("jwm-remote".into()).chain(connection))
                    .unwrap()
                    .command;
            let profile = profile_from_command(command).unwrap();
            assert_eq!(validated_profile(profile.clone()).unwrap(), profile);
            let encoded = serde_json::to_vec(&profile).unwrap();
            assert_eq!(Profile::decode(&encoded).unwrap(), profile);
        }
        for args in [
            vec!["jwm-remote", "profile", "list"],
            vec![
                "jwm-remote",
                "profile",
                "--directory",
                "/tmp/profiles",
                "show",
                "desk",
            ],
            vec![
                "jwm-remote",
                "profile",
                "run",
                "desk",
                "--view-only",
                "--no-clipboard",
            ],
            vec!["jwm-remote", "profile", "remove", "desk"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        let Command::Profile {
            action:
                ProfileAction::Save {
                    replace,
                    connection,
                    ..
                },
            ..
        } = Cli::try_parse_from([
            "jwm-remote",
            "profile",
            "save",
            "desk",
            "--replace",
            "--",
            "ssh",
            "desk",
        ])
        .unwrap()
        .command
        else {
            panic!("save expected");
        };
        assert!(replace);
        assert_eq!(connection, ["ssh", "desk"]);
        assert!(
            Cli::try_parse_from(["jwm-remote", "profile", "run", "desk", "--clipboard"]).is_err()
        );
    }

    #[test]
    fn window_position_profiles_and_signed_cli_are_explicit_and_compatible() {
        for transport in ["connect", "ssh"] {
            let command = Cli::try_parse_from([
                "jwm-remote",
                transport,
                "desk",
                "--view-only",
                "--window-position",
                "-32768,32767",
            ])
            .unwrap()
            .command;
            let profile = profile_from_command(command).unwrap();
            assert_eq!(
                profile.viewer.window_position,
                Some(WindowPosition {
                    x: -32768,
                    y: 32767
                })
            );
            assert_eq!(validated_profile(profile.clone()).unwrap(), profile);
            assert!(profile.viewer.view_only);
            assert!(!profile.viewer.clipboard);
            let mut encoded = serde_json::to_value(&profile).unwrap();
            encoded["viewer"]
                .as_object_mut()
                .unwrap()
                .remove("window_position");
            let legacy =
                validated_profile(Profile::decode(&serde_json::to_vec(&encoded).unwrap()).unwrap())
                    .unwrap();
            assert_eq!(legacy.viewer.window_position, None);
            encoded["viewer"]["window_position"] = serde_json::json!({"x":32768,"y":0});
            assert!(Profile::decode(&serde_json::to_vec(&encoded).unwrap()).is_err());
            encoded["viewer"]["window_position"] = serde_json::json!({"x":0,"y":0,"source":1});
            assert!(Profile::decode(&serde_json::to_vec(&encoded).unwrap()).is_err());
            for value in ["32768,0", "0,-32769", "1,2,3", " 1,2"] {
                assert!(
                    Cli::try_parse_from([
                        "jwm-remote",
                        transport,
                        "desk",
                        "--window-position",
                        value
                    ])
                    .is_err()
                );
            }
        }
        assert!(Cli::try_parse_from(["jwm-remote", "host", "--window-position", "1,2"]).is_err());
    }

    #[test]
    fn window_size_cli_and_profiles_roundtrip_without_changing_permissions() {
        for transport in ["connect", "ssh"] {
            let command = Cli::try_parse_from([
                "jwm-remote",
                transport,
                "desk",
                "--view-only",
                "--window-size",
                "1280x720",
                "--frame-max-pixels",
                "10000",
            ])
            .unwrap()
            .command;
            let profile = profile_from_command(command).unwrap();
            assert_eq!(
                profile.viewer.window_size.unwrap().dimensions(),
                (1280, 720)
            );
            assert_eq!(profile.viewer.frame_max_pixels, Some(10000));
            assert!(profile.viewer.view_only);
            assert!(!profile.viewer.clipboard);
            assert_eq!(validated_profile(profile.clone()).unwrap(), profile);
            let mut legacy = serde_json::to_value(&profile).unwrap();
            legacy["viewer"]
                .as_object_mut()
                .unwrap()
                .remove("window_size");
            let legacy =
                validated_profile(Profile::decode(&serde_json::to_vec(&legacy).unwrap()).unwrap())
                    .unwrap();
            assert_eq!(legacy.viewer.window_size, None);
            for size in ["0x720", "8193x1", "8192x2049", "1280X720", "1x1x1"] {
                assert!(
                    Cli::try_parse_from(["jwm-remote", transport, "desk", "--window-size", size])
                        .is_err()
                );
            }
            let mut invalid = profile.clone();
            invalid.viewer.window_size = Some(WindowSize {
                width: 8192,
                height: 2049,
            });
            assert!(validated_profile(invalid).is_err());
            let mut edited = serde_json::to_value(&profile).unwrap();
            edited["viewer"]["window_size"]["position"] = 1.into();
            assert!(Profile::decode(&serde_json::to_vec(&edited).unwrap()).is_err());
        }
        assert!(Cli::try_parse_from(["jwm-remote", "host", "--window-size", "1280x720"]).is_err());
    }

    #[test]
    fn saved_profiles_reject_commands_secrets_and_invalid_semantics() {
        // Invalid monitor names are rejected at the CLI boundary, before a
        // profile can be constructed or a host connection can be attempted.
        assert!(
            Cli::try_parse_from(["jwm-remote", "ssh", "desk", "--monitor", "bad\nname",]).is_err()
        );
        for args in [
            vec!["jwm-remote", "host"],
            vec!["jwm-remote", "ssh", "desk", "--remote-command", "custom"],
            vec![
                "jwm-remote",
                "ssh",
                "desk",
                "--ssh-option",
                "ProxyCommand=custom",
            ],
            vec![
                "jwm-remote",
                "connect",
                "desk",
                "--key-file",
                "relative.key",
            ],
            vec!["jwm-remote", "ssh", "bad host"],
            vec!["jwm-remote", "ssh", "desk", "--escape-key", "not_a_keysym"],
        ] {
            assert!(profile_from_command(Cli::try_parse_from(args).unwrap().command).is_err());
        }
        let profile = profile_from_command(
            Cli::try_parse_from(["jwm-remote", "ssh", "desk", "--view-only"])
                .unwrap()
                .command,
        )
        .unwrap();
        assert!(profile.viewer.view_only);
        assert!(!profile.viewer.clipboard);
        let mut bad = profile.clone();
        bad.viewer.grab_input = true;
        assert!(validated_profile(bad).is_err());
        for (fps, quality, width) in [
            (Some(0), None, None),
            (None, Some(101), None),
            (None, None, Some(1)),
        ] {
            let mut bad = profile.clone();
            bad.connection = Connection::Ssh {
                destination: "desk".into(),
                monitor: None,
                fps,
                jpeg_quality: quality,
                max_width: width,
            };
            assert!(validated_profile(bad).is_err());
        }
    }

    #[test]
    fn interactive_defaults_keep_native_fit_and_congestion_escape_floor() {
        let Command::Host {
            fps,
            jpeg_quality,
            jpeg_quality_floor,
            max_width,
            ..
        } = Cli::try_parse_from(["jwm-remote", "host"]).unwrap().command
        else {
            panic!("host expected")
        };
        assert_eq!((fps, jpeg_quality, max_width), (30, 85, 0));
        assert_eq!(
            resolve_quality_floor(jpeg_quality, jpeg_quality_floor).unwrap(),
            40
        );
        let Command::Host {
            fps,
            jpeg_quality,
            max_width,
            ..
        } = Cli::try_parse_from([
            "jwm-remote",
            "host",
            "--fps",
            "12",
            "--jpeg-quality",
            "70",
            "--max-width",
            "1280",
        ])
        .unwrap()
        .command
        else {
            panic!("host expected")
        };
        assert_eq!((fps, jpeg_quality, max_width), (12, 70, 1280));
    }

    #[test]
    fn host_listen_short_forms_preserve_defaults_permissions_and_port_zero() {
        let Command::Host {
            listen,
            allow_lan,
            allow_input,
            allow_clipboard,
            ..
        } = Cli::try_parse_from(["jwm-remote", "host"]).unwrap().command
        else {
            panic!("host variant expected")
        };
        assert_eq!(listen, "127.0.0.1:48221");
        assert!(!allow_lan && !allow_input && !allow_clipboard);
        for (input, expected) in [
            ("192.0.2.10", "192.0.2.10:48221"),
            ("localhost", "localhost:48221"),
            ("hostname:0", "hostname:0"),
            ("127.0.0.1:0", "127.0.0.1:0"),
            ("::1", "[::1]:48221"),
            ("[::1]", "[::1]:48221"),
            ("[::1]:0", "[::1]:0"),
        ] {
            let Command::Host {
                listen,
                allow_lan,
                allow_input,
                allow_clipboard,
                ..
            } = Cli::try_parse_from(["jwm-remote", "host", "--listen", input])
                .unwrap()
                .command
            else {
                panic!("host variant expected")
            };
            assert_eq!(listen, expected);
            assert!(!allow_lan && !allow_input && !allow_clipboard);
        }
        let Command::Host {
            listen, allow_lan, ..
        } = Cli::try_parse_from([
            "jwm-remote",
            "host",
            "--listen",
            "192.0.2.10",
            "--allow-lan",
        ])
        .unwrap()
        .command
        else {
            panic!("host variant expected")
        };
        assert_eq!(listen, "192.0.2.10:48221");
        assert!(allow_lan);
    }

    #[test]
    fn host_listen_help_matches_parser_and_bad_addresses_fail_before_startup() {
        for invalid in ["", ":48221", "host:65536", "host:", "[::1]:", "host\x1b"] {
            let error =
                Cli::try_parse_from(["jwm-remote", "host", "--listen", invalid]).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
            assert!(error.to_string().contains("host --listen"));
        }
        let help = Cli::try_parse_from(["jwm-remote", "host", "--help"]).unwrap_err();
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = help.to_string();
        assert!(help.contains("LOCAL_HOST[:PORT]"));
        assert!(help.contains("default 48221"));
        assert!(help.contains("port 0"));
    }

    #[test]
    fn help_and_short_examples_match_the_parser() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        for command in [
            vec!["jwm-remote", "--help"],
            vec!["jwm-remote", "keygen", "--help"],
            vec!["jwm-remote", "doctor", "--help"],
            vec!["jwm-remote", "host", "--help"],
            vec!["jwm-remote", "connect", "--help"],
            vec!["jwm-remote", "ssh", "--help"],
        ] {
            let error = Cli::try_parse_from(command).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        }
        for command in [
            vec!["jwm-remote", "keygen"],
            vec!["jwm-remote", "doctor"],
            vec![
                "jwm-remote",
                "host",
                "--listen",
                "0.0.0.0:48221",
                "--allow-lan",
            ],
            vec!["jwm-remote", "connect", "host", "--view-only"],
            vec![
                "jwm-remote",
                "connect",
                "[::1]:1234",
                "--key-file",
                "private.key",
                "--grab-input",
            ],
        ] {
            assert!(Cli::try_parse_from(command).is_ok());
        }
        let Command::Ssh {
            destination,
            monitor,
            ssh_options,
            ..
        } = Cli::try_parse_from([
            "jwm-remote",
            "ssh",
            "ubuntu@desk",
            "--grab-input",
            "--monitor",
            "DP-0",
            "--ssh-option",
            "Port=2222",
            "--ssh-option",
            "User=me",
        ])
        .unwrap()
        .command
        else {
            panic!("ssh variant expected")
        };
        assert_eq!(destination, "ubuntu@desk");
        assert!(
            Cli::try_parse_from(["jwm-remote", "ssh", "desk", "--notify"])
                .unwrap()
                .notify
        );
        assert_eq!(monitor.as_deref(), Some("DP-0"));
        assert_eq!(ssh_options, ["Port=2222", "User=me"]);
        for conflicting in [
            vec!["jwm-remote", "ssh", "desk", "--view-only", "--grab-input"],
            vec!["jwm-remote", "host", "--key-stdin", "--key-file", "k"],
        ] {
            assert!(Cli::try_parse_from(conflicting).is_err());
        }
        assert!(
            Cli::try_parse_from(["jwm-remote", "host", "--key-stdin", "--display", "auto"]).is_ok()
        );

        let Command::Connect {
            address, key_file, ..
        } = Cli::try_parse_from(["jwm-remote", "connect", "host"])
            .unwrap()
            .command
        else {
            panic!("connect variant expected")
        };
        assert_eq!(address, "host:48221");
        assert!(key_file.is_none());
        assert_eq!(
            resolve_key_path(Some(PathBuf::from("explicit.key"))).unwrap(),
            PathBuf::from("explicit.key")
        );
    }

    #[test]
    fn window_host_is_explicit_single_use_and_view_only() {
        let base = ["jwm-remote", "host", "--window", "0x200001", "--once"];
        assert!(Cli::try_parse_from(base).is_ok());
        assert!(Cli::try_parse_from(["jwm-remote", "host", "--window", "1"]).is_err());
        for flag in ["--allow-input", "--allow-clipboard", "--share-root"] {
            let mut args = base.to_vec();
            args.push(flag);
            assert!(Cli::try_parse_from(args).is_err());
        }
        for id in ["0", "0x0", "-1", "4294967296", "window-name"] {
            assert!(parse_window_id(id).is_err());
        }
        assert_eq!(parse_window_id("0x200001").unwrap(), 0x200001);
    }

    #[test]
    fn cli_rejects_conflicting_permissions_and_malformed_addresses() {
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "connect",
                "host",
                "--view-only",
                "--grab-input"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["jwm-remote", "connect", "host:99999"]).is_err());
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "host",
                "--monitor",
                "DP-1",
                "--region",
                "100x100+0+0"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["jwm-remote", "doctor", "--allow-lan"]).is_err());
        assert!(Cli::try_parse_from(["jwm-remote", "doctor", "--allow-input"]).is_err());
        assert!(Cli::try_parse_from(["jwm-remote", "doctor", "--clipboard"]).is_err());
    }

    #[test]
    fn unsafe_ranges_fail_at_cli_boundary() {
        assert_eq!(parse_fps("60"), Ok(60));
        assert!(parse_fps("0").is_err());
        assert!(parse_quality("101").is_err());
        assert_eq!(resolve_quality_floor(70, None), Ok(40));
        assert_eq!(resolve_quality_floor(25, None), Ok(25));
        assert_eq!(resolve_quality_floor(70, Some(50)), Ok(50));
        assert!(resolve_quality_floor(39, Some(40)).is_err());
        assert_eq!(parse_max_width("0"), Ok(0));
        assert!(parse_max_width("100").is_err());
    }

    #[test]
    fn fixed_quality_conflicts_with_an_explicit_adaptive_floor() {
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "host",
                "--key-file",
                "key",
                "--fixed-jpeg-quality",
                "--jpeg-quality-floor",
                "40",
            ])
            .is_err()
        );
    }
    #[test]
    fn diagnostics_global_flag_and_defaults() {
        for args in [
            vec!["jwm-remote", "--diagnostics-json", "doctor"],
            vec!["jwm-remote", "doctor", "--diagnostics-json"],
            vec!["jwm-remote", "profile", "--diagnostics-json", "list"],
            vec!["jwm-remote", "profile", "list", "--diagnostics-json"],
        ] {
            assert!(Cli::try_parse_from(args).unwrap().diagnostics_json);
        }
        let cli = Cli::try_parse_from(["jwm-remote", "doctor"]).unwrap();
        assert!(!cli.diagnostics_json);
        assert!(!cli.notify);
    }

    #[test]
    fn diagnostics_schema_is_fixed_and_private_text_is_absent() {
        let mut output = Vec::new();
        emit_diagnostic(&mut output, "doctor", &Ok(())).unwrap();
        assert_eq!(output.iter().filter(|&&b| b == b'\n').count(), 1);
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "jwm_remote_command_result", "schema_version": 2,
                "command": "doctor", "outcome": "success", "stage": null,
                "category": "success", "io_kind": null, "peer_close_code": null,
                "cleanup": "unknown",
                "reconnect_certified": false
            })
        );
        let error = Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "PRIVATE /home/alice/key host:48221 profile-secret \n\u{1b}[31m",
        )
        .into());
        output.clear();
        emit_diagnostic(&mut output, "profile_run", &error).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains("PRIVATE"));
        assert!(!text.contains("alice"));
        assert!(!text.contains("host:48221"));
        assert!(!text.contains("profile-secret"));
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["category"], "io");
        assert_eq!(value["io_kind"], "permission_denied");
        assert_eq!(value["cleanup"], "unknown");
    }

    #[test]
    fn diagnostic_writer_failure_is_returned_without_changing_result() {
        struct BrokenWriter;
        impl std::io::Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let result = Ok(());
        assert!(emit_diagnostic(&mut BrokenWriter, "doctor", &result).is_err());
        assert!(result.is_ok());
    }

    #[test]
    fn terminal_emitter_has_one_main_call_site() {
        let source = include_str!("jwm_remote.rs");
        let main = source
            .split("fn main() ")
            .nth(1)
            .unwrap()
            .split("fn diagnostic_command")
            .next()
            .unwrap();
        assert_eq!(main.matches("emit_diagnostic(").count(), 1);
        assert_eq!(main.matches("run(cli.command)").count(), 1);
        let run = source
            .split("fn run(command:")
            .nth(1)
            .unwrap()
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!run.contains("emit_diagnostic("));
    }
}

#[cfg(test)]
mod explicit_resize_tests {
    use super::*;
    #[test]
    fn explicit_resize_flags_require_scope_and_cannot_be_saved() {
        let args = [
            "jwm-remote",
            "connect",
            "127.0.0.1:9876",
            "--view-only",
            "--resize-source",
            "480x320",
        ];
        let parsed = Cli::try_parse_from(args).unwrap();
        assert!(profile_from_command(parsed.command).is_err());
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "connect",
                "host",
                "--resize-source",
                "480x320"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "connect",
                "host",
                "--view-only",
                "--resize-source",
                "480x320",
                "--reconnect-attempts",
                "1"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["jwm-remote", "host", "--allow-window-resize"]).is_err());
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "host",
                "--window",
                "0x200001",
                "--once",
                "--allow-window-resize"
            ])
            .is_ok()
        );
    }
}

#[cfg(test)]
mod wayland_source_cli_tests {
    use super::*;
    #[test]
    fn wayland_selection_is_local_single_use_and_has_no_x11_permissions() {
        assert!(Cli::try_parse_from(["jwm-remote", "host", "--wayland-select", "--once"]).is_ok());
        assert!(Cli::try_parse_from(["jwm-remote", "host", "--wayland-select"]).is_err());
        assert!(
            Cli::try_parse_from(["jwm-remote", "host", "--wayland-display", "wayland-1"]).is_err()
        );
        for denied in [
            "--allow-input",
            "--allow-clipboard",
            "--allow-window-resize",
            "--key-stdin",
            "--share-root",
        ] {
            assert!(
                Cli::try_parse_from(["jwm-remote", "host", "--wayland-select", "--once", denied])
                    .is_err(),
                "{denied}"
            );
        }
        for denied in [
            "--display",
            "--window",
            "--monitor",
            "--region",
            "--capture-source",
        ] {
            assert!(
                Cli::try_parse_from([
                    "jwm-remote",
                    "host",
                    "--wayland-select",
                    "--once",
                    denied,
                    "1"
                ])
                .is_err(),
                "{denied}"
            );
        }
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "host",
                "--wayland-select",
                "--once",
                "--wayland-display",
                "wayland-private"
            ])
            .is_ok()
        );
    }
}

#[cfg(test)]
mod wayland_keyboard_cli_tests {
    use super::*;

    #[test]
    fn keyboard_lease_requires_explicit_native_host_and_scoped_viewer() {
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "host",
                "--wayland-select",
                "--once",
                "--allow-wayland-keyboard"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["jwm-remote", "host", "--allow-wayland-keyboard", "--once"])
                .is_err()
        );
        for denied in [
            "--allow-input",
            "--allow-clipboard",
            "--allow-window-resize",
        ] {
            assert!(
                Cli::try_parse_from([
                    "jwm-remote",
                    "host",
                    "--wayland-select",
                    "--once",
                    "--allow-wayland-keyboard",
                    denied
                ])
                .is_err()
            );
        }
        assert!(
            Cli::try_parse_from(["jwm-remote", "connect", "127.0.0.1", "--wayland-keyboard"])
                .is_ok()
        );
        for denied in ["--view-only", "--clipboard", "--grab-input"] {
            assert!(
                Cli::try_parse_from([
                    "jwm-remote",
                    "connect",
                    "127.0.0.1",
                    "--wayland-keyboard",
                    denied
                ])
                .is_err()
            );
        }
        for (flag, value) in [
            ("--resize-source", "800x600"),
            ("--reconnect-attempts", "1"),
        ] {
            assert!(
                Cli::try_parse_from([
                    "jwm-remote",
                    "connect",
                    "127.0.0.1",
                    "--wayland-keyboard",
                    flag,
                    value
                ])
                .is_err()
            );
        }
        let command =
            Cli::try_parse_from(["jwm-remote", "connect", "127.0.0.1", "--wayland-keyboard"])
                .unwrap()
                .command;
        assert!(profile_from_command(command).is_err());
    }
}

#[cfg(test)]
mod wayland_pointer_cli_tests {
    use super::*;
    #[test]
    fn pointer_permission_is_explicit_exclusive_and_not_persisted() {
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "host",
                "--wayland-select",
                "--once",
                "--allow-wayland-pointer"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["jwm-remote", "host", "--once", "--allow-wayland-pointer"])
                .is_err()
        );
        for other in [
            "--allow-wayland-keyboard",
            "--allow-input",
            "--allow-clipboard",
            "--allow-window-resize",
        ] {
            assert!(
                Cli::try_parse_from([
                    "jwm-remote",
                    "host",
                    "--wayland-select",
                    "--once",
                    "--allow-wayland-pointer",
                    other
                ])
                .is_err()
            );
        }
        for other in [
            "--wayland-keyboard",
            "--view-only",
            "--clipboard",
            "--grab-input",
        ] {
            assert!(
                Cli::try_parse_from([
                    "jwm-remote",
                    "connect",
                    "127.0.0.1",
                    "--wayland-pointer",
                    other
                ])
                .is_err()
            );
        }
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "connect",
                "127.0.0.1",
                "--wayland-pointer",
                "--reconnect-attempts",
                "1"
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from(["jwm-remote", "connect", "127.0.0.1", "--wayland-pointer"])
            .unwrap();
        assert!(profile_from_command(cli.command).is_err());
    }
}

#[cfg(test)]
mod wayland_scroll_cli_tests {
    use super::*;
    #[test]
    fn scroll_requires_explicit_pointer_and_cannot_be_saved() {
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "host",
                "--wayland-select",
                "--once",
                "--allow-wayland-scroll"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "jwm-remote",
                "host",
                "--wayland-select",
                "--once",
                "--allow-wayland-pointer",
                "--allow-wayland-scroll"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["jwm-remote", "connect", "127.0.0.1", "--wayland-scroll"])
                .is_err()
        );
        let cli = Cli::try_parse_from([
            "jwm-remote",
            "connect",
            "127.0.0.1",
            "--wayland-pointer",
            "--wayland-scroll",
        ])
        .unwrap();
        assert!(profile_from_command(cli.command).is_err());
        for restricted in [
            "--view-only",
            "--wayland-keyboard",
            "--clipboard",
            "--grab-input",
        ] {
            assert!(
                Cli::try_parse_from([
                    "jwm-remote",
                    "connect",
                    "127.0.0.1",
                    "--wayland-pointer",
                    "--wayland-scroll",
                    restricted
                ])
                .is_err()
            );
        }
    }
}

#[cfg(test)]
mod scroll_copy_cli_tests {
    use super::*;
    #[test]
    fn scroll_copy_is_host_only_default_off_and_does_not_grant_input() {
        for enabled in [false, true] {
            let mut args = vec!["jwm-remote", "host"];
            if enabled {
                args.push("--scroll-copy");
            }
            let Command::Host {
                scroll_copy,
                allow_input,
                allow_wayland_pointer,
                allow_wayland_scroll,
                allow_clipboard,
                ..
            } = Cli::try_parse_from(args).unwrap().command
            else {
                panic!("host expected")
            };
            assert_eq!(scroll_copy, enabled);
            assert!(
                !allow_input && !allow_wayland_pointer && !allow_wayland_scroll && !allow_clipboard
            );
        }
        for args in [
            vec!["jwm-remote", "connect", "127.0.0.1", "--scroll-copy"],
            vec!["jwm-remote", "ssh", "example", "--scroll-copy"],
        ] {
            assert!(
                Cli::try_parse_from(&args).is_err(),
                "unexpectedly accepted {args:?}"
            );
        }
        // Profile save deliberately captures raw connection arguments. Follow
        // run_profile's nested parse/validation boundary, not the outer parser.
        // Neither step opens a connection or writes a profile.
        for connection in [
            vec!["--scroll-copy"],
            vec!["connect", "127.0.0.1", "--scroll-copy"],
            vec!["ssh", "example", "--scroll-copy"],
            vec!["host", "--scroll-copy"],
        ] {
            let args = ["jwm-remote", "profile", "save", "example", "--"]
                .into_iter()
                .chain(connection.iter().copied())
                .collect::<Vec<_>>();
            let Command::Profile {
                action:
                    ProfileAction::Save {
                        connection: raw, ..
                    },
                ..
            } = Cli::try_parse_from(&args).unwrap().command
            else {
                panic!("profile save expected for {args:?}")
            };
            let parsed = Cli::try_parse_from(std::iter::once("jwm-remote".to_owned()).chain(raw));
            if connection[0] == "host" {
                let error = profile_from_command(parsed.unwrap().command).unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "only connect and ssh commands can be saved"
                );
            } else {
                assert!(
                    parsed.is_err(),
                    "nested profile connection accepted {args:?}"
                );
            }
        }
    }
}
