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
pub const IMAGE_PREVIEW_BYTE_LIMIT: usize = 8 * 1024 * 1024;
const IMAGE_PREVIEW_PIXEL_LIMIT: u32 = 8_388_608;
const IMAGE_PREVIEW_AXIS_LIMIT: u32 = 8192;

#[derive(Debug, Eq, PartialEq)]
pub struct ImagePreview {
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
    NotJpeg,
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
            Self::NotJpeg => out.write_str("not a supported JPEG"),
            Self::ImageLimit => out.write_str("image exceeds quick preview size limits"),
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
    ) -> Result<ImagePreview, PreviewError> {
        let bytes = self.read_preview_bytes(
            displayed_parent,
            name,
            expected,
            IMAGE_PREVIEW_BYTE_LIMIT,
            cancel,
        )?;
        decode_png(&bytes, cancel)
    }

    /// Decode a bounded JPEG without following a child symlink.
    pub fn read_jpeg_preview(
        &self,
        displayed_parent: &Path,
        name: &OsStr,
        expected: FileIdentity,
        cancel: &Cancellation,
    ) -> Result<ImagePreview, PreviewError> {
        let bytes = self.read_preview_bytes(
            displayed_parent,
            name,
            expected,
            IMAGE_PREVIEW_BYTE_LIMIT,
            cancel,
        )?;
        decode_jpeg(&bytes, cancel)
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

fn check_image_size(width: u32, height: u32) -> Result<usize, PreviewError> {
    match width.checked_mul(height) {
        Some(pixels)
            if pixels > 0
                && pixels <= IMAGE_PREVIEW_PIXEL_LIMIT
                && width <= IMAGE_PREVIEW_AXIS_LIMIT
                && height <= IMAGE_PREVIEW_AXIS_LIMIT =>
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

fn decode_png(bytes: &[u8], cancel: &Cancellation) -> Result<ImagePreview, PreviewError> {
    ensure_active(cancel)?;
    if bytes.len() > IMAGE_PREVIEW_BYTE_LIMIT {
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
    let pixels = check_image_size(expected.0, expected.1)?;
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
    Ok(ImagePreview {
        width,
        height,
        rgba,
    })
}

fn decode_jpeg(bytes: &[u8], cancel: &Cancellation) -> Result<ImagePreview, PreviewError> {
    use zune_jpeg::{
        JpegDecoder,
        errors::DecodeErrors,
        zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions},
    };
    ensure_active(cancel)?;
    if bytes.len() > IMAGE_PREVIEW_BYTE_LIMIT {
        return Err(PreviewError::ImageLimit);
    }
    if !bytes.ends_with(&[0xff, 0xd9]) {
        return Err(PreviewError::NotJpeg);
    }
    let options = DecoderOptions::default()
        .set_strict_mode(true)
        .set_max_width(IMAGE_PREVIEW_AXIS_LIMIT as usize)
        .set_max_height(IMAGE_PREVIEW_AXIS_LIMIT as usize)
        .jpeg_set_max_scans(36)
        .jpeg_set_out_colorspace(ColorSpace::RGBA);
    let error = |error| match error {
        DecodeErrors::LargeDimensions(_) => PreviewError::ImageLimit,
        _ => PreviewError::NotJpeg,
    };
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().map_err(error)?;
    let header = decoder.info().ok_or(PreviewError::NotJpeg)?;
    let (width, height) = (u32::from(header.width), u32::from(header.height));
    let length = check_image_size(width, height)? * 4;
    if decoder.output_buffer_size() != Some(length) {
        return Err(PreviewError::NotJpeg);
    }
    ensure_active(cancel)?;
    let mut rgba = vec![0; length];
    decoder.decode_into(&mut rgba).map_err(error)?;
    ensure_active(cancel)?;
    let orientation = header
        .exif_data
        .as_deref()
        .and_then(exif_orientation)
        .unwrap_or(1);
    orient_image(
        ImagePreview {
            width,
            height,
            rgba,
        },
        orientation,
        cancel,
    )
}

/// Read only the inline Orientation SHORT in the primary TIFF directory.
/// Do not follow offsets to thumbnails, nested directories or other tags.
fn exif_orientation(tiff: &[u8]) -> Option<u16> {
    let little = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let short = |bytes: &[u8]| {
        let word: [u8; 2] = bytes.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(word)
        } else {
            u16::from_be_bytes(word)
        })
    };
    if short(tiff.get(2..4)?)? != 42 {
        return None;
    }
    let offset: [u8; 4] = tiff.get(4..8)?.try_into().ok()?;
    let offset = if little {
        u32::from_le_bytes(offset)
    } else {
        u32::from_be_bytes(offset)
    } as usize;
    if offset < 8 {
        return None;
    }
    let directory = tiff.get(offset..)?;
    let count = usize::from(short(directory.get(..2)?)?);
    for field in directory.get(2..)?.chunks_exact(12).take(count) {
        if short(&field[..2])? != 0x0112 {
            continue;
        }
        let one = if little { [1, 0, 0, 0] } else { [0, 0, 0, 1] };
        if short(&field[2..4])? != 3 || field[4..8] != one {
            return None;
        }
        return short(&field[8..10]).filter(|value| (1..=8).contains(value));
    }
    None
}

fn orient_image(
    image: ImagePreview,
    orientation: u16,
    cancel: &Cancellation,
) -> Result<ImagePreview, PreviewError> {
    if orientation == 1 || !(2..=8).contains(&orientation) {
        return Ok(image);
    }
    let (source_w, source_h) = (image.width, image.height);
    let (width, height) = if orientation >= 5 {
        (source_h, source_w)
    } else {
        (source_w, source_h)
    };
    let mut rgba = Vec::with_capacity(image.rgba.len());
    for y in 0..height {
        ensure_active(cancel)?;
        for x in 0..width {
            let (sx, sy) = match orientation {
                2 => (source_w - 1 - x, y),
                3 => (source_w - 1 - x, source_h - 1 - y),
                4 => (x, source_h - 1 - y),
                5 => (y, x),
                6 => (y, source_h - 1 - x),
                7 => (source_w - 1 - y, source_h - 1 - x),
                8 => (source_w - 1 - y, x),
                _ => unreachable!(),
            };
            let offset = ((sy * source_w + sx) * 4) as usize;
            rgba.extend_from_slice(&image.rgba[offset..offset + 4]);
        }
    }
    Ok(ImagePreview {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_quick_preview_decodes_progressive_gray_and_honors_orientation() {
        let source = b"\xFF\xD8\xFF\xE0\x00\x10\x4A\x46\x49\x46\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xFF\xE1\x00\x22\x45\x78\x69\x66\x00\x00\x4D\x4D\x00\x2A\x00\x00\x00\x08\x00\x01\x01\x12\x00\x03\x00\x00\x00\x01\x00\x06\x00\x00\x00\x00\x00\x00\xFF\xDB\x00\x43\x00\x03\x02\x02\x03\x02\x02\x03\x03\x02\x03\x03\x03\x03\x03\x04\x07\x05\x04\x04\x04\x04\x09\x06\x07\x05\x07\x0A\x09\x0B\x0B\x0A\x09\x0A\x0A\x0C\x0D\x11\x0E\x0C\x0C\x10\x0C\x0A\x0A\x0E\x14\x0F\x10\x11\x12\x13\x13\x13\x0B\x0E\x14\x16\x14\x12\x16\x11\x12\x13\x12\xFF\xC2\x00\x0B\x08\x00\x02\x00\x03\x01\x01\x11\x00\xFF\xC4\x00\x14\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x07\xFF\xDA\x00\x08\x01\x01\x00\x00\x00\x01\x14\xFF\xC4\x00\x14\x10\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xFF\xDA\x00\x08\x01\x01\x00\x01\x05\x02\x7F\xFF\xC4\x00\x14\x10\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xFF\xDA\x00\x08\x01\x01\x00\x06\x3F\x02\x7F\xFF\xC4\x00\x14\x10\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xFF\xDA\x00\x08\x01\x01\x00\x01\x3F\x21\x7F\xFF\xDA\x00\x08\x01\x01\x00\x00\x00\x10\xFF\x00\xFF\xC4\x00\x14\x10\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xFF\xDA\x00\x08\x01\x01\x00\x01\x3F\x10\x7F\xFF\xD9";
        let cancel = Cancellation::default();
        let image = decode_jpeg(source, &cancel).unwrap();
        assert_eq!((image.width, image.height), (2, 3));
        assert_eq!(image.rgba, [48, 48, 48, 255].repeat(6));
        assert!(matches!(
            decode_jpeg(&source[..source.len() - 2], &cancel),
            Err(PreviewError::NotJpeg)
        ));
        assert!(matches!(
            decode_jpeg(&vec![0; IMAGE_PREVIEW_BYTE_LIMIT + 1], &cancel),
            Err(PreviewError::ImageLimit)
        ));
        cancel.cancel();
        assert!(matches!(
            decode_jpeg(source, &cancel),
            Err(PreviewError::Cancelled)
        ));
    }

    #[test]
    fn jpeg_exif_orientation_transforms_each_non_square_axis_and_mirror() {
        let cancel = Cancellation::default();
        for (orientation, order) in [
            (1, [1, 2, 3, 4, 5, 6]),
            (2, [3, 2, 1, 6, 5, 4]),
            (3, [6, 5, 4, 3, 2, 1]),
            (4, [4, 5, 6, 1, 2, 3]),
            (5, [1, 4, 2, 5, 3, 6]),
            (6, [4, 1, 5, 2, 6, 3]),
            (7, [6, 3, 5, 2, 4, 1]),
            (8, [3, 6, 2, 5, 1, 4]),
        ] {
            let image = ImagePreview {
                width: 3,
                height: 2,
                rgba: (1..=6).flat_map(|i| [i, i, i, 255]).collect(),
            };
            let result = orient_image(image, orientation, &cancel).unwrap();
            assert_eq!(
                (result.width, result.height),
                if orientation >= 5 { (2, 3) } else { (3, 2) }
            );
            assert_eq!(
                result
                    .rgba
                    .chunks_exact(4)
                    .map(|rgba| rgba[0])
                    .collect::<Vec<_>>(),
                order
            );
        }
        assert_eq!(exif_orientation(b"II\x2A\x00\xFF\xFF\xFF\xFF"), None);
        assert_eq!(exif_orientation(b"MM\x00\x2A\x00\x00\x00\x08\x00\x01\x01\x12\x00\x03\x00\x00\x00\x01\x00\x08\x00\x00"), Some(8));
    }

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
            decode_png(&vec![0; IMAGE_PREVIEW_BYTE_LIMIT + 1], &cancel),
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
