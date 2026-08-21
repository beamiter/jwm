//! JWM-to-JWM remote desktop helper for trusted X11 LANs.

use clap::{Parser, Subcommand};
use jwm::remote::client::{ClientOptions, run_client};
use jwm::remote::host::{HostOptions, run_host};
use jwm::remote::key::generate_key_file;
use jwm::remote::x11_capture::{CaptureArea, CaptureRegion, CaptureSource};
use jwm::remote::x11_viewer::DEFAULT_ESCAPE_KEY;
use std::path::PathBuf;

const DEFAULT_JPEG_QUALITY_FLOOR: u8 = 40;

#[derive(Debug, Parser)]
#[command(
    name = "jwm-remote",
    version,
    about = "JWM-to-JWM remote viewing and control for trusted X11 LANs"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a new private pre-shared key file (never overwrites).
    Keygen {
        #[arg(long, value_name = "PATH")]
        output: PathBuf,
    },
    /// Share the current JWM X11 desktop.
    Host {
        /// TCP listener. Non-loopback addresses also require --allow-lan.
        #[arg(long, default_value = "127.0.0.1:48221", value_name = "ADDRESS:PORT")]
        listen: String,

        #[arg(long, value_name = "PATH")]
        key_file: PathBuf,

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
        #[arg(value_name = "HOST:PORT")]
        address: String,

        #[arg(long, value_name = "PATH")]
        key_file: PathBuf,

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

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match Cli::parse().command {
        Command::Keygen { output } => {
            generate_key_file(&output)?;
            println!("Generated private remote key: {}", output.display());
            Ok(())
        }
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
            key_file,
            display,
            view_only,
            grab_input,
            clipboard,
            escape_key,
        }),
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
