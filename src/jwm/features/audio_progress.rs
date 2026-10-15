//! Bounded parser for the dedicated ffmpeg `-progress pipe:1` stream.
//! Readiness requires a complete nonterminal report with positive encoded time.

use std::io::{self, Read};

const MAX_LINE: usize = 1024;
const READS_PER_POLL: usize = 4;

#[derive(Default)]
pub(super) struct AudioProgress {
    line: Vec<u8>,
    dropping_line: bool,
    invalid_report: bool,
    time_us: Option<i64>,
}

impl AudioProgress {
    fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut ready = false;
        for &byte in bytes {
            if byte == b'\n' {
                if !self.dropping_line {
                    ready |= self.finish_line();
                }
                self.line.clear();
                self.dropping_line = false;
            } else if !self.dropping_line {
                if self.line.len() == MAX_LINE {
                    self.line.clear();
                    self.dropping_line = true;
                    self.invalid_report = true;
                } else {
                    self.line.push(byte);
                }
            }
        }
        ready
    }

    fn finish_line(&mut self) -> bool {
        let Ok(line) = std::str::from_utf8(&self.line) else {
            self.invalid_report = true;
            return false;
        };
        let Some((key, value)) = line.trim_end_matches('\r').split_once('=') else {
            self.invalid_report = true;
            return false;
        };
        match key {
            "out_time_us" => {
                if self.time_us.is_some() {
                    self.invalid_report = true;
                }
                match value.parse::<i64>() {
                    Ok(time) => self.time_us = Some(time),
                    Err(_) => self.invalid_report = true,
                }
                false
            }
            "progress" => {
                let ready = value == "continue"
                    && !self.invalid_report
                    && self.time_us.is_some_and(|time| time > 0);
                self.time_us = None;
                self.invalid_report = false;
                ready
            }
            _ => false,
        }
    }

    /// The reader MUST be nonblocking. Bound each poll's work as well as line
    /// storage, so a noisy child cannot starve timeout/stop/exit checks.
    /// Returns (observed positive complete report, EOF).
    pub(super) fn poll(&mut self, reader: &mut impl Read) -> io::Result<(bool, bool)> {
        let mut ready = false;
        let mut bytes = [0; 4096];
        for _ in 0..READS_PER_POLL {
            match reader.read(&mut bytes) {
                Ok(0) => return Ok((ready, true)),
                Ok(count) => ready |= self.feed(&bytes[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok((ready, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_positive_time_not_amplitude_proves_readiness() {
        // Silent audio still has encoded duration; no waveform/size threshold.
        let mut parser = AudioProgress::default();
        assert!(!parser.feed(b"out_time_us=500000\n"));
        assert!(!parser.feed(b"progress=cont"));
        assert!(parser.feed(b"inue\n"));
    }

    #[test]
    fn empty_zero_negative_invalid_and_terminal_reports_are_not_ready() {
        for report in [
            "progress=continue\n",
            "out_time_us=0\nprogress=continue\n",
            "out_time_us=-1\nprogress=continue\n",
            "out_time_us=N/A\nprogress=continue\n",
            "out_time_us=999999999999999999999999\nprogress=continue\n",
            "out_time_us=1\nprogress=end\n",
            "out_time_us=1\nmalformed\nprogress=continue\n",
            "out_time_us=1\nout_time_us=2\nprogress=continue\n",
        ] {
            let mut parser = AudioProgress::default();
            assert!(!parser.feed(report.as_bytes()), "{report}");
            assert!(parser.feed(b"out_time_us=20000\nprogress=continue\n"));
        }
    }

    #[test]
    fn oversized_lines_are_bounded_and_poison_only_their_report() {
        let mut parser = AudioProgress::default();
        assert!(!parser.feed(&vec![b'x'; MAX_LINE * 10]));
        assert!(parser.line.len() <= MAX_LINE);
        assert!(!parser.feed(b"\nout_time_us=1\nprogress=continue\n"));
        assert!(parser.feed(b"out_time_us=2\nprogress=continue\n"));
    }

    #[test]
    fn fields_do_not_leak_across_reports_and_crlf_is_supported() {
        let mut parser = AudioProgress::default();
        assert!(parser.feed(b"out_time_us=100\r\nprogress=continue\r\n"));
        assert!(!parser.feed(b"progress=continue\n"));
        assert!(!parser.feed(b"out_time_us=100\nprogress=garbage\n"));
        assert!(!parser.feed(b"progress=continue\n"));
    }

    #[test]
    fn nonblocking_poll_bounds_noise_and_interrupted_reads() {
        struct Noisy {
            calls: usize,
            interrupted: bool,
        }
        impl Read for Noisy {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.interrupted {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                buf.fill(b'x');
                Ok(buf.len())
            }
        }
        for interrupted in [false, true] {
            let mut reader = Noisy {
                calls: 0,
                interrupted,
            };
            assert_eq!(
                AudioProgress::default().poll(&mut reader).unwrap(),
                (false, false)
            );
            assert_eq!(reader.calls, READS_PER_POLL);
        }
    }

    #[test]
    fn eof_does_not_complete_partial_reports() {
        let mut bytes = &b"out_time_us=100\nprogress=continue"[..];
        assert_eq!(
            AudioProgress::default().poll(&mut bytes).unwrap(),
            (false, true)
        );
    }
}
