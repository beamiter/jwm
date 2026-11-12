//! TCP connection attempts share one elapsed-time budget after name resolution.
//! DNS resolution and the authenticated handshake have separate lifetimes.

use super::super::RemoteResult;
use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

pub(super) fn connect_with_timeout(address: &str, timeout: Duration) -> RemoteResult<TcpStream> {
    let resolved = address
        .to_socket_addrs()
        .map_err(|error| super::invalid_data(format!("cannot resolve {address}: {error}")))?
        .collect::<Vec<_>>();
    if resolved.is_empty() {
        return Err(super::invalid_data(format!("{address} resolved to no addresses")).into());
    }
    let started = Instant::now();
    connect_resolved(
        &resolved,
        timeout,
        || started.elapsed(),
        TcpStream::connect_timeout,
    )
    .map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot reach {address} within {timeout:?} after resolution: {error}"),
        )
        .into()
    })
}

/// Recompute a fair share from the remaining budget after each attempt, rather
/// than multiplying a per-address minimum by the number of DNS answers. Fast
/// failures leave their unused share for later addresses. Repeated answers do
/// not consume another attempt. The injected clock/connector keep deadline
/// regressions deterministic without relying on external black-holed routes.
fn connect_resolved<T>(
    resolved: &[SocketAddr],
    timeout: Duration,
    elapsed: impl Fn() -> Duration,
    mut connect: impl FnMut(&SocketAddr, Duration) -> io::Result<T>,
) -> io::Result<T> {
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zero TCP connect timeout",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let addresses: Vec<_> = resolved
        .iter()
        .filter(|address| seen.insert(**address))
        .collect();
    let mut last_error = io::Error::new(io::ErrorKind::InvalidInput, "no TCP addresses");
    for (index, address) in addresses.iter().enumerate() {
        let remaining = timeout.saturating_sub(elapsed());
        if remaining.is_zero() {
            return Err(deadline_expired());
        }
        let count = u32::try_from(addresses.len() - index).unwrap_or(u32::MAX);
        let share = remaining / count;
        if share.is_zero() {
            return Err(deadline_expired());
        }
        let result = connect(address, share);
        // A connector (or descheduled caller) may return after its allowance.
        // Do not start another attempt or accept a late socket after the budget.
        if elapsed() >= timeout {
            return Err(deadline_expired());
        }
        match result {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn deadline_expired() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "TCP connection budget exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn endpoints(count: u16) -> Vec<SocketAddr> {
        (1..=count)
            .map(|port| SocketAddr::from(([127, 0, 0, 1], port)))
            .collect()
    }

    #[test]
    fn many_slow_addresses_share_one_budget_without_a_one_second_floor() {
        let elapsed = Cell::new(Duration::ZERO);
        let mut attempts = 0;
        let error = connect_resolved::<()>(
            &endpoints(20),
            Duration::from_secs(10),
            || elapsed.get(),
            |_, allowance| {
                assert_eq!(allowance, Duration::from_millis(500));
                attempts += 1;
                elapsed.set(elapsed.get() + allowance);
                Err(io::Error::from(io::ErrorKind::TimedOut))
            },
        )
        .unwrap_err();
        assert_eq!(attempts, 20);
        assert_eq!(elapsed.get(), Duration::from_secs(10));
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn fast_failure_preserves_time_for_fallback() {
        let elapsed = Cell::new(Duration::ZERO);
        let mut allowances = Vec::new();
        let value = connect_resolved(
            &endpoints(2),
            Duration::from_secs(10),
            || elapsed.get(),
            |_, allowance| {
                allowances.push(allowance);
                if allowances.len() == 1 {
                    elapsed.set(Duration::from_millis(50));
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                } else {
                    Ok(7)
                }
            },
        )
        .unwrap();
        assert_eq!(value, 7);
        assert_eq!(
            allowances,
            [Duration::from_secs(5), Duration::from_millis(9950)]
        );
    }

    #[test]
    fn duplicate_addresses_are_attempted_once_in_resolver_order() {
        let addresses = endpoints(2);
        let mut attempted = Vec::new();
        let error = connect_resolved::<()>(
            &[addresses[0], addresses[0], addresses[1], addresses[0]],
            Duration::from_secs(10),
            || Duration::ZERO,
            |address, _| {
                attempted.push(*address);
                Err(io::Error::from(io::ErrorKind::ConnectionRefused))
            },
        )
        .unwrap_err();
        assert_eq!(attempted, addresses);
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn late_success_is_dropped_and_never_starts_a_second_attempt() {
        struct Socket<'a>(&'a Cell<bool>);
        impl Drop for Socket<'_> {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let elapsed = Cell::new(Duration::ZERO);
        let dropped = Cell::new(false);
        let mut attempts = 0;
        let result = connect_resolved(
            &endpoints(2),
            Duration::from_secs(1),
            || elapsed.get(),
            |_, _| {
                attempts += 1;
                elapsed.set(Duration::from_secs(2));
                Ok(Socket(&dropped))
            },
        );
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::TimedOut));
        assert_eq!(attempts, 1);
        assert!(dropped.get());
    }

    #[test]
    fn zero_or_exhausted_budget_never_calls_connector() {
        for (timeout, elapsed, expected) in [
            (Duration::ZERO, Duration::ZERO, io::ErrorKind::InvalidInput),
            (
                Duration::from_secs(1),
                Duration::from_secs(1),
                io::ErrorKind::TimedOut,
            ),
            (
                Duration::from_nanos(1),
                Duration::ZERO,
                io::ErrorKind::TimedOut,
            ),
        ] {
            let result = connect_resolved::<()>(
                &endpoints(2),
                timeout,
                || elapsed,
                |_, _| panic!("no connect allowance"),
            );
            assert_eq!(result.unwrap_err().kind(), expected);
        }
    }

    #[test]
    fn refused_attempt_falls_back_to_real_loopback_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // Inject the first refusal rather than racing reuse of an unbound port.
        let refused_address = SocketAddr::from(([127, 0, 0, 1], 0));
        let started = Instant::now();
        let mut stream = connect_resolved(
            &[refused_address, listener.local_addr().unwrap()],
            Duration::from_secs(2),
            || started.elapsed(),
            |address, timeout| {
                if *address == refused_address {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                } else {
                    TcpStream::connect_timeout(address, timeout)
                }
            },
        )
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), listener.local_addr().unwrap());
        stream.write_all(b"live").unwrap();
        let (mut accepted, _) = listener.accept().unwrap();
        accepted
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut bytes = [0; 4];
        accepted.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"live");
    }

    #[test]
    fn public_numeric_address_path_connects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = connect_with_timeout(
            &listener.local_addr().unwrap().to_string(),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), listener.local_addr().unwrap());
    }
}
