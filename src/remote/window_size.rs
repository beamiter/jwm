//! Explicit local viewer startup size, independent of decoded-frame budgets.
use serde::{Deserialize, Serialize};
use std::{fmt, io, str::FromStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowSize {
    pub width: u16,
    pub height: u16,
}

impl WindowSize {
    pub fn validate(self) -> io::Result<()> {
        if self.width == 0
            || self.height == 0
            || self.width > 8192
            || self.height > 8192
            || u64::from(self.width) * u64::from(self.height) > 16_777_216
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "window size must have dimensions 1..=8192 and at most 16777216 pixels",
            ));
        }
        Ok(())
    }

    pub fn dimensions(self) -> (u16, u16) {
        (self.width, self.height)
    }
}

impl fmt::Display for WindowSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

impl FromStr for WindowSize {
    type Err = io::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "window size must be WIDTHxHEIGHT using ASCII digits",
            )
        };
        let (width, height) = value.split_once('x').ok_or_else(invalid)?;
        if [width, height]
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(invalid());
        }
        let size = Self {
            width: width.parse().map_err(|_| invalid())?,
            height: height.parse().map_err(|_| invalid())?,
        };
        size.validate()?;
        Ok(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_size_parser_and_bounds() {
        for valid in ["1x1", "8192x2048", "2048x8192", "4096x4096", "1280x720"] {
            let size: WindowSize = valid.parse().unwrap();
            assert_eq!(size.to_string(), valid);
        }
        for invalid in [
            "",
            "1",
            "1x",
            "x1",
            "1X1",
            "+1x1",
            "-1x1",
            " 1x1",
            "1x1 ",
            "١x1",
            "1x1x1",
            "0x1",
            "1x0",
            "8193x1",
            "1x8193",
            "8192x2049",
            "4097x4096",
            "65536x1",
            "999999999999999x1",
        ] {
            assert!(invalid.parse::<WindowSize>().is_err(), "{invalid}");
        }
        assert!(
            WindowSize {
                width: 0,
                height: 1
            }
            .validate()
            .is_err()
        );
    }
}
