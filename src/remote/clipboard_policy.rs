//! Explicitly consented clipboard policy with monotonic runtime narrowing. Directions are always
//! named relative to the host, including when evaluated on the host itself.
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClipboardMode {
    Text,
    TextImages,
    #[default]
    All,
}
impl ClipboardMode {
    pub const fn mask(self) -> u8 {
        match self {
            Self::Text => 1,
            Self::TextImages => 3,
            Self::All => 7,
        }
    }
}
impl fmt::Display for ClipboardMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text => "text",
            Self::TextImages => "text-images",
            Self::All => "all",
        })
    }
}
impl FromStr for ClipboardMode {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "text" => Ok(Self::Text),
            "text-images" => Ok(Self::TextImages),
            "all" => Ok(Self::All),
            _ => Err("clipboard mode must be text, text-images or all".into()),
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClipboardDirection {
    #[default]
    Both,
    ToHost,
    FromHost,
}
impl ClipboardDirection {
    pub const fn mask(self) -> u8 {
        match self {
            Self::Both => 3,
            Self::ToHost => 1,
            Self::FromHost => 2,
        }
    }
}
impl fmt::Display for ClipboardDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Both => "both",
            Self::ToHost => "to-host",
            Self::FromHost => "from-host",
        })
    }
}
impl FromStr for ClipboardDirection {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "both" => Ok(Self::Both),
            "to-host" => Ok(Self::ToHost),
            "from-host" => Ok(Self::FromHost),
            _ => Err("clipboard direction must be both, to-host or from-host".into()),
        }
    }
}

/// Construct a valid request without allowing a mode to imply consent.
pub fn requested(
    enabled: bool,
    mode: ClipboardMode,
    direction: ClipboardDirection,
    file_limit: u64,
) -> (u8, u8) {
    if !enabled {
        return (0, 0);
    }
    (
        mode.mask() & if file_limit == 0 { 3 } else { 7 },
        direction.mask(),
    )
}

pub fn intersect(request: (u8, u8), policy: (u8, u8)) -> (u8, u8) {
    let content = request.0 & policy.0;
    let directions = request.1 & policy.1;
    if content == 0 || directions == 0 {
        (0, 0)
    } else {
        (content, directions)
    }
}

pub fn label(content: u8, directions: u8) -> String {
    let content = match content {
        1 => "text",
        3 => "text + images",
        7 => "text + images + files",
        _ => return "off".into(),
    };
    let direction = match directions {
        1 => "to host",
        2 => "from host",
        3 => "both ways",
        _ => return "off".into(),
    };
    format!("{content}, {direction}")
}

/// Runtime controls can remove content, never add it or change direction.
pub const fn valid_content(content: u8) -> bool {
    matches!(content, 0 | 1 | 3 | 7)
}

pub const fn next_content(content: u8) -> Option<u8> {
    match content {
        7 => Some(3),
        3 => Some(1),
        1 => Some(0),
        _ => None,
    }
}

pub const fn strictly_narrows(current: u8, requested: u8) -> bool {
    valid_content(current)
        && valid_content(requested)
        && current != requested
        && requested & !current == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matrix_never_enlarges_either_permission() {
        for mode in [
            ClipboardMode::Text,
            ClipboardMode::TextImages,
            ClipboardMode::All,
        ] {
            for direction in [
                ClipboardDirection::Both,
                ClipboardDirection::ToHost,
                ClipboardDirection::FromHost,
            ] {
                for enabled in [false, true] {
                    for limit in [0, 42] {
                        let request = requested(enabled, mode, direction, limit);
                        if !enabled {
                            assert_eq!(request, (0, 0));
                        }
                        if limit == 0 {
                            assert_eq!(request.0 & 4, 0);
                        }
                        for content in [0, 1, 3, 7] {
                            for directions in 0..=3 {
                                let result = intersect(request, (content, directions));
                                assert_eq!(result.0 & !request.0, 0);
                                assert_eq!(result.0 & !content, 0);
                                assert_eq!(result.1 & !request.1, 0);
                                assert_eq!(result.1 & !directions, 0);
                                assert_eq!(result.0 == 0, result.1 == 0);
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(intersect((7, 1), (7, 2)), (0, 0));
    }
}
