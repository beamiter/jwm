//! JWM-to-JWM remote desktop helper for trusted X11 LANs.

use clap::{Parser, Subcommand};
use jwm::remote::client::{ClientOptions, run_client};
use jwm::remote::host::{HostOptions, run_host};
use jwm::remote::key::generate_key_file;
use jwm::remote::x11_capture::CaptureSource;
use std::path::PathBuf;

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

        /// Downscale wider frames; 0 keeps native width.
        #[arg(long, default_value = "1280", value_parser = parse_max_width)]
        max_width: u16,

        /// auto prefers JWM's Composite overlay and falls back to root.
        #[arg(long, default_value = "auto", value_parser = parse_capture_source)]
        capture_source: CaptureSource,

        /// Permit binding beyond loopback. Traffic is authenticated, not encrypted.
        #[arg(long)]
        allow_lan: bool,

        /// Permit the authenticated peer to inject keyboard and pointer input.
        #[arg(long)]
        allow_input: bool,

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

        /// Click the viewer to grab local input; F12 releases the grab.
        #[arg(long, conflicts_with = "view_only")]
        grab_input: bool,
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
            max_width,
            capture_source,
            allow_lan,
            allow_input,
            once,
        } => run_host(HostOptions {
            listen,
            key_file,
            display,
            fps,
            jpeg_quality,
            max_width,
            capture_source,
            allow_lan,
            allow_input,
            once,
        }),
        Command::Connect {
            address,
            key_file,
            display,
            view_only,
            grab_input,
        } => run_client(ClientOptions {
            address,
            key_file,
            display,
            view_only,
            grab_input,
        }),
    }
}

fn parse_fps(value: &str) -> Result<u16, String> {
    parse_range(value, 1, 60, "fps")
}

fn parse_quality(value: &str) -> Result<u8, String> {
    parse_range(value, 1, 100, "JPEG quality")
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
        assert_eq!(parse_max_width("0"), Ok(0));
        assert!(parse_max_width("100").is_err());
    }
}
