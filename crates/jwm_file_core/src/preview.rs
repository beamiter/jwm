use crate::{Cancellation, DirectoryHandle, FileIdentity};
use rustix::{
    fd::AsRawFd,
    fs::{self, FileType, Mode, OFlags},
    io::Errno,
};
use std::{
    ffi::OsStr,
    fmt,
    fs::File,
    io::{self, Read},
    path::Path,
};

pub const TEXT_PREVIEW_LIMIT: usize = 128 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub struct TextPreview {
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug)]
pub enum PreviewError {
    InvalidName,
    Changed,
    NotRegular,
    NotText,
    Cancelled,
    Io(io::Error),
}

impl fmt::Display for PreviewError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => out.write_str("expected one file name"),
            Self::Changed => out.write_str("preview source moved or changed"),
            Self::NotRegular => out.write_str("only regular files can be previewed"),
            Self::NotText => out.write_str("not a supported plain UTF-8 text file"),
            Self::Cancelled => out.write_str("preview cancelled"),
            Self::Io(err) => err.fmt(out),
        }
    }
}

impl std::error::Error for PreviewError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

fn ensure_active(cancel: &Cancellation) -> Result<(), PreviewError> {
    cancel.check().map_err(|_| PreviewError::Cancelled)
}

fn io_error(error: Errno) -> PreviewError {
    PreviewError::Io(error.into())
}

fn decode(mut bytes: Vec<u8>) -> Result<TextPreview, PreviewError> {
    let truncated = bytes.len() > TEXT_PREVIEW_LIMIT;
    bytes.truncate(TEXT_PREVIEW_LIMIT);
    if bytes
        .iter()
        .any(|byte| *byte < 0x20 && !matches!(byte, b'\n' | b'\r' | b'\t'))
        || bytes.contains(&0x7f)
    {
        return Err(PreviewError::NotText);
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if truncated && error.utf8_error().error_len().is_none() => {
            let edge = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(edge);
            String::from_utf8(bytes).map_err(|_| PreviewError::NotText)?
        }
        Err(_) => return Err(PreviewError::NotText),
    };
    Ok(TextPreview { text, truncated })
}

impl DirectoryHandle {
    /// Read a capped plain-text preview without following a child symlink.
    /// No content is interpreted as markup or loaded through an external API.
    pub fn read_text_preview(
        &self,
        displayed_parent: &Path,
        name: &OsStr,
        expected: FileIdentity,
        cancel: &Cancellation,
    ) -> Result<TextPreview, PreviewError> {
        ensure_active(cancel)?;
        if !crate::mkdir::single_name(name) {
            return Err(PreviewError::InvalidName);
        }
        let current = Self::open(displayed_parent).map_err(PreviewError::Io)?;
        if current.identity() != self.identity() {
            return Err(PreviewError::Changed);
        }
        let pin = fs::openat(
            &self.fd,
            name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io_error)?;
        let stat = fs::fstat(&pin).map_err(io_error)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(PreviewError::NotRegular);
        }
        if stat.st_ino != expected.inode || stat.st_dev != expected.device {
            return Err(PreviewError::Changed);
        }
        let reopened = fs::open(
            format!("/proc/self/fd/{}", pin.as_raw_fd()),
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io_error)?;
        let mut input = File::from(reopened);
        let mut content = Vec::new();
        let mut buffer = [0_u8; 8192];
        while content.len() <= TEXT_PREVIEW_LIMIT {
            ensure_active(cancel)?;
            let cap = (TEXT_PREVIEW_LIMIT + 1 - content.len()).min(buffer.len());
            let count = match input.read(&mut buffer[..cap]) {
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                result => result.map_err(PreviewError::Io)?,
            };
            if count == 0 {
                break;
            }
            content.extend_from_slice(&buffer[..count]);
        }
        ensure_active(cancel)?;
        let after = fs::fstat(&pin).map_err(io_error)?;
        if (
            stat.st_size,
            stat.st_mtime,
            stat.st_mtime_nsec,
            stat.st_ctime,
            stat.st_ctime_nsec,
        ) != (
            after.st_size,
            after.st_mtime,
            after.st_mtime_nsec,
            after.st_ctime,
            after.st_ctime_nsec,
        ) {
            return Err(PreviewError::Changed);
        }
        decode(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_preview_bounds_input_and_drops_only_truncated_utf8_tail() {
        let mut content = vec![b'a'; TEXT_PREVIEW_LIMIT - 1];
        content.extend("你好".as_bytes());
        let preview = decode(content).unwrap();
        assert!(preview.truncated);
        assert_eq!(preview.text.len(), TEXT_PREVIEW_LIMIT - 1);
        assert!(matches!(decode(vec![0, b'a']), Err(PreviewError::NotText)));
        assert!(matches!(decode(vec![0xff]), Err(PreviewError::NotText)));
        assert!(matches!(decode(vec![0xe4]), Err(PreviewError::NotText)));
        assert_eq!(
            decode("Markdown # title".into()).unwrap().text,
            "Markdown # title"
        );
    }
}
