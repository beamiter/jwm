//! jwm-bridge — freedesktop D-Bus services backed by jwm's built-in shell.
//!
//! jwm draws notifications itself and keeps their history, but applications
//! speak `org.freedesktop.Notifications` on the session bus. This process is
//! the translation layer, deliberately outside the compositor: the window
//! manager's event loop stays synchronous and D-Bus-free, and a wedged bus
//! cannot stall a frame.
//!
//! Process model:
//!   tokio runtime hosts the zbus service and the signal pump.
//!   One OS thread owns the blocking jwm event subscription.
//!   Commands run on `spawn_blocking` with bounded socket timeouts.

mod bluez;
mod jwm_ipc;
mod mpris;
mod notifications;

use jwm_ipc::JwmIpc;

/// Each subscription has its own bounded queue. Once full, the blocking IPC
/// worker stops reading its Unix socket until the async consumer catches up.
const EVENT_QUEUE_CAPACITY: usize = 256;

/// What `jwm-bridge` was asked to do.
#[derive(Debug, PartialEq, Eq)]
enum Verb {
    /// No subcommand: run the long-lived notification/MPRIS daemon.
    Serve,
    Pair,
    Accept,
    Discover,
    /// A subcommand this build does not recognize.
    Unknown,
}

/// Classify the first argument. An unrecognized verb is `Unknown`, never
/// `Serve`: a stale `jwm-bridge` in `PATH` whose verb jwm no longer speaks —
/// exactly the drift the user's own notes record between jwm and its side
/// binaries — must fail fast, not silently start a second notification daemon
/// that turns every `discover`/`pair`/`accept` into a 25s SIGKILLed no-op.
fn classify_verb(arg: Option<&str>) -> Verb {
    match arg {
        None => Verb::Serve,
        Some("pair") => Verb::Pair,
        Some("accept") => Verb::Accept,
        Some("discover") => Verb::Discover,
        Some(_) => Verb::Unknown,
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();

    // One-shot subcommands: drive a single Bluetooth verb against the system
    // bus and exit. Neither touches the session bus or the
    // org.freedesktop.Notifications name, so neither can disturb a
    // long-lived bridge running in the same session.
    let mut argv = std::env::args().skip(1);
    let verb = argv.next();
    match classify_verb(verb.as_deref()) {
        Verb::Pair => {
            let Some(address) = argv.next() else {
                eprintln!("usage: jwm-bridge pair <AA:BB:CC:DD:EE:FF>");
                std::process::exit(bluez::EXIT_USAGE);
            };
            // The session cookie arrives over the environment, not argv:
            // `ps` output is world-readable on machines without hidepid, and
            // the cookie is what authorizes answers to the pairing prompt.
            let cookie = std::env::var("JWM_PAIRING_COOKIE").unwrap_or_default();
            if cookie.is_empty() {
                eprintln!("jwm-bridge pair: JWM_PAIRING_COOKIE is not set");
                std::process::exit(bluez::EXIT_USAGE);
            }
            std::process::exit(bluez::run(&address, &cookie).await);
        }
        // `accept` holds one armed inbound window open: register an agent,
        // make the controller reachable, relay whatever rings to jwm's
        // picker, and put both back on the way out. Same cookie discipline
        // as `pair` — without a cookie jwm minted there is no window to
        // serve, so there is nothing this can be talked into.
        Verb::Accept => {
            let cookie = std::env::var("JWM_PAIRING_COOKIE").unwrap_or_default();
            if cookie.is_empty() {
                eprintln!("jwm-bridge accept: JWM_PAIRING_COOKIE is not set");
                std::process::exit(bluez::EXIT_USAGE);
            }
            std::process::exit(bluez::run_accept(&cookie).await);
        }
        // `discover [seconds]` prints a JSON array of what bluez knows;
        // `discover 0` lists without touching the radio. No cookie: it
        // reads nothing secret, answers on stdout rather than over the IPC
        // socket, and can be run by hand to see what the picker sees.
        Verb::Discover => {
            let seconds = match argv.next() {
                None => bluez::DISCOVERY_DEFAULT_SECONDS,
                Some(argument) => match argument.parse::<u64>() {
                    Ok(seconds) => seconds,
                    Err(_) => {
                        eprintln!("usage: jwm-bridge discover [seconds]");
                        std::process::exit(bluez::EXIT_USAGE);
                    }
                },
            };
            std::process::exit(bluez::discover(seconds).await);
        }
        Verb::Unknown => {
            eprintln!(
                "jwm-bridge: unknown subcommand {:?}",
                verb.as_deref().unwrap_or_default()
            );
            eprintln!("usage: jwm-bridge [pair <AA:BB:CC:DD:EE:FF> | accept | discover [seconds]]");
            std::process::exit(bluez::EXIT_USAGE);
        }
        // No subcommand: fall through to the long-lived notification daemon.
        Verb::Serve => {}
    }

    log::info!("jwm-bridge {} starting", env!("CARGO_PKG_VERSION"));

    let ipc = JwmIpc::new();
    // Fail loudly at startup rather than silently swallowing every
    // notification: without the compositor there is nothing to bridge to.
    if let Err(error) = ipc.query("get_version") {
        log::warn!(
            "jwm is not answering on {} yet ({error}); serving anyway and retrying per request",
            ipc.socket().display()
        );
    }

    // One subscription per consumer keeps the two features independent: a
    // stalled MPRIS call cannot delay a notification signal.
    let (notify_tx, notify_rx) = tokio::sync::mpsc::channel(EVENT_QUEUE_CAPACITY);
    jwm_ipc::subscribe(ipc.clone(), &["notification"], notify_tx);
    let (media_tx, media_rx) = tokio::sync::mpsc::channel(EVENT_QUEUE_CAPACITY);
    jwm_ipc::subscribe(ipc.clone(), &["media"], media_tx);

    let connection = zbus::connection::Builder::session()?
        .name(notifications::NAME)?
        .serve_at(
            notifications::PATH,
            notifications::Notifications::new(ipc.clone()),
        )?
        .build()
        .await?;
    log::info!("D-Bus service registered as {}", notifications::NAME);

    tokio::spawn(mpris::run(connection.clone(), ipc, media_rx));

    notifications::pump_signals(connection, notify_rx).await;
    log::warn!("jwm-bridge event pump ended");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Verb, classify_verb};

    #[test]
    fn only_the_known_verbs_are_recognized() {
        // No argument runs the daemon; each real verb maps to itself.
        assert_eq!(classify_verb(None), Verb::Serve);
        assert_eq!(classify_verb(Some("pair")), Verb::Pair);
        assert_eq!(classify_verb(Some("accept")), Verb::Accept);
        assert_eq!(classify_verb(Some("discover")), Verb::Discover);

        // Anything else is a usage error, not the daemon: a near-miss, an
        // empty string, and a plausible-but-unimplemented verb all reject so
        // a stale binary fails fast instead of starting a silent second
        // notification daemon.
        assert_eq!(classify_verb(Some("paired")), Verb::Unknown);
        assert_eq!(classify_verb(Some("")), Verb::Unknown);
        assert_eq!(classify_verb(Some("serve")), Verb::Unknown);
        assert_eq!(classify_verb(Some("--version")), Verb::Unknown);
    }
}
