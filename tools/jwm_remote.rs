//! JWM-to-JWM remote desktop helper for trusted X11 LANs.

use clap::{Parser, Subcommand};
use jwm::remote::client::{ClientOptions, run_client};
use jwm::remote::host::{HostOptions, run_host};
use jwm::remote::key::generate_key_file;
use jwm::remote::usability::{default_key_path, key_metadata, normalize_address};
use jwm::remote::x11_capture::{CaptureArea, CaptureRegion, CaptureSource};
use jwm::remote::x11_viewer::DEFAULT_ESCAPE_KEY;
use jwm::remote::{RemoteResult, messages::APPLICATION_VERSION, protocol::TRANSPORT_VERSION};
use std::path::PathBuf;

const DEFAULT_JPEG_QUALITY_FLOOR: u8 = 40;

#[derive(Debug, Parser)]
#[command(
    name = "jwm-remote",
    version,
    about = "JWM-to-JWM remote viewing and control for trusted X11 LANs",
    after_help = "Start here: jwm-remote doctor\nCreate a key explicitly: jwm-remote keygen (never overwrites; see keygen --help).\nShare the same private key securely with your other machine.\nHost: jwm-remote host --listen 0.0.0.0:48221 --allow-lan\nViewer: jwm-remote connect HOST --view-only\nInput needs host --allow-input; clipboard requires explicit consent on both ends.\nDefault key: $XDG_CONFIG_HOME/jwm/remote.key, otherwise $HOME/.config/jwm/remote.key.\nNo command silently creates a key, opens LAN access, or enables clipboard sharing."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
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
        /// TCP listener. Non-loopback addresses also require --allow-lan.
        #[arg(long, default_value = "127.0.0.1:48221", value_name = "ADDRESS:PORT")]
        listen: String,

        /// Private key; defaults to the JWM key path shown in top-level help.
        #[arg(long, value_name = "PATH")]
        key_file: Option<PathBuf>,

        /// X11 display to share (defaults to DISPLAY).
        #[arg(long, value_name = "DISPLAY")]
        display: Option<String>,

        #[arg(long, default_value = "12", value_parser = parse_fps)]
        fps: u16,

        #[arg(long, default_value = "70", value_parser = parse_quality)]
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

        /// Ceiling on encoded width; 0 keeps native width.
        ///
        /// A viewer reports its window size and the host narrows below this
        /// automatically. A peer can never raise it.
        #[arg(long, default_value = "1280", value_parser = parse_max_width)]
        max_width: u16,

        /// auto prefers JWM's Composite overlay and falls back to root.
        #[arg(long, default_value = "auto", value_parser = parse_capture_source)]
        capture_source: CaptureSource,

        /// Share one RandR monitor instead of the whole root, by name.
        ///
        /// The root spans every monitor, so on a dual 1920x1080 desk it is
        /// 3840x1080 and each display arrives at less than half the requested
        /// width. Re-resolved by name when the layout changes.
        #[arg(long, value_name = "NAME", conflicts_with = "region")]
        monitor: Option<String>,

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

        /// Share the clipboard with the authenticated peer, both directions.
        ///
        /// Text marked secret by a password manager is never shared.
        #[arg(long)]
        allow_clipboard: bool,

        /// Exit after the first connection ends (useful for tests/scripts).
        #[arg(long)]
        once: bool,
    },
    /// Open a remote JWM desktop in a window on this JWM X11 session.
    Connect {
        /// Hostname/IP, optionally with port (default 48221). Use [IPv6]:PORT.
        #[arg(value_name = "HOST[:PORT]", value_parser = normalize_address)]
        address: String,

        /// Private key; defaults to the JWM key path shown in top-level help.
        #[arg(long, value_name = "PATH")]
        key_file: Option<PathBuf>,

        /// Local X11 display for the viewer window (defaults to DISPLAY).
        #[arg(long, value_name = "DISPLAY")]
        display: Option<String>,

        /// Never send local keyboard or pointer events.
        #[arg(long)]
        view_only: bool,

        /// Click the viewer to grab local input; double-tap the escape key to release.
        #[arg(long, conflicts_with = "view_only")]
        grab_input: bool,

        /// X keysym name that releases the grab when tapped twice quickly.
        ///
        /// A single press is forwarded to the remote machine as usual.
        #[arg(long, default_value = DEFAULT_ESCAPE_KEY, value_name = "KEYSYM")]
        escape_key: String,

        /// Share the clipboard with the host, both directions.
        ///
        /// The host must also be started with --allow-clipboard.
        #[arg(long)]
        clipboard: bool,
    },
}

fn main() -> std::process::ExitCode {
    match run(Cli::parse().command) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("jwm-remote: {error}");
            std::process::ExitCode::FAILURE
        }
    }
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

fn run(command: Command) -> RemoteResult<()> {
    match command {
        Command::Keygen { output } => {
            let output = resolve_key_path(output)?;
            if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
                if !parent.is_dir() {
                    return Err(std::io::Error::new(std::io::ErrorKind::NotFound,
                        format!("key parent directory does not exist: {:?}. Create a private directory first (see keygen --help); nothing was created", parent)).into());
                }
            }
            generate_key_file(&output)?;
            println!("Generated private remote key: {:?}", output);
            Ok(())
        }
        Command::Doctor { key_file, display } => run_doctor(resolve_key_path(key_file)?, display),
        Command::Host {
            listen,
            key_file,
            display,
            fps,
            jpeg_quality,
            jpeg_quality_floor,
            fixed_jpeg_quality,
            max_width,
            capture_source,
            monitor,
            region,
            allow_lan,
            allow_input,
            allow_clipboard,
            once,
        } => {
            let key_file = resolve_key_path(key_file)?;
            let jpeg_quality_floor = resolve_quality_floor(jpeg_quality, jpeg_quality_floor)?;
            let capture_area = match (monitor, region) {
                (Some(name), _) => CaptureArea::Monitor(name),
                (None, Some(region)) => CaptureArea::Region(region),
                (None, None) => CaptureArea::Root,
            };
            run_host(HostOptions {
                listen,
                key_file,
                display,
                fps,
                jpeg_quality,
                jpeg_quality_floor,
                fixed_jpeg_quality,
                max_width,
                capture_source,
                capture_area,
                allow_lan,
                allow_input,
                allow_clipboard,
                once,
            })
        }
        Command::Connect {
            address,
            key_file,
            display,
            view_only,
            grab_input,
            clipboard,
            escape_key,
        } => run_client(ClientOptions {
            address,
            key_file: resolve_key_path(key_file)?,
            display,
            view_only,
            grab_input,
            clipboard,
            escape_key,
        }),
    }
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
    let mut blocked = display.is_none() || session.eq_ignore_ascii_case("wayland");
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
