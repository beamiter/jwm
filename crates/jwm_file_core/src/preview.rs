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
    io::{self, Cursor, Read},
    path::Path,
};

pub const TEXT_PREVIEW_LIMIT: usize = 128 * 1024;
pub const PNG_PREVIEW_BYTE_LIMIT: usize = 8 * 1024 * 1024;
const PNG_PREVIEW_PIXEL_LIMIT: u32 = 8_388_608;
const PNG_PREVIEW_AXIS_LIMIT: u32 = 8192;

#[derive(Debug, Eq, PartialEq)]
pub struct PngPreview {
    pub width: u32,
    pub height: u32,
    /// Packed straight-alpha RGBA, eight bits per component.
    pub rgba: Vec<u8>,
}

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
    NotStaticPng,
    ImageLimit,
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
            Self::NotStaticPng => out.write_str("not a supported static PNG"),
            Self::ImageLimit => out.write_str("image exceeds PNG preview size limits"),
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
        decode(self.read_preview_bytes(
            displayed_parent,
            name,
            expected,
            TEXT_PREVIEW_LIMIT,
            cancel,
        )?)
    }

    /// Decode a bounded static PNG without following a child symlink.
    pub fn read_png_preview(
        &self,
        displayed_parent: &Path,
        name: &OsStr,
        expected: FileIdentity,
        cancel: &Cancellation,
    ) -> Result<PngPreview, PreviewError> {
        let bytes = self.read_preview_bytes(
            displayed_parent,
            name,
            expected,
            PNG_PREVIEW_BYTE_LIMIT,
            cancel,
        )?;
        decode_png(&bytes, cancel)
    }

    fn read_preview_bytes(
        &self,
        displayed_parent: &Path,
        name: &OsStr,
        expected: FileIdentity,
        limit: usize,
        cancel: &Cancellation,
    ) -> Result<Vec<u8>, PreviewError> {
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
        while content.len() <= limit {
            ensure_active(cancel)?;
            let cap = (limit + 1 - content.len()).min(buffer.len());
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
        Ok(content)
    }
}

fn check_png_size(width: u32, height: u32) -> Result<usize, PreviewError> {
    match width.checked_mul(height) {
        Some(pixels)
            if pixels > 0
                && pixels <= PNG_PREVIEW_PIXEL_LIMIT
                && width <= PNG_PREVIEW_AXIS_LIMIT
                && height <= PNG_PREVIEW_AXIS_LIMIT =>
        {
            Ok(pixels as usize)
        }
        _ => Err(PreviewError::ImageLimit),
    }
}

fn png_error(error: png::DecodingError) -> PreviewError {
    if matches!(error, png::DecodingError::LimitsExceeded) {
        PreviewError::ImageLimit
    } else {
        PreviewError::NotStaticPng
    }
}

fn decode_png(bytes: &[u8], cancel: &Cancellation) -> Result<PngPreview, PreviewError> {
    ensure_active(cancel)?;
    if bytes.len() > PNG_PREVIEW_BYTE_LIMIT {
        return Err(PreviewError::ImageLimit);
    }
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: 16 * 1024 * 1024,
    });
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let header = decoder.read_header_info().map_err(png_error)?;
    let expected = (header.width, header.height);
    let pixels = check_png_size(expected.0, expected.1)?;
    let mut reader = decoder.read_info().map_err(png_error)?;
    if reader.info().is_animated() {
        return Err(PreviewError::NotStaticPng);
    }
    let size = reader
        .output_buffer_size()
        .filter(|size| *size <= pixels * 4)
        .ok_or(PreviewError::ImageLimit)?;
    let mut samples = vec![0; size];
    let frame = reader.next_frame(&mut samples).map_err(png_error)?;
    reader.finish().map_err(png_error)?;
    ensure_active(cancel)?;
    let (width, height) = (frame.width, frame.height);
    if (width, height) != expected || frame.bit_depth != png::BitDepth::Eight {
        return Err(PreviewError::NotStaticPng);
    }
    let channels = frame.color_type.samples();
    if frame.buffer_size() != pixels * channels || frame.buffer_size() > samples.len() {
        return Err(PreviewError::NotStaticPng);
    }
    samples.truncate(frame.buffer_size());
    let rgba = if frame.color_type == png::ColorType::Rgba {
        samples
    } else {
        let mut rgba = Vec::with_capacity(pixels * 4);
        for row in samples.chunks_exact(width as usize * channels) {
            ensure_active(cancel)?;
            for pixel in row.chunks_exact(channels) {
                let color = match *pixel {
                    [l] => [l, l, l, 255],
                    [l, a] => [l, l, l, a],
                    [r, g, b] => [r, g, b, 255],
                    _ => return Err(PreviewError::NotStaticPng),
                };
                rgba.extend_from_slice(&color);
            }
        }
        rgba
    };
    Ok(PngPreview {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_png(
        width: u32,
        height: u32,
        color: png::ColorType,
        samples: &[u8],
        animated: bool,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(color);
            if animated {
                encoder.set_animated(1, 0).unwrap();
            }
            let mut writer = encoder.write_header().unwrap();
            if !samples.is_empty() {
                writer.write_image_data(samples).unwrap();
            }
        }
        bytes
    }

    #[test]
    fn static_png_returns_exact_straight_alpha_pixels() {
        let cancel = Cancellation::default();
        let bytes = fixture_png(2, 1, png::ColorType::Rgb, &[255, 0, 0, 0, 0, 255], false);
        let image = decode_png(&bytes, &cancel).unwrap();
        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(image.rgba, [255, 0, 0, 255, 0, 0, 255, 255]);
        let gray = fixture_png(1, 1, png::ColorType::GrayscaleAlpha, &[20, 90], false);
        assert_eq!(decode_png(&gray, &cancel).unwrap().rgba, [20, 20, 20, 90]);
        let rgba = fixture_png(1, 1, png::ColorType::Rgba, &[2, 4, 6, 8], false);
        assert_eq!(decode_png(&rgba, &cancel).unwrap().rgba, [2, 4, 6, 8]);
    }

    #[test]
    fn png_preview_rejects_bad_animation_oversize_and_cancelled_input() {
        let cancel = Cancellation::default();
        let animated = fixture_png(1, 1, png::ColorType::Rgb, &[255, 0, 0], true);
        assert!(matches!(
            decode_png(&animated, &cancel),
            Err(PreviewError::NotStaticPng)
        ));
        let huge = fixture_png(3200, 3200, png::ColorType::Rgba, &[], false);
        assert!(matches!(
            decode_png(&huge, &cancel),
            Err(PreviewError::ImageLimit)
        ));
        assert!(matches!(
            decode_png(&vec![0; PNG_PREVIEW_BYTE_LIMIT + 1], &cancel),
            Err(PreviewError::ImageLimit)
        ));
        assert!(matches!(
            decode_png(b"not a PNG", &cancel),
            Err(PreviewError::NotStaticPng)
        ));
        let good = fixture_png(1, 1, png::ColorType::Rgb, &[255, 0, 0], false);
        assert!(matches!(
            decode_png(&good[..good.len() - 13], &cancel),
            Err(PreviewError::NotStaticPng)
        ));
        cancel.cancel();
        assert!(matches!(
            decode_png(&good, &cancel),
            Err(PreviewError::Cancelled)
        ));
    }

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
