//! Per-session low-delay hardware video using FFmpeg's packet and frame APIs.

use std::ffi::{CStr, c_char, c_void};
use std::io;
use std::ptr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ffmpeg::{codec, color, ffi, format::Pixel, frame::Video, software::scaling};
use ffmpeg_next as ffmpeg;
use image::RgbImage;
use rayon::prelude::*;

use super::{frame::DecodeBufferCheckout, messages::FrameCodec};

const MAX_PACKET_BYTES: usize = 1024 * 1024;
const MAX_PIXELS: u32 = 4096 * 2160;
const MAX_DIMENSION: u32 = 4096;

struct Format {
    name: &'static str,
    encoder: &'static str,
    decoder: codec::Id,
    pixels: Pixel,
    bitrate: usize,
    rate_control_buffer_bits: i32,
    preset: &'static str,
    devices: [Hardware; 2],
    probe: &'static [u8],
    device: OnceLock<Hardware>,
}

static H264: Format = Format {
    name: "H.264",
    encoder: "h264_nvenc",
    decoder: codec::Id::H264,
    pixels: Pixel::YUV420P,
    bitrate: 6_000_000,
    rate_control_buffer_bits: 400_000,
    preset: "p1",
    devices: [Hardware::Vaapi, Hardware::Cuda],
    probe: H264_PROBE,
    device: OnceLock::new(),
};

static HEVC444: Format = Format {
    name: "HEVC 4:4:4",
    encoder: "hevc_nvenc",
    decoder: codec::Id::HEVC,
    pixels: Pixel::YUV444P,
    bitrate: 10_000_000,
    // Allow more bits for desktop keyframes, which can stay on screen until
    // the next damage event. The target and maximum rates still match.
    rate_control_buffer_bits: 1_333_333,
    preset: "p3",
    devices: [Hardware::Cuda, Hardware::Vaapi],
    probe: HEVC444_PROBE,
    device: OnceLock::new(),
};

impl Format {
    fn from_codec(codec: FrameCodec) -> io::Result<&'static Self> {
        match codec {
            FrameCodec::H264 => Ok(&H264),
            FrameCodec::Hevc444 => Ok(&HEVC444),
            FrameCodec::JpegTiles => Err(invalid("unsupported hardware video codec")),
        }
    }

    fn accepts_coded_axis(&self, visible: i32, coded: i32) -> bool {
        if self.pixels == Pixel::YUV444P {
            // NVENC rounds HEVC to a coding block, which varies by GPU and
            // encoder settings. Limit it to at most one 64-pixel block.
            (visible..=((visible + 63) & !63)).contains(&coded)
        } else {
            coded == (visible + 15) & !15
        }
    }

    fn accepts_transfer(&self, transfer: Pixel) -> bool {
        transfer == self.pixels || (self.pixels == Pixel::YUV420P && transfer == Pixel::NV12)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn codec_error(operation: &str, error: ffmpeg::Error) -> io::Error {
    let kind = match error {
        ffmpeg::Error::InvalidData => io::ErrorKind::InvalidData,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("{operation}: {error}"))
}

fn checked_av(operation: &str, result: i32) -> io::Result<()> {
    if result < 0 {
        return Err(codec_error(operation, ffmpeg::Error::from(result)));
    }

    Ok(())
}

fn validate_dimensions(width: u32, height: u32) -> io::Result<()> {
    if !(2..=MAX_DIMENSION).contains(&width)
        || !(2..=MAX_DIMENSION).contains(&height)
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || width * height > MAX_PIXELS
    {
        return Err(invalid("unsupported hardware video dimensions"));
    }

    Ok(())
}

fn validate_packet(bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_PACKET_BYTES {
        return Err(invalid("video access unit exceeds 1 MiB"));
    }
    let start = bytes
        .strip_prefix(&[0, 0, 1])
        .or_else(|| bytes.strip_prefix(&[0, 0, 0, 1]));
    if !start.is_some_and(|nal| !nal.is_empty()) {
        return Err(invalid("invalid video Annex B packet"));
    }

    Ok(())
}

fn video_buffer(format: Pixel, width: u32, height: u32) -> io::Result<Video> {
    let mut frame = Video::empty();
    frame.set_format(format);
    frame.set_width(width);
    frame.set_height(height);

    // FFmpeg owns the allocation, including padding for vectorized reads.
    checked_av("allocating video frame buffer", unsafe {
        ffi::av_frame_get_buffer(frame.as_mut_ptr(), 32)
    })?;

    Ok(frame)
}

fn scaler(input: Pixel, output: Pixel, width: u32, height: u32) -> io::Result<scaling::Context> {
    let flags = if input == Pixel::YUV444P || output == Pixel::YUV444P {
        // The FAST path adjusts the horizontal sample step even at 1:1.
        // Preserve exact sample positions and keep full chroma in each direction.
        scaling::Flags::BILINEAR | scaling::Flags::FULL_CHR_H_INP | scaling::Flags::FULL_CHR_H_INT
    } else {
        scaling::Flags::FAST_BILINEAR
    };
    scaling::Context::get(input, width, height, output, width, height, flags)
        .map_err(|error| codec_error("opening video pixel converter", error))
}

struct ColorBand {
    first_row: u32,
    source_first_row: u32,
    rows: u32,
    scaler: scaling::Context,
    scratch: Option<Video>,
}

// Each band owns its SwsContext and scratch frame exclusively. A context can
// move between threads but a frame must never be submitted to it concurrently.
unsafe impl Send for ColorBand {}

impl ColorBand {
    /// The caller owns this band's destination rows for the duration of the call.
    unsafe fn convert(&mut self, input: &Video, output: &Video) -> io::Result<()> {
        if let Some(scratch) = &self.scratch {
            unsafe {
                scale_rows(&mut self.scaler, input, scratch, self.source_first_row, 0)?;
                copy_band_rows(
                    scratch,
                    output,
                    self.first_row - self.source_first_row,
                    self.first_row,
                    self.rows,
                );
            }
            Ok(())
        } else {
            unsafe {
                scale_rows(
                    &mut self.scaler,
                    input,
                    output,
                    self.first_row,
                    self.first_row,
                )
            }
        }
    }

    fn pack_rgb(&mut self, input: &Video, output: &mut [u8], row_bytes: usize) -> io::Result<()> {
        let scratch = self.scratch.as_ref().expect("RGB band scratch frame");
        let rows = output.len() / row_bytes;
        let first_row = self.first_row - self.source_first_row;
        let stride = scratch.stride(0);
        debug_assert!(rows <= self.rows as usize && row_bytes <= stride);
        unsafe {
            scale_rows(&mut self.scaler, input, scratch, self.source_first_row, 0)?;
            // libswscale writes only its aligned scratch frame. Copy the
            // visible bytes into this worker's independently borrowed slice.
            let source = (*scratch.as_ptr()).data[0].add(first_row as usize * stride);
            ffi::av_image_copy_plane(
                output.as_mut_ptr(),
                row_bytes as i32,
                source,
                stride as i32,
                row_bytes as i32,
                rows as i32,
            );
        }
        Ok(())
    }
}

struct PixelConverter {
    full: scaling::Context,
    parallel: Option<(rayon::ThreadPool, Vec<ColorBand>)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DecodeColor {
    matrix: i32,
    full_range: bool,
}

impl DecodeColor {
    fn from_surface(surface: &Video) -> Self {
        let matrix = match surface.color_space() {
            color::Space::BT709 => ffi::SWS_CS_ITU709,
            color::Space::FCC => ffi::SWS_CS_FCC,
            color::Space::SMPTE240M => ffi::SWS_CS_SMPTE240M,
            color::Space::BT2020NCL => ffi::SWS_CS_BT2020,
            _ => ffi::SWS_CS_DEFAULT,
        };
        Self {
            matrix,
            full_range: surface.color_range() == color::Range::JPEG,
        }
    }

    fn configure(self, scaler: &mut scaling::Context) -> io::Result<()> {
        // libswscale does not read AVFrame color properties from sws_scale.
        // The inverse table describes the encoded YUV. RGB output is full range.
        unsafe {
            let coefficients = ffi::sws_getCoefficients(self.matrix);
            if coefficients.is_null() {
                return Err(invalid("video color matrix is unavailable"));
            }
            checked_av(
                "configuring decoded video color",
                ffi::sws_setColorspaceDetails(
                    scaler.as_mut_ptr(),
                    coefficients,
                    i32::from(self.full_range),
                    coefficients,
                    1,
                    0,
                    1 << 16,
                    1 << 16,
                ),
            )
        }
    }
}

impl PixelConverter {
    fn new(input: Pixel, output: Pixel, width: u32, height: u32) -> io::Result<Self> {
        let full = scaler(input, output, width, height)?;
        let threads = std::thread::available_parallelism().map_or(1, usize::from);
        let parallel_yuv420 =
            std::env::var_os("JWM_REMOTE_PARALLEL_YUV420").as_deref() == Some("1".as_ref());
        let parallel = Self::bands(input, output, width, height, threads, parallel_yuv420);
        if let Some((_, bands)) = &parallel
            && bands.first().is_some_and(|band| band.scratch.is_some())
        {
            eprintln!(
                "jwm-remote: parallel 4:2:0 conversion active ({input:?} -> {output:?}, {width}x{height}, {} bands)",
                bands.len(),
            );
        }
        Ok(Self { full, parallel })
    }

    fn bands(
        input: Pixel,
        output: Pixel,
        width: u32,
        height: u32,
        threads: usize,
        parallel_yuv420: bool,
    ) -> Option<(rayon::ThreadPool, Vec<ColorBand>)> {
        let workers = threads.min(4).min(height as usize / 64);
        if workers < 2 || width * height < 1024 * 1024 {
            return None;
        }
        let margin = match (input, output) {
            // At 1:1, these formats can write directly to their own rows.
            (Pixel::RGB24, Pixel::YUV444P) | (Pixel::YUV444P, Pixel::RGB24) => 0,
            (Pixel::RGB24, Pixel::YUV420P) | (Pixel::YUV420P | Pixel::NV12, Pixel::RGB24)
                if parallel_yuv420 && width.is_multiple_of(2) && height.is_multiple_of(2) =>
            {
                // Chroma filters can use rows across a band boundary. Convert
                // the neighbors as well and retain only the interior. The even
                // coded size keeps the chroma sampling step identical in every
                // context. Any visible crop occurs after color conversion.
                8
            }
            _ => return None,
        };
        // Preserve both the chroma row alignment and the phase of libswscale's
        // 8-row dithering pattern, including the start of the extended band.
        let rows = height.div_ceil(workers as u32).next_multiple_of(8);
        let mut bands = Vec::with_capacity(workers);
        for first_row in (0..height).step_by(rows as usize) {
            let end_row = (first_row + rows).min(height);
            let source_first_row = first_row.saturating_sub(margin);
            let source_end_row = (end_row + margin).min(height);
            let source_height = source_end_row - source_first_row;
            bands.push(ColorBand {
                first_row,
                source_first_row,
                rows: end_row - first_row,
                scaler: scaler(input, output, width, source_height).ok()?,
                scratch: if margin == 0 {
                    None
                } else {
                    Some(video_buffer(output, width, source_height).ok()?)
                },
            });
        }
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(bands.len())
            .thread_name(|index| format!("jwm-remote-color-{index}"))
            .build()
            .ok()?;
        Some((pool, bands))
    }

    fn set_decode_color(&mut self, color: DecodeColor) -> io::Result<()> {
        color.configure(&mut self.full)?;
        if let Some((_, bands)) = self.parallel.as_mut() {
            for band in bands {
                color.configure(&mut band.scaler)?;
            }
        }
        Ok(())
    }

    fn convert(&mut self, input: &Video, output: &mut Video) -> io::Result<()> {
        let source = self.full.input();
        let target = self.full.output();
        if (input.format(), input.width(), input.height())
            != (source.format, source.width, source.height)
            || (output.format(), output.width(), output.height())
                != (target.format, target.width, target.height)
        {
            return Err(invalid("unexpected video pixel conversion format"));
        }

        if let Some((pool, bands)) = &mut self.parallel {
            let output = &*output;
            pool.install(|| {
                bands.par_iter_mut().try_for_each(|band| {
                    // Each call owns its context and writes a disjoint range
                    // of full-stride rows. All writes finish before return.
                    unsafe { band.convert(input, output) }
                })
            })
        } else {
            // This context covers the entire exclusively borrowed destination.
            unsafe { scale_rows(&mut self.full, input, output, 0, 0) }
        }
    }

    fn packs_rgb_bands(&self) -> bool {
        self.full.output().format == Pixel::RGB24
            && self
                .parallel
                .as_ref()
                .is_some_and(|(_, bands)| bands.first().is_some_and(|band| band.scratch.is_some()))
    }

    fn convert_packed_rgb(
        &mut self,
        input: &Video,
        visible: (u32, u32),
        output: &mut Vec<u8>,
    ) -> io::Result<usize> {
        let source = self.full.input();
        if !self.packs_rgb_bands()
            || (input.format(), input.width(), input.height())
                != (source.format, source.width, source.height)
            || visible.0 == 0
            || visible.1 == 0
            || visible.0 > source.width
            || visible.1 > source.height
        {
            return Err(invalid("unexpected packed video conversion format or crop"));
        }

        let (pool, bands) = self.parallel.as_mut().expect("packed RGB uses bands");
        // Every interior band has the first band's height. The final visible
        // chunk may end within a band, including an odd last row.
        let rows_per_band = bands[0].rows;
        let count = visible.1.div_ceil(rows_per_band) as usize;
        let row_bytes = visible.0 as usize * 3;
        let len = visible.1 as usize * row_bytes;
        output
            .try_reserve_exact(len.saturating_sub(output.len()))
            .map_err(|error| io::Error::other(format!("reserving video RGB output: {error}")))?;
        // The recycler preserves initialized bytes. Only new storage needs
        // initialization before the workers overwrite all visible pixels.
        output.resize(len, 0);
        let result = pool.install(|| {
            bands[..count]
                .par_iter_mut()
                .zip_eq(output.par_chunks_mut(rows_per_band as usize * row_bytes))
                .try_for_each(|(band, rows)| band.pack_rgb(input, rows, row_bytes))
        });
        if result.is_err() {
            output.clear();
        }
        result.map(|()| len)
    }
}

fn plane_row(format: Pixel, plane: usize, row: u32) -> u32 {
    if plane != 0 && matches!(format, Pixel::YUV420P | Pixel::NV12) {
        row / 2
    } else {
        row
    }
}

/// The caller owns the destination rows for the full lifetime of this call.
/// Source and destination offsets refer to luma rows and must be even for 4:2:0.
unsafe fn scale_rows(
    scaler: &mut scaling::Context,
    input: &Video,
    output: &Video,
    input_row: u32,
    output_row: u32,
) -> io::Result<()> {
    let height = scaler.input().height as i32;
    let rows = unsafe {
        let input = &*input.as_ptr();
        let output = &*output.as_ptr();
        let starts = |frame: &ffi::AVFrame, format, first_row| {
            let mut data = frame.data;
            for (index, (plane, stride)) in data.iter_mut().zip(frame.linesize).enumerate() {
                *plane = plane.wrapping_offset(
                    plane_row(format, index, first_row) as isize * stride as isize,
                );
            }
            data
        };
        ffi::sws_scale(
            scaler.as_mut_ptr(),
            starts(input, scaler.input().format, input_row)
                .as_ptr()
                .cast(),
            input.linesize.as_ptr(),
            0,
            height,
            starts(output, scaler.output().format, output_row).as_ptr(),
            output.linesize.as_ptr(),
        )
    };
    checked_av("converting video pixels", rows)?;
    if rows != height {
        return Err(invalid("incomplete video pixel conversion"));
    }
    Ok(())
}

/// Copy only this band's active pixels. The adjacent scratch rows belong to
/// the filter, and an adjacent band may be writing its own output concurrently.
unsafe fn copy_band_rows(
    input: &Video,
    output: &Video,
    input_row: u32,
    output_row: u32,
    rows: u32,
) {
    let width = output.width();
    let format = output.format();
    let widths: &[_] = match format {
        Pixel::RGB24 => &[width * 3],
        Pixel::YUV420P => &[width, width / 2, width / 2],
        _ => unreachable!("unsupported subsampled band output"),
    };
    unsafe {
        let input = &*input.as_ptr();
        let output = &*output.as_ptr();
        for (plane, &bytes) in widths.iter().enumerate() {
            let src_stride = input.linesize[plane];
            let dst_stride = output.linesize[plane];
            debug_assert!(bytes <= src_stride.unsigned_abs() && bytes <= dst_stride.unsigned_abs());
            let src = input.data[plane]
                .offset(plane_row(format, plane, input_row) as isize * src_stride as isize);
            let dst = output.data[plane]
                .offset(plane_row(format, plane, output_row) as isize * dst_stride as isize);
            ffi::av_image_copy_plane(
                dst,
                dst_stride,
                src,
                src_stride,
                bytes as i32,
                plane_row(format, plane, rows) as i32,
            );
        }
    }
}

fn require_no_queued_output(result: Result<(), ffmpeg::Error>) -> io::Result<()> {
    match result {
        Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => Ok(()),
        Ok(()) => Err(invalid("more than one video output per input")),
        Err(error) => Err(codec_error("checking video output boundary", error)),
    }
}

pub(crate) struct Encoder {
    codec: codec::encoder::video::Encoder,
    rgb: Video,
    yuv: Video,
    converter: PixelConverter,
    sequence: i64,
}

#[derive(Clone, Copy)]
pub(super) enum NvencRateControl {
    Cbr(u16),
    Hevc444Lossless,
}

#[derive(Clone, Copy, Default)]
pub(super) struct EncodeTimings {
    pub(super) rgb_copy: Duration,
    pub(super) conversion: Duration,
    pub(super) submit: Duration,
    pub(super) output: Duration,
}

impl Encoder {
    pub(crate) fn new(kind: FrameCodec, width: u32, height: u32, fps: u32) -> io::Result<Self> {
        Self::with_rate(kind, width, height, fps, NvencRateControl::Cbr(100))
    }

    pub(super) fn with_rate(
        kind: FrameCodec,
        width: u32,
        height: u32,
        fps: u32,
        rate_control: NvencRateControl,
    ) -> io::Result<Self> {
        validate_dimensions(width, height)?;
        let format = Format::from_codec(kind)?;
        let fps = i32::try_from(fps)
            .ok()
            .filter(|fps| (1..=120).contains(fps))
            .ok_or_else(|| invalid("unsupported hardware video frame rate"))?;
        let (bitrate, vbv, tune, rc) = match rate_control {
            NvencRateControl::Cbr(percent) if (1..=100).contains(&percent) => (
                format.bitrate * usize::from(percent) / 100,
                format.rate_control_buffer_bits * i32::from(percent) / 100,
                "ull",
                "cbr",
            ),
            NvencRateControl::Cbr(_) => {
                return Err(invalid("hardware video bitrate percentage is out of range"));
            }
            NvencRateControl::Hevc444Lossless if kind == FrameCodec::Hevc444 => {
                (0, 0, "lossless", "constqp")
            }
            NvencRateControl::Hevc444Lossless => {
                return Err(invalid("NVENC lossless requires HEVC 4:4:4"));
            }
        };
        let codec = ffmpeg::encoder::find_by_name(format.encoder)
            .ok_or_else(|| io::Error::other(format!("FFmpeg {} unavailable", format.encoder)))?;
        let mut encoder = codec::Context::new_with_codec(codec)
            .encoder()
            .video()
            .map_err(|error| codec_error("allocating NVENC encoder", error))?;
        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_format(format.pixels);
        encoder.set_time_base((1, fps));
        encoder.set_frame_rate(Some((fps, 1)));
        encoder.set_gop((2 * fps) as u32);
        encoder.set_max_b_frames(0);
        encoder.set_bit_rate(bitrate);
        encoder.set_max_bit_rate(bitrate);
        encoder.set_flags(codec::Flags::LOW_DELAY);
        // The codec context still belongs to encoder.
        unsafe {
            (*encoder.as_mut_ptr()).rc_buffer_size = vbv;
        }

        let mut options = ffmpeg::Dictionary::new();
        for (key, value) in [
            ("preset", format.preset),
            ("tune", tune),
            ("rc", rc),
            ("rc-lookahead", "0"),
            ("delay", "0"),
            ("zerolatency", "1"),
            ("forced-idr", "1"),
        ] {
            options.set(key, value);
        }
        if kind == FrameCodec::Hevc444 {
            options.set("profile", "rext");
        }
        if matches!(rate_control, NvencRateControl::Hevc444Lossless) {
            options.set("qp", "0");
        }
        let codec = encoder
            .open_as_with(codec, options)
            .map_err(|error| codec_error("opening NVENC encoder", error))?;

        Ok(Self {
            codec,
            rgb: video_buffer(Pixel::RGB24, width, height)?,
            yuv: video_buffer(format.pixels, width, height)?,
            converter: PixelConverter::new(Pixel::RGB24, format.pixels, width, height)?,
            sequence: 0,
        })
    }

    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.rgb.width(), self.rgb.height())
    }

    /// The packed RGB staging frame still holds the input to the last access unit.
    pub(crate) fn matches_last_input(&self, image: &RgbImage) -> bool {
        if self.sequence == 0 || image.dimensions() != self.dimensions() {
            return false;
        }
        let width = image.width() as usize * 3;
        let stride = self.rgb.stride(0);
        let stored = self.rgb.data(0);
        let pixels = image.as_raw().as_slice();
        if width == stride {
            return stored[..pixels.len()] == *pixels;
        }
        stored
            .chunks_exact(stride)
            .zip(pixels.chunks_exact(width))
            .all(|(row, pixels)| row[..width] == *pixels)
    }

    /// Append one complete Annex B access unit to the existing output.
    pub(crate) fn encode_into(
        &mut self,
        rgb: &RgbImage,
        output: &mut Vec<u8>,
        force_idr: bool,
        measure: bool,
    ) -> io::Result<EncodeTimings> {
        if rgb.dimensions() != self.dimensions() {
            return Err(invalid("capture size changed during video encoding"));
        }
        let next = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("video encoder sequence exhausted"))?;
        let mut timings = EncodeTimings::default();
        let copy_started = measure.then(Instant::now);
        let row_bytes = rgb.width() as usize * 3;
        let stride = self.rgb.stride(0);
        for (source, row) in rgb
            .as_raw()
            .chunks_exact(row_bytes)
            .zip(self.rgb.data_mut(0).chunks_exact_mut(stride))
        {
            row[..row_bytes].copy_from_slice(source);
        }
        timings.rgb_copy = copy_started.map_or(Duration::ZERO, |start| start.elapsed());

        // Sending an AVFrame can retain its allocation. Make it independent
        // before libswscale writes the next frame into the same AVFrame.
        let conversion_started = measure.then(Instant::now);
        checked_av("making NVENC input writable", unsafe {
            ffi::av_frame_make_writable(self.yuv.as_mut_ptr())
        })?;
        self.converter.convert(&self.rgb, &mut self.yuv)?;
        timings.conversion = conversion_started.map_or(Duration::ZERO, |start| start.elapsed());
        self.yuv.set_pts(Some(self.sequence));
        let keyframe = self.sequence == 0 || force_idr;
        self.yuv.set_kind(if keyframe {
            ffmpeg::picture::Type::I
        } else {
            ffmpeg::picture::Type::None
        });
        let submit_started = measure.then(Instant::now);
        self.codec
            .send_frame(&self.yuv)
            .map_err(|error| codec_error("submitting frame to NVENC", error))?;
        timings.submit = submit_started.map_or(Duration::ZERO, |start| start.elapsed());

        let output_started = measure.then(Instant::now);
        let mut packet = ffmpeg::Packet::empty();
        self.codec
            .receive_packet(&mut packet)
            .map_err(|error| codec_error("waiting for low-delay NVENC output", error))?;
        if packet.pts() != Some(self.sequence) || (keyframe && !packet.is_key()) {
            return Err(invalid(
                "unexpected NVENC packet timestamp or missing keyframe",
            ));
        }
        let bytes = packet.data().ok_or_else(|| invalid("empty NVENC packet"))?;
        validate_packet(bytes)?;
        require_no_queued_output(self.codec.receive_packet(&mut ffmpeg::Packet::empty()))?;
        timings.output = output_started.map_or(Duration::ZERO, |start| start.elapsed());

        output.extend_from_slice(bytes);
        self.sequence = next;
        Ok(timings)
    }
}

#[derive(Clone, Copy)]
enum Hardware {
    Vaapi,
    Cuda,
}

impl Hardware {
    fn name(self) -> &'static str {
        match self {
            Self::Vaapi => "VAAPI",
            Self::Cuda => "NVDEC",
        }
    }

    fn device(self) -> ffi::AVHWDeviceType {
        match self {
            Self::Vaapi => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            Self::Cuda => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
        }
    }

    fn pixel(self) -> ffi::AVPixelFormat {
        match self {
            Self::Vaapi => ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
            Self::Cuda => ffi::AVPixelFormat::AV_PIX_FMT_CUDA,
        }
    }
}

struct DecodeFormat {
    format: &'static Format,
    pixel: ffi::AVPixelFormat,
    width: i32,
    height: i32,
}

unsafe extern "C" fn select_hardware_format(
    context: *mut ffi::AVCodecContext,
    mut formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    let unavailable = ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    // FFmpeg supplies a non-null context and a sentinel-terminated format list.
    // The Decoder owns the immutable object to which the context's opaque
    // pointer refers. It outlives the codec and all of its callbacks.
    unsafe {
        let context = &*context;
        let format = &*context.opaque.cast::<DecodeFormat>();
        // Cropping can make a much larger coded frame appear to have the
        // expected output extent. Bound both before selecting a hardware
        // format so FFmpeg cannot allocate its hardware surfaces for it.
        if (context.width, context.height) != (format.width, format.height)
            || !format
                .format
                .accepts_coded_axis(format.width, context.coded_width)
            || !format
                .format
                .accepts_coded_axis(format.height, context.coded_height)
        {
            return unavailable;
        }
        while *formats != unavailable {
            if *formats == format.pixel {
                return format.pixel;
            }
            formats = formats.add(1);
        }
    }

    unavailable
}

pub(crate) struct Decoder {
    // Drop the codec before releasing its callback data.
    codec: codec::decoder::Video,
    format: Box<DecodeFormat>,
    hardware: Video,
    extra: Video,
    transfer: RgbTransfer,
    first: bool,
}

// Independent, exclusively owned FFmpeg and libswscale contexts have no thread
// affinity. The boxed callback state keeps the same address during a move.
unsafe impl Send for Decoder {}

struct DownloadBuffer {
    frame: Video,
    context: *mut ffi::AVBufferRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DownloadTarget {
    Populated,
    Reused,
    Replaced,
}

#[derive(Default)]
struct VaapiMap {
    enabled: bool,
    rejected: bool,
    staging: Option<Video>,
}

impl VaapiMap {
    fn stage(&mut self, source: &Video) -> bool {
        if !self.enabled || self.rejected {
            return false;
        }
        if let Err(error) = self.try_stage(source) {
            self.staging = None;
            self.rejected = true;
            eprintln!(
                "jwm-remote: direct VA-API NV12 mapping unavailable; using RGB download ({error})"
            );
            return false;
        }
        true
    }

    fn try_stage(&mut self, source: &Video) -> io::Result<()> {
        let (width, height) = (source.width(), source.height());
        validate_dimensions(width, height)?;
        let mut mapped = Video::empty();
        mapped.set_format(Pixel::NV12);
        // The source is leased by the sequential decoder. FFmpeg owns the
        // mapping and releases its extra source reference on drop.
        unsafe {
            let source = &*source.as_ptr();
            if source.format != ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32
                || source.hw_frames_ctx.is_null()
            {
                return Err(invalid("direct mapping requires a VA-API surface"));
            }
            checked_av(
                "mapping VA-API NV12 for CPU read",
                ffi::av_hwframe_map(
                    mapped.as_mut_ptr(),
                    source,
                    ffi::AV_HWFRAME_MAP_READ as i32 | ffi::AV_HWFRAME_MAP_DIRECT as i32,
                ),
            )?;
        }
        if self
            .staging
            .as_ref()
            .is_none_or(|frame| (frame.width(), frame.height()) != (width, height))
        {
            self.staging = Some(video_buffer(Pixel::NV12, width, height)?);
        }
        copy_mapped_nv12(
            &mapped,
            self.staging.as_mut().expect("NV12 staging allocated"),
        )
    }
}

fn copy_mapped_nv12(mapped: &Video, staging: &mut Video) -> io::Result<()> {
    let (width, height) = (staging.width() as i32, staging.height() as i32);
    if mapped.format() != Pixel::NV12
        || staging.format() != Pixel::NV12
        || (mapped.width(), mapped.height()) != (staging.width(), staging.height())
        || width <= 0
        || height <= 0
        || width % 2 != 0
        || height % 2 != 0
    {
        return Err(invalid("invalid direct NV12 mapping format or dimensions"));
    }
    // A mapped AVFrame exposes active rows without guaranteeing readable
    // padding for libswscale. Only its active pixels reach the padded frame.
    unsafe {
        let source = &*mapped.as_ptr();
        let destination = &mut *staging.as_mut_ptr();
        let frames = [source, &*destination];
        if frames.iter().any(|frame| {
            (0..2).any(|plane| frame.data[plane].is_null() || frame.linesize[plane] < width)
        }) {
            return Err(invalid("invalid direct NV12 mapping plane or stride"));
        }
        // The specialized copy can round a row up to a cache line.
        // Enable it only when every load stays within the active bytes.
        if width % 64 == 0
            && frames.iter().all(|frame| {
                (0..2).all(|plane| {
                    frame.linesize[plane] % 64 == 0 && frame.data[plane].addr().is_multiple_of(16)
                })
            })
        {
            let mut source_data = source.data.map(|plane| plane.cast_const());
            ffi::av_image_copy_uc_from(
                destination.data.as_mut_ptr(),
                destination.linesize.map(|stride| stride as isize).as_ptr(),
                source_data.as_mut_ptr(),
                source.linesize.map(|stride| stride as isize).as_ptr(),
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                width,
                height,
            );
        } else {
            for (plane, rows) in [height, height / 2].into_iter().enumerate() {
                ffi::av_image_copy_plane(
                    destination.data[plane],
                    destination.linesize[plane],
                    source.data[plane],
                    source.linesize[plane],
                    width,
                    rows,
                );
            }
        }
    }
    Ok(())
}

impl Default for DownloadBuffer {
    fn default() -> Self {
        Self {
            frame: Video::empty(),
            context: ptr::null_mut(),
        }
    }
}

impl Drop for DownloadBuffer {
    fn drop(&mut self) {
        // This reference is owned by the decoder and may be null.
        unsafe { ffi::av_buffer_unref(&mut self.context) };
    }
}

impl DownloadBuffer {
    fn clear(&mut self) {
        // Software download storage is private and no transfer is in flight.
        unsafe {
            ffi::av_frame_unref(self.frame.as_mut_ptr());
            ffi::av_buffer_unref(&mut self.context);
        }
    }

    fn backing(&self) -> ([*mut u8; 8], [*mut c_void; 8]) {
        // Record plane and buffer identity without taking an extra reference.
        // A reference would affect whether FFmpeg can write into the target.
        unsafe {
            let frame = &*self.frame.as_ptr();
            let buffers = frame.buf.map(|reference| {
                if reference.is_null() {
                    ptr::null_mut()
                } else {
                    (*reference).buffer.cast()
                }
            });
            (frame.data, buffers)
        }
    }

    fn transfer(
        &mut self,
        source: &Video,
        measure: bool,
    ) -> io::Result<(&Video, Option<DownloadTarget>)> {
        // The source's live hardware frame owns its frames context. Retain a
        // reference while caching the download so context identity cannot be
        // confused by allocator address reuse across decoder reconfiguration.
        let target = unsafe {
            let context = (*source.as_ptr()).hw_frames_ctx;
            if context.is_null() {
                return Err(invalid("video surface has no hardware frames context"));
            }
            if self.context.is_null() || (*context).buffer != (*self.context).buffer {
                self.clear();
                self.context = ffi::av_buffer_ref(context);
                if self.context.is_null() {
                    return Err(io::Error::other("retaining hardware frames context failed"));
                }
            }
            // No other consumer holds this software frame. FFmpeg can reuse
            // its original allocation, whose dimensions match this context.
            let before = measure.then(|| self.backing());
            checked_av(
                "downloading video hardware frame",
                ffi::av_hwframe_transfer_data(self.frame.as_mut_ptr(), source.as_ptr(), 0),
            )?;
            before.map(|before| {
                if before.0[0].is_null() {
                    DownloadTarget::Populated
                } else if before == self.backing() {
                    DownloadTarget::Reused
                } else {
                    DownloadTarget::Replaced
                }
            })
        };
        Ok((&self.frame, target))
    }
}

#[derive(Clone, Copy)]
pub(super) enum RgbCopy {
    FullFrame(usize),
    Banded(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RgbInput {
    Download(DownloadTarget),
    DirectMap(usize),
    MapFallback(DownloadTarget),
}

#[derive(Clone, Copy)]
pub(super) struct HardwareRgbWork {
    pub(super) input: RgbInput,
    pub(super) copy: RgbCopy,
    pub(super) checkout: Option<DecodeBufferCheckout>,
}

#[derive(Clone, Copy, Default)]
pub(super) struct DecodeTimings {
    pub(super) decode: Option<Duration>,
    pub(super) download: Option<Duration>,
    pub(super) direct_map: Option<Duration>,
    pub(super) conversion: Option<Duration>,
    pub(super) copy: Option<Duration>,
    pub(super) rgb_work: Option<HardwareRgbWork>,
}

impl DecodeTimings {
    pub(super) fn with_checkout(mut self, checkout: Option<DecodeBufferCheckout>) -> Self {
        if let Some(work) = &mut self.rgb_work {
            work.checkout = checkout;
        }
        self
    }
}

struct RgbConversion {
    converter: PixelConverter,
    color: DecodeColor,
    full_frame: Option<Video>,
}

#[derive(Default)]
pub(super) struct RgbTransfer {
    download: DownloadBuffer,
    vaapi_map: VaapiMap,
    conversion: Option<RgbConversion>,
}

impl RgbTransfer {
    fn convert(
        &mut self,
        surface: &Video,
        format: &Format,
        visible: (u32, u32),
        output: &mut Vec<u8>,
        measure: bool,
    ) -> io::Result<DecodeTimings> {
        let expected = (surface.width(), surface.height());
        let color = DecodeColor::from_surface(surface);
        let map_started = (measure && self.vaapi_map.enabled).then(Instant::now);
        let (input, input_work, download, direct_map) = if self.vaapi_map.stage(surface) {
            let staged = self.vaapi_map.staging.as_ref().expect("mapped NV12 staged");
            (
                staged,
                measure.then_some(RgbInput::DirectMap(
                    expected.0 as usize * expected.1 as usize * 3 / 2,
                )),
                None,
                map_started.map(|start| start.elapsed()),
            )
        } else {
            let download_started = measure.then(Instant::now);
            let (downloaded, target) = self.download.transfer(surface, measure)?;
            let work = target.map(|target| {
                if self.vaapi_map.enabled {
                    RgbInput::MapFallback(target)
                } else {
                    RgbInput::Download(target)
                }
            });
            (
                downloaded,
                work,
                download_started.map(|start| start.elapsed()),
                None,
            )
        };
        let conversion_started = measure.then(Instant::now);
        let pixels = input.format();
        if !format.accepts_transfer(pixels) {
            return Err(invalid(
                "hardware video did not preserve the negotiated chroma format",
            ));
        }
        if self.conversion.as_ref().is_none_or(|current| {
            let input = current.converter.full.input();
            input.format != pixels || (input.width, input.height) != expected
        }) {
            let mut converter = PixelConverter::new(pixels, Pixel::RGB24, expected.0, expected.1)?;
            converter.set_decode_color(color)?;
            let full_frame = if converter.packs_rgb_bands() {
                eprintln!("jwm-remote: parallel 4:2:0 packs directly into recycled RGB frames");
                None
            } else {
                Some(video_buffer(Pixel::RGB24, expected.0, expected.1)?)
            };
            self.conversion = Some(RgbConversion {
                converter,
                color,
                full_frame,
            });
        }
        let current = self.conversion.as_mut().expect("converter initialized");
        if current.color != color {
            current.converter.set_decode_color(color)?;
            current.color = color;
        }
        let (conversion, copy, copied) = if let Some(rgb) = current.full_frame.as_mut() {
            current.converter.convert(input, rgb)?;
            let conversion = conversion_started.map(|start| start.elapsed());
            let copy_started = measure.then(Instant::now);
            let bytes = copy_visible_rgb(rgb.data(0), rgb.stride(0), visible, output)?;
            (
                conversion,
                copy_started.map(|start| start.elapsed()),
                RgbCopy::FullFrame(bytes),
            )
        } else {
            let bytes = current
                .converter
                .convert_packed_rgb(input, visible, output)?;
            // Band conversion and packing overlap across workers. Report the
            // wall time together, with no separate full-frame copy sample.
            (
                conversion_started.map(|start| start.elapsed()),
                None,
                RgbCopy::Banded(bytes),
            )
        };
        Ok(DecodeTimings {
            download,
            direct_map,
            conversion,
            copy,
            rgb_work: input_work.map(|input| HardwareRgbWork {
                input,
                copy: copied,
                checkout: None,
            }),
            ..DecodeTimings::default()
        })
    }

    pub(super) fn vaapi_fallback(
        &mut self,
        surface: &Video,
        visible: (u32, u32),
        output: &mut Vec<u8>,
        measure: bool,
    ) -> io::Result<DecodeTimings> {
        self.convert(surface, &H264, visible, output, measure)
    }
}

impl Decoder {
    pub(crate) fn new(kind: FrameCodec, width: u32, height: u32) -> io::Result<Self> {
        validate_dimensions(width, height)?;
        let format = Format::from_codec(kind)?;
        Self::open(format, width, height, probe_device(format)?)
    }

    pub(super) fn native_vaapi(width: u32, height: u32, node: &CStr) -> io::Result<Self> {
        Self::open_device(&H264, width, height, Hardware::Vaapi, Some(node))
    }

    pub(super) fn decode_vaapi_surface(
        &mut self,
        bytes: &[u8],
        measure: bool,
    ) -> io::Result<(Video, DecodeTimings)> {
        if self.format.pixel != Hardware::Vaapi.pixel() || !std::ptr::eq(self.format.format, &H264)
        {
            return Err(invalid(
                "native presentation requires the H.264 VA-API decoder",
            ));
        }
        let decode = self.receive_surface(bytes, measure)?;
        self.first = false;
        let frame = std::mem::replace(&mut self.hardware, Video::empty());
        Ok((
            frame,
            DecodeTimings {
                decode,
                ..DecodeTimings::default()
            },
        ))
    }

    fn open(
        format: &'static Format,
        width: u32,
        height: u32,
        hardware: Hardware,
    ) -> io::Result<Self> {
        Self::open_device(format, width, height, hardware, None)
    }

    fn open_device(
        format: &'static Format,
        width: u32,
        height: u32,
        hardware: Hardware,
        device: Option<&CStr>,
    ) -> io::Result<Self> {
        validate_dimensions(width, height)?;
        let codec = ffmpeg::decoder::find(format.decoder).ok_or_else(|| {
            io::Error::other(format!("FFmpeg {} decoder unavailable", format.name))
        })?;
        let mut format = Box::new(DecodeFormat {
            format,
            pixel: hardware.pixel(),
            width: width as i32,
            height: height as i32,
        });
        // The context is dropped before format even if opening fails.
        let mut context = codec::Context::new_with_codec(codec).decoder();
        context.set_flags(codec::Flags::LOW_DELAY);

        unsafe {
            let context = &mut *context.as_mut_ptr();
            context.width = format.width;
            context.height = format.height;
            context.thread_count = 1;
            context.err_recognition = ffi::AV_EF_EXPLODE;
            // Generic buffers may pad both dimensions. The format callback
            // also requires the visible SPS dimensions to match the header.
            let padded_width = i64::from(width.next_multiple_of(64));
            let padded_height = i64::from(height.next_multiple_of(64));
            context.max_pixels = padded_width * padded_height;
            context.opaque = (&mut *format as *mut DecodeFormat).cast::<c_void>();
            context.get_format = Some(select_hardware_format);
            checked_av(
                "opening video hardware device",
                ffi::av_hwdevice_ctx_create(
                    &mut context.hw_device_ctx,
                    hardware.device(),
                    device.map_or(ptr::null::<c_char>(), CStr::as_ptr),
                    ptr::null_mut(),
                    0,
                ),
            )?;
        }
        let codec = context
            .open_as(codec)
            .and_then(|opened| opened.video())
            .map_err(|error| codec_error("opening hardware video decoder", error))?;

        Ok(Self {
            codec,
            format,
            hardware: Video::empty(),
            extra: Video::empty(),
            transfer: RgbTransfer::default(),
            first: true,
        })
    }

    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.format.width as u32, self.format.height as u32)
    }

    pub(super) fn enable_vaapi_map(&mut self) {
        self.transfer.vaapi_map.enabled = std::ptr::eq(self.format.format, &H264);
    }

    /// Discard queued output and reference pictures at a source or geometry change.
    pub(crate) fn reset(&mut self) {
        self.codec.flush();
        self.transfer.download.clear();
        self.transfer.vaapi_map.rejected = false;
        self.first = true;
    }

    pub(crate) fn decode_into(
        &mut self,
        bytes: &[u8],
        visible: (u32, u32),
        output: &mut Vec<u8>,
        measure: bool,
    ) -> io::Result<DecodeTimings> {
        let expected = self.dimensions();
        if visible.0 == 0 || visible.1 == 0 || visible.0 > expected.0 || visible.1 > expected.1 {
            return Err(invalid("invalid visible hardware video dimensions"));
        }
        let decode = self.receive_surface(bytes, measure)?;
        let result =
            self.transfer
                .convert(&self.hardware, self.format.format, visible, output, measure);
        // CPU conversion completed or failed. Drop our reference to
        // this GPU surface before preparing the next frame.
        unsafe { ffi::av_frame_unref(self.hardware.as_mut_ptr()) };
        let mut timings = result?;
        timings.decode = decode;
        self.first = false;
        Ok(timings)
    }

    fn receive_surface(&mut self, bytes: &[u8], measure: bool) -> io::Result<Option<Duration>> {
        validate_packet(bytes)?;
        let expected = self.dimensions();
        use ffmpeg::codec::packet::Mut;

        let mut packet = ffmpeg::Packet::empty();
        // av_new_packet overallocates and zeroes the bitreader padding.
        checked_av("allocating video packet", unsafe {
            ffi::av_new_packet(packet.as_mut_ptr(), bytes.len() as i32)
        })?;
        packet.data_mut().unwrap().copy_from_slice(bytes);
        let decode_started = measure.then(Instant::now);
        self.codec
            .send_packet(&packet)
            .map_err(|error| codec_error("submitting video packet", error))?;

        self.codec
            .receive_frame(&mut self.hardware)
            .map_err(|error| codec_error("waiting for low-delay video decode", error))?;
        // AV_PIX_FMT_VAAPI has different spelling in the Rust enum across
        // supported FFmpeg versions, so compare libavutil's raw AVPixelFormat.
        if unsafe { (*self.hardware.as_ptr()).format } != self.format.pixel as i32
            || (self.hardware.width(), self.hardware.height()) != expected
            || (self.first && !self.hardware.is_key())
        {
            return Err(invalid(
                "unexpected video hardware frame or missing keyframe",
            ));
        }
        require_no_queued_output(self.codec.receive_frame(&mut self.extra))?;
        Ok(decode_started.map(|start| start.elapsed()))
    }
}

pub(super) fn with_vaapi_probe_surface<T>(
    device: &CStr,
    bitstream: &[u8],
    use_surface: impl FnOnce(&Video) -> super::RemoteResult<T>,
) -> super::RemoteResult<T> {
    let mut decoder = Decoder::open_device(&H264, 256, 256, Hardware::Vaapi, Some(device))?;
    decoder.receive_surface(bitstream, false)?;
    use_surface(&decoder.hardware)
}

pub(super) fn vaapi_probe_rgb(device: &CStr, bitstream: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoder = Decoder::open_device(&H264, 256, 256, Hardware::Vaapi, Some(device))?;
    let mut pixels = Vec::new();
    decoder.decode_into(bitstream, (256, 256), &mut pixels, false)?;
    Ok(pixels)
}

fn copy_visible_rgb(
    pixels: &[u8],
    stride: usize,
    visible: (u32, u32),
    output: &mut Vec<u8>,
) -> io::Result<usize> {
    let row_bytes = visible.0 as usize * 3;
    let rows = visible.1 as usize;
    if stride < row_bytes
        || pixels
            .len()
            .checked_div(stride)
            .is_none_or(|size| size < rows)
    {
        return Err(invalid("invalid stride or row count in decoded video RGB"));
    }
    output.clear();
    output
        .try_reserve_exact(row_bytes * rows)
        .map_err(|error| io::Error::other(format!("reserving video RGB output: {error}")))?;
    for row in pixels.chunks_exact(stride).take(rows) {
        output.extend_from_slice(&row[..row_bytes]);
    }
    Ok(row_bytes * rows)
}

fn probe_device(format: &'static Format) -> io::Result<Hardware> {
    if let Some(&hardware) = format.device.get() {
        return Ok(hardware);
    }
    let mut failures = Vec::new();
    for hardware in format.devices {
        let result = Decoder::open(format, 256, 256, hardware).and_then(|mut decoder| {
            decoder.decode_into(format.probe, (256, 256), &mut Vec::new(), false)
        });
        match result {
            Ok(_) => {
                return Ok(*format.device.get_or_init(|| {
                    eprintln!(
                        "jwm-remote: {} hardware decoder active ({})",
                        format.name,
                        hardware.name()
                    );
                    hardware
                }));
            }
            Err(error) => failures.push(format!("{}: {error}", hardware.name())),
        }
    }

    Err(io::Error::other(format!(
        "{} hardware decode unavailable: {}",
        format.name,
        failures.join("; ")
    )))
}

pub(crate) fn probe_decode(kind: FrameCodec) -> io::Result<&'static str> {
    probe_device(Format::from_codec(kind)?).map(Hardware::name)
}

pub(crate) fn probe_encode(kind: FrameCodec) -> io::Result<()> {
    probe_encoder(Encoder::new(kind, 256, 256, 30)?)
}

pub(super) fn probe_hevc444_lossless() -> io::Result<()> {
    probe_encoder(Encoder::with_rate(
        FrameCodec::Hevc444,
        256,
        256,
        30,
        NvencRateControl::Hevc444Lossless,
    )?)
}

fn probe_encoder(mut encoder: Encoder) -> io::Result<()> {
    encoder
        .encode_into(&RgbImage::new(256, 256), &mut Vec::new(), false, false)
        .map(|_| ())
}

// One 256x256 baseline IDR generated from FFmpeg's solid-color test source
// with libx264, ultrafast, zerolatency and the encoder-information SEI removed.
pub(super) const H264_PROBE: &[u8] = b"\
    \x00\x00\x00\x01\x67\x42\xc0\x0d\xda\x04\x02\x1b\x01\x10\x00\x00\
    \x03\x00\x10\x00\x00\x03\x03\xc8\xf1\x42\xaa\x00\x00\x00\x01\x68\
    \xce\x0f\xc8\x00\x00\x01\x65\x88\x84\x3a\x11\x8a\x00\x02\x21\xf1\
    \xc0\x00\x40\x32\x38\x00\x08\x03\xc9\xc9\xc9\xc9\xc9\xc9\xc9\xc9\
    \xc9\xc9\xc9\xc9\xc9\xc9\xc9\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\
    \xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\
    \x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\
    \x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\
    \xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\
    \x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\
    \x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\
    \xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\
    \x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\
    \x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\
    \xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\
    \x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xd7\x5d\x75\xe0";

// One 256x256 HEVC Rext 4:4:4 IDR from NVENC, with SEI removed.
const HEVC444_PROBE: &[u8] = b"\
    \x00\x00\x00\x01\x40\x01\x0c\x01\xff\xff\x04\x08\x00\x00\x03\x00\
    \x9e\x08\x00\x00\x03\x00\x00\x3f\x97\x02\x40\x00\x00\x00\x01\x42\
    \x01\x01\x04\x08\x00\x00\x03\x00\x9e\x08\x00\x00\x03\x00\x00\x3f\
    \x90\x01\x01\x00\x80\xb2\xcb\xa5\x21\x08\xc8\xba\xa1\x80\x80\x80\
    \x00\x00\x03\x00\x80\x00\x00\x0f\x30\x02\xef\x28\x80\x01\xe8\x48\
    \x00\x01\xe8\x48\x08\x00\x00\x00\x01\x44\x01\xc1\xf7\x18\x30\x0c\
    \xc9\x00\x00\x01\x26\x01\xaf\x0f\xe0\x5a\xd9\x3f\xff\x3f\x39\xf2\
    \x3c\x9a\x4f\xff\x71\xb8\x97\xd7\x6e\xa9\x78\x5c\xc5\xc7\x8a\xec\
    \x79\x61\xbc\x00\x00\x03\x00\x00\x03\x00\x00\x03\x00\x00\x03\x00\
    \x00\x03\x00\x00\x03\x00\x00\x03\x00\x0e\x78";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_codec_rejects_odd_or_excessive_frames_and_unframed_bytes() {
        assert!(validate_dimensions(1392, 784).is_ok());
        assert!(validate_dimensions(1392, 783).is_err());
        assert!(validate_dimensions(4096, 2162).is_err());
        assert!(validate_dimensions(u32::MAX, u32::MAX).is_err());
        assert!(validate_packet(H264_PROBE).is_ok());
        assert!(validate_packet(&[0, 0, 0, 1]).is_err());
        let mut too_large = vec![0; MAX_PACKET_BYTES + 1];
        too_large[..4].copy_from_slice(&[0, 0, 1, 0x65]);
        assert_eq!(
            validate_packet(&too_large).unwrap_err().to_string(),
            "video access unit exceeds 1 MiB"
        );
    }

    #[test]
    fn nvenc_lossless_rejects_a_subsampled_codec_before_opening_the_device() {
        let result = Encoder::with_rate(
            FrameCodec::H264,
            256,
            256,
            30,
            NvencRateControl::Hevc444Lossless,
        );
        assert_eq!(
            result.err().unwrap().to_string(),
            "NVENC lossless requires HEVC 4:4:4"
        );
    }

    #[test]
    #[ignore = "requires NVENC with HEVC 4:4:4 lossless support"]
    fn hevc444_lossless_preserves_every_encoded_yuv_sample() {
        let (width, height) = (256, 256);
        let kind = FrameCodec::Hevc444;
        let mut encoder =
            Encoder::with_rate(kind, width, height, 30, NvencRateControl::Hevc444Lossless).unwrap();
        let codec = ffmpeg::decoder::find(codec::Id::HEVC).unwrap();
        let mut decoder = codec::Context::new_with_codec(codec).decoder();
        decoder.set_flags(codec::Flags::LOW_DELAY);
        // Decode synchronously so every submitted access unit has an output.
        unsafe { (*decoder.as_mut_ptr()).thread_count = 1 };
        let mut decoder = decoder.open_as(codec).unwrap().video().unwrap();
        let mut packet = Vec::new();
        // Alternate saturated adjacent pixels and shift their layout to
        // exercise temporal prediction as well as both forced IDRs.
        for index in 0..3 {
            let rgb = RgbImage::from_fn(width, height, |x, y| {
                let phase = if y < height / 2 { x } else { y };
                let colors = [[194, 44, 30], [20, 90, 240], [118, 118, 118]];
                image::Rgb(colors[((phase + index) % 3) as usize])
            });
            let idr = index != 1;
            packet.clear();
            encoder.encode_into(&rgb, &mut packet, idr, false).unwrap();
            assert_eq!(
                super::super::h264_wire::hardware_has_idr(kind, &packet),
                idr
            );
            decoder.send_packet(&ffmpeg::Packet::copy(&packet)).unwrap();
            let mut decoded = Video::empty();
            decoder.receive_frame(&mut decoded).unwrap();
            assert_eq!(decoded.format(), Pixel::YUV444P);
            assert_eq!((decoded.width(), decoded.height()), (width, height));
            for plane in 0..3 {
                let input = encoder
                    .yuv
                    .data(plane)
                    .chunks_exact(encoder.yuv.stride(plane));
                let output = decoded.data(plane).chunks_exact(decoded.stride(plane));
                assert!(input.len() >= height as usize && output.len() >= height as usize);
                for (row, (original, actual)) in input.zip(output).take(height as usize).enumerate()
                {
                    assert_eq!(
                        &actual[..width as usize],
                        &original[..width as usize],
                        "picture {index}, plane {plane}, row {row}"
                    );
                }
            }
        }
    }

    #[test]
    fn hevc_conversion_keeps_adjacent_pixels_full_chroma() {
        let (width, height) = (1280, 16);
        let colors = [[194, 44, 30], [20, 90, 240]];
        let mut rgb = video_buffer(Pixel::RGB24, width, height).unwrap();
        let mut yuv = video_buffer(Pixel::YUV444P, width, height).unwrap();
        let stride = rgb.stride(0);
        let row_bytes = width as usize * 3;
        for (y, row) in rgb.data_mut(0).chunks_exact_mut(stride).enumerate() {
            for (x, pixel) in row[..row_bytes].chunks_exact_mut(3).enumerate() {
                let phase = if y < height as usize / 2 { x } else { y };
                pixel.copy_from_slice(&colors[phase % 2]);
            }
        }
        let mut conversion =
            PixelConverter::new(Pixel::RGB24, Pixel::YUV444P, width, height).unwrap();
        conversion.convert(&rgb, &mut yuv).unwrap();
        assert!(yuv.data(1)[0].abs_diff(yuv.data(1)[1]) > 70);
        assert!(yuv.data(2)[0].abs_diff(yuv.data(2)[1]) > 70);
        assert!(!HEVC444.accepts_transfer(Pixel::NV12));

        let mut roundtrip = video_buffer(Pixel::RGB24, width, height).unwrap();
        let mut inverse = PixelConverter::new(Pixel::YUV444P, Pixel::RGB24, width, height).unwrap();
        inverse
            .set_decode_color(DecodeColor::from_surface(&yuv))
            .unwrap();
        inverse.convert(&yuv, &mut roundtrip).unwrap();
        for (y, (original, actual)) in rgb
            .data(0)
            .chunks_exact(stride)
            .zip(roundtrip.data(0).chunks_exact(roundtrip.stride(0)))
            .take(height as usize)
            .enumerate()
        {
            for (x, (want, have)) in original[..row_bytes]
                .chunks_exact(3)
                .zip(actual[..row_bytes].chunks_exact(3))
                .enumerate()
            {
                assert!(
                    have.iter().zip(want).all(|(a, b)| a.abs_diff(*b) <= 4),
                    "4:4:4 roundtrip at ({x},{y}) returned {have:?}, expected {want:?}"
                );
            }
        }
    }

    #[test]
    fn parallel_color_conversion_matches_full_frame_in_both_directions() {
        for (width, height, workers) in [(2560, 1440, 4), (1394, 786, 3)] {
            for (source, target) in [
                (Pixel::RGB24, Pixel::YUV444P),
                (Pixel::YUV444P, Pixel::RGB24),
            ] {
                let mut input = video_buffer(source, width, height).unwrap();
                let mut seed = 17u32;
                for plane in 0..input.planes() {
                    for byte in input.data_mut(plane) {
                        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        *byte = (seed >> 24) as u8;
                    }
                }
                let mut expected = video_buffer(target, width, height).unwrap();
                let mut actual = video_buffer(target, width, height).unwrap();
                let mut full = scaler(source, target, width, height).unwrap();
                full.run(&input, &mut expected).unwrap();
                let mut parallel = PixelConverter {
                    full,
                    parallel: Some(
                        PixelConverter::bands(source, target, width, height, workers, false)
                            .unwrap(),
                    ),
                };
                parallel.convert(&input, &mut actual).unwrap();
                let row_bytes = width as usize * if target == Pixel::RGB24 { 3 } else { 1 };
                for plane in 0..expected.planes() {
                    let rows = expected.data(plane).chunks_exact(expected.stride(plane));
                    let converted = actual.data(plane).chunks_exact(actual.stride(plane));
                    for (row, (a, b)) in rows.zip(converted).enumerate() {
                        assert!(a[..row_bytes] == b[..row_bytes], "plane {plane}, row {row}");
                    }
                }
            }
        }
    }

    #[test]
    fn subsampled_bands_match_full_frame_planes_and_declared_colors() {
        // 1394x786 exercises an unaligned row width and an uneven last band.
        // The other fixtures cover both native high-resolution shapes.
        for (width, height, workers) in [(1394, 786, 3), (2560, 1440, 4), (3840, 2160, 4)] {
            for (source, target) in [
                (Pixel::RGB24, Pixel::YUV420P),
                (Pixel::YUV420P, Pixel::RGB24),
                (Pixel::NV12, Pixel::RGB24),
            ] {
                let mut input = video_buffer(source, width, height).unwrap();
                let mut seed = 183u32;
                for plane in 0..input.planes() {
                    for byte in input.data_mut(plane) {
                        // Independent rows expose even a one-row chroma seam.
                        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        *byte = (seed >> 24) as u8;
                    }
                }
                let odd_crop = (width, height) == (1394, 786);
                let packed_size = (target == Pixel::RGB24).then_some(if odd_crop {
                    (width - 1, height - 1)
                } else {
                    (width, height)
                });
                let mut packed = Vec::new();
                if packed_size.is_some() && odd_crop {
                    // Poison the coded luma outside the visible right and
                    // bottom edges.
                    let stride = input.stride(0);
                    for row in input.data_mut(0).chunks_exact_mut(stride) {
                        row[width as usize - 1] = 0xfa;
                    }
                    input.data_mut(0)[(height as usize - 1) * stride..].fill(0xfb);
                }
                if packed_size.is_some() {
                    // Simulate a recycled packed frame, including a larger
                    // prior frame in the odd-crop case.
                    packed.resize(width as usize * height as usize * 3, 0xac);
                }

                let mut expected = video_buffer(target, width, height).unwrap();
                for plane in 0..expected.planes() {
                    let bytes = expected.stride(plane) * expected.plane_height(plane) as usize;
                    // The comparison also borrows the stride padding, which
                    // swscale need not write.
                    unsafe {
                        std::ptr::write_bytes((*expected.as_mut_ptr()).data[plane], 0, bytes);
                    }
                }
                let mut actual = video_buffer(target, width, height).unwrap();
                let mut full = scaler(source, target, width, height).unwrap();
                let mut parallel = PixelConverter {
                    full: scaler(source, target, width, height).unwrap(),
                    parallel: Some(
                        PixelConverter::bands(source, target, width, height, workers, true)
                            .unwrap(),
                    ),
                };

                let colors = if target == Pixel::YUV420P {
                    &[(ffi::SWS_CS_DEFAULT, false)][..]
                } else {
                    &[
                        (ffi::SWS_CS_DEFAULT, false),
                        (ffi::SWS_CS_ITU709, true),
                        (ffi::SWS_CS_DEFAULT, true),
                        (ffi::SWS_CS_ITU709, false),
                    ][..]
                };
                for &(matrix, full_range) in colors {
                    if target == Pixel::RGB24 {
                        let color = DecodeColor { matrix, full_range };
                        color.configure(&mut full).unwrap();
                        parallel.set_decode_color(color).unwrap();
                    }
                    unsafe { scale_rows(&mut full, &input, &expected, 0, 0) }.unwrap();
                    for plane in 0..actual.planes() {
                        actual.data_mut(plane).fill(0xad);
                    }
                    parallel.convert(&input, &mut actual).unwrap();

                    for plane in 0..expected.planes() {
                        let row_bytes = match target {
                            Pixel::RGB24 => width as usize * 3,
                            Pixel::YUV420P if plane != 0 => width as usize / 2,
                            _ => width as usize,
                        };
                        let rows = expected.data(plane).chunks_exact(expected.stride(plane));
                        let converted = actual.data(plane).chunks_exact(actual.stride(plane));
                        for (row, (want, have)) in rows.zip(converted).enumerate() {
                            assert!(
                                want[..row_bytes] == have[..row_bytes],
                                "{source:?} -> {target:?}, {width}x{height}, matrix {matrix}, \
                                 full range {full_range}, plane {plane}, row {row}",
                            );
                            assert!(have[row_bytes..].iter().all(|&byte| byte == 0xad));
                        }
                    }
                    if let Some(visible) = packed_size {
                        let mut reference_crop = Vec::new();
                        let full_bytes = copy_visible_rgb(
                            expected.data(0),
                            expected.stride(0),
                            visible,
                            &mut reference_crop,
                        )
                        .unwrap();
                        let allocation = packed.as_ptr();
                        packed.fill(0xac);
                        let banded_bytes = parallel
                            .convert_packed_rgb(&input, visible, &mut packed)
                            .unwrap();
                        let expected_bytes = (visible.0 * visible.1 * 3) as usize;
                        assert_eq!(full_bytes, expected_bytes);
                        assert_eq!(banded_bytes, expected_bytes);
                        assert_eq!(packed.as_ptr(), allocation);
                        assert_eq!(packed.len(), reference_crop.len());
                        let row_bytes = visible.0 as usize * 3;
                        for (row, (want, have)) in reference_crop
                            .chunks_exact(row_bytes)
                            .zip(packed.chunks_exact(row_bytes))
                            .enumerate()
                        {
                            assert!(
                                want == have,
                                "packed {source:?}, matrix {matrix}, full range {full_range}, row {row}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn subsampled_bands_require_opt_in_and_even_coded_dimensions() {
        for (source, target, width, height, threads, enabled) in [
            (Pixel::RGB24, Pixel::YUV420P, 2560, 1440, 4, false),
            (Pixel::NV12, Pixel::RGB24, 2560, 1440, 4, false),
            (Pixel::RGB24, Pixel::YUV420P, 1393, 786, 4, true),
            (Pixel::NV12, Pixel::RGB24, 1394, 785, 4, true),
            (Pixel::RGB24, Pixel::YUV444P, 640, 480, 4, true),
            (Pixel::YUV444P, Pixel::RGB24, 2560, 1440, 1, true),
        ] {
            assert!(
                PixelConverter::bands(source, target, width, height, threads, enabled).is_none()
            );
        }
    }

    #[test]
    fn mapped_nv12_copy_checks_layout_and_preserves_active_rows() {
        for width in [64, 66] {
            let height = 8;
            let mut source = video_buffer(Pixel::NV12, width, height).unwrap();
            let mut staging = video_buffer(Pixel::NV12, width, height).unwrap();
            for (plane, rows) in [height, height / 2].into_iter().enumerate() {
                let stride = source.stride(plane);
                for row in 0..rows as usize {
                    for (x, sample) in source.data_mut(plane)[row * stride..][..width as usize]
                        .iter_mut()
                        .enumerate()
                    {
                        *sample = (x + 19 * row + 73 * plane) as u8;
                    }
                }
            }
            copy_mapped_nv12(&source, &mut staging).unwrap();
            for (plane, rows) in [height, height / 2].into_iter().enumerate() {
                let source_stride = source.stride(plane);
                let staging_stride = staging.stride(plane);
                for row in 0..rows as usize {
                    assert_eq!(
                        source.data(plane)[row * source_stride..][..width as usize],
                        staging.data(plane)[row * staging_stride..][..width as usize],
                    );
                }
            }

            // Never read a driver plane which advertises less than one row.
            unsafe { (*source.as_mut_ptr()).linesize[1] = (width - 1) as i32 };
            assert!(copy_mapped_nv12(&source, &mut staging).is_err());
        }
    }

    #[test]
    #[ignore = "requires an explicit Intel VA-API render node"]
    fn direct_vaapi_map_matches_download_and_recovers_on_same_surface() {
        let node = std::env::var_os("JWM_TEST_VAAPI_NODE").expect("Intel render node required");
        let node = std::ffi::CString::new(node.as_encoded_bytes()).unwrap();
        with_vaapi_probe_surface(&node, H264_PROBE, |surface| {
            let visible = (251, 247);
            let mut fallback = RgbTransfer::default();
            let mut direct = RgbTransfer {
                vaapi_map: VaapiMap {
                    enabled: true,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut downloaded = Vec::new();
            let mut mapped = Vec::new();
            fallback.convert(surface, &H264, visible, &mut downloaded, true)?;
            let timings = direct.convert(surface, &H264, visible, &mut mapped, true)?;
            assert_eq!(mapped, downloaded);
            assert!(timings.download.is_none());
            assert!(timings.direct_map.is_some());
            assert_eq!(
                timings.rgb_work.unwrap().input,
                RgbInput::DirectMap(256 * 256 * 3 / 2)
            );

            // Simulate an ineligible mapping source while preserving the
            // decoded frame. Recovery must use its ordinary download.
            let ineligible = video_buffer(Pixel::NV12, 256, 256)?;
            assert!(!direct.vaapi_map.stage(&ineligible));
            assert!(direct.vaapi_map.staging.is_none());
            mapped.clear();
            let timings = direct.convert(surface, &H264, visible, &mut mapped, true)?;
            assert_eq!(mapped, downloaded);
            assert!(timings.direct_map.is_none());
            assert!(timings.download.is_some());
            assert_eq!(
                timings.rgb_work.unwrap().input,
                RgbInput::MapFallback(DownloadTarget::Populated)
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn yuv_download_uses_the_declared_matrix_and_range() {
        let mut input = video_buffer(Pixel::NV12, 16, 16).unwrap();
        let mut output = video_buffer(Pixel::RGB24, 16, 16).unwrap();
        let mut converter = PixelConverter::new(Pixel::NV12, Pixel::RGB24, 16, 16).unwrap();
        input.set_color_space(color::Space::BT709);
        input.set_color_range(color::Range::MPEG);
        input.data_mut(0).fill(63);
        for uv in input.data_mut(1).chunks_exact_mut(2) {
            uv.copy_from_slice(&[102, 240]);
        }
        converter
            .set_decode_color(DecodeColor::from_surface(&input))
            .unwrap();
        converter.convert(&input, &mut output).unwrap();
        let color = &output.data(0)[..3];
        assert!(
            color[0] >= 250 && color[1] <= 4 && color[2] <= 4,
            "{color:?}"
        );

        input.set_color_range(color::Range::JPEG);
        input.data_mut(0).fill(32);
        input.data_mut(1).fill(128);
        converter
            .set_decode_color(DecodeColor::from_surface(&input))
            .unwrap();
        converter.convert(&input, &mut output).unwrap();
        assert!(
            output.data(0)[..3]
                .iter()
                .all(|channel| channel.abs_diff(32) <= 2)
        );
    }

    #[test]
    fn rgb_copy_preserves_visible_rows_and_reuses_output() {
        let padded = RgbImage::from_fn(7, 8, |x, y| image::Rgb([x as u8, y as u8, 57]));
        let mut pixels = Vec::with_capacity(7 * 8 * 3);
        let allocation = pixels.as_ptr();
        for (width, height) in [(5, 7), (6, 7), (5, 8), (7, 8)] {
            let bytes =
                copy_visible_rgb(padded.as_raw(), 7 * 3, (width, height), &mut pixels).unwrap();
            assert_eq!(bytes, (width * height * 3) as usize);
            assert_eq!(pixels.len(), bytes);
            assert_eq!(pixels.as_ptr(), allocation);
            for row in 0..height {
                let begin = (row * width * 3) as usize;
                let source = (row * 7 * 3) as usize;
                let bytes = width as usize * 3;
                assert_eq!(
                    &pixels[begin..begin + bytes],
                    &padded.as_raw()[source..source + bytes]
                );
            }
        }
    }

    fn decode_test_image(
        decoder: &mut Decoder,
        packet: &[u8],
    ) -> io::Result<(RgbImage, HardwareRgbWork)> {
        let (width, height) = decoder.dimensions();
        let mut rgb = Vec::new();
        let timings = decoder.decode_into(packet, (width, height), &mut rgb, true)?;
        Ok((
            RgbImage::from_raw(width, height, rgb).expect("valid decoded test image"),
            timings.rgb_work.expect("measured hardware RGB work"),
        ))
    }

    fn assert_odd_visible_crop(kind: FrameCodec) {
        let (width, height) = (641, 359);
        let (encoded_width, encoded_height) =
            super::super::h264_wire::h264_padded_dimensions(width, height).unwrap();
        let visible = (u32::from(width), u32::from(height));
        let encoded = (u32::from(encoded_width), u32::from(encoded_height));
        let (right, bottom) = visible;
        let poison = 246;
        // Distinct markers identify both crop edges and their intersection.
        // Keep chroma neutral so this also works when NV12 shares a chroma
        // sample between the last visible pixel and a poisoned padding pixel.
        let picture = RgbImage::from_fn(encoded.0, encoded.1, |x, y| {
            let level = match (x, y) {
                (x, y) if x >= right || y >= bottom => poison,
                (x, y) if x >= right - 8 && y >= bottom - 8 => 52,
                (x, _) if x >= right - 8 => 93,
                (_, y) if y >= bottom - 8 => 158,
                _ => 121,
            };
            image::Rgb([level; 3])
        });
        let mut packet = Vec::new();
        Encoder::new(kind, encoded.0, encoded.1, 30)
            .unwrap()
            .encode_into(&picture, &mut packet, true, true)
            .unwrap();
        assert!(super::super::h264_wire::hardware_has_idr(kind, &packet));
        let mut decoder = Decoder::new(kind, encoded.0, encoded.1).unwrap();
        decoder.receive_surface(&packet, true).unwrap();
        // The decoder owns this live context. Its visible and coded sizes
        // have now been populated from the actual packet's SPS.
        let (sps_visible, coded) = unsafe {
            let context = &*decoder.codec.as_ptr();
            (
                (context.width, context.height),
                (context.coded_width, context.coded_height),
            )
        };
        assert_eq!(
            sps_visible,
            (i32::from(encoded_width), i32::from(encoded_height))
        );
        let format = decoder.format.format;
        assert!(format.accepts_coded_axis(sps_visible.0, coded.0));
        assert!(format.accepts_coded_axis(sps_visible.1, coded.1));
        let surface = &decoder.hardware;
        assert_eq!((surface.width(), surface.height()), encoded);
        let color_space = surface.color_space();
        let color_range = surface.color_range();
        let color_primaries = surface.color_primaries();
        let color_transfer = surface.color_transfer_characteristic();
        let chroma_location = surface.chroma_location();
        let decode_color = DecodeColor::from_surface(surface);

        let mut cropped = Vec::new();
        let timings = decoder
            .transfer
            .convert(surface, format, visible, &mut cropped, true)
            .unwrap();
        let work = timings.rgb_work.unwrap();
        assert_eq!(work.input, RgbInput::Download(DownloadTarget::Populated));
        assert!(matches!(work.copy, RgbCopy::FullFrame(690_357)));
        let uncropped = decoder
            .transfer
            .conversion
            .as_ref()
            .unwrap()
            .full_frame
            .as_ref()
            .unwrap();
        let stride = uncropped.stride(0);
        let row_bytes = usize::from(width) * 3;
        assert!(stride >= usize::from(encoded_width) * 3);
        assert_eq!(cropped.len(), row_bytes * usize::from(height));
        for (row, cropped_row) in cropped.chunks_exact(row_bytes).enumerate() {
            let start = row * stride;
            assert_eq!(cropped_row, &uncropped.data(0)[start..start + row_bytes]);
        }
        let cropped = RgbImage::from_raw(visible.0, visible.1, cropped).unwrap();
        assert_eq!(cropped.dimensions(), visible);
        let mut samples = Vec::new();
        for (x, y, level) in [
            (right / 2, bottom / 2, 121),
            (right - 1, bottom / 2, 93),
            (right / 2, bottom - 1, 158),
            (right - 1, bottom - 1, 52),
        ] {
            let pixel = cropped.get_pixel(x, y).0;
            assert!(
                pixel
                    .into_iter()
                    .all(|channel| channel.abs_diff(level) <= 12),
                "{} odd crop at ({x},{y}) returned {pixel:?}, expected gray {level}",
                kind.name()
            );
            samples.push((x, y, pixel));
        }
        let mut padding_samples = Vec::new();
        for (x, y) in [(right, bottom / 2), (right / 2, bottom), (right, bottom)] {
            let start = y as usize * stride + x as usize * 3;
            let pixel = &uncropped.data(0)[start..start + 3];
            let boundary = cropped.get_pixel(x.min(right - 1), y.min(bottom - 1)).0;
            // One isolated padding pixel may be quantized substantially.
            // It must still be distinguishable from the adjacent visible pixel.
            assert!(
                pixel
                    .iter()
                    .zip(boundary)
                    .all(|(a, b)| *a > b.saturating_add(24)),
                "{} protocol padding at ({x},{y}) returned {pixel:?}, visible edge {boundary:?}",
                kind.name()
            );
            padding_samples.push((x, y, pixel));
        }

        eprintln!(
            "JWM_REMOTE_CODEC_CROP {}",
            serde_json::json!({
                "codec": kind.name(),
                "visible": visible,
                "sps_visible": sps_visible,
                "sps_coded": coded,
                "surface": (surface.width(), surface.height()),
                "surface_format": format!("{:?}", decoder.format.pixel),
                "download_format": format!("{:?}", decoder.transfer.download.frame.format()),
                "color_space": format!("{color_space:?}"),
                "color_range": format!("{color_range:?}"),
                "color_primaries": format!("{color_primaries:?}"),
                "color_transfer": format!("{color_transfer:?}"),
                "chroma_location": format!("{chroma_location:?}"),
                "swscale_matrix": decode_color.matrix,
                "swscale_full_range": decode_color.full_range,
                "visible_row_bytes": row_bytes,
                "decoded_rgb_stride": stride,
                "visible_samples": samples,
                "padding_samples": padding_samples,
            })
        );
    }

    #[test]
    #[ignore = "requires NVENC and hardware decoders for H.264 and HEVC 4:4:4"]
    fn hardware_codec_uses_one_access_unit_per_frame_and_validates_sps_dimensions() {
        let (width, height) = (1392, 784);
        for kind in [FrameCodec::H264, FrameCodec::Hevc444] {
            let mut encoder = Encoder::new(kind, width, height, 30).unwrap();
            let mut decoder = Decoder::new(kind, width, height).unwrap();
            let mut packet = Vec::new();

            for (index, color) in [[56, 60, 68], [80, 96, 111], [90, 120, 135]]
                .into_iter()
                .enumerate()
            {
                packet.clear();
                let frame = RgbImage::from_pixel(width, height, image::Rgb(color));
                encoder
                    .encode_into(&frame, &mut packet, false, true)
                    .unwrap();
                let (decoded, work) = decode_test_image(&mut decoder, &packet).unwrap();
                let expected = if index == 0 {
                    DownloadTarget::Populated
                } else {
                    DownloadTarget::Reused
                };
                assert_eq!(work.input, RgbInput::Download(expected));
                let bytes = match work.copy {
                    RgbCopy::FullFrame(bytes) | RgbCopy::Banded(bytes) => bytes,
                };
                assert_eq!(bytes, (width * height * 3) as usize);
                let pixel = decoded.get_pixel(width / 2, height / 2).0;
                assert!(
                    pixel
                        .into_iter()
                        .zip(color)
                        .all(|(a, b)| a.abs_diff(b) <= 10)
                );
            }

            // A source switch retains NVENC but starts a fresh decoder.
            // The later IDR must contain enough headers to decode by itself.
            packet.clear();
            let color = [180, 130, 70];
            let frame = RgbImage::from_pixel(width, height, image::Rgb(color));
            encoder
                .encode_into(&frame, &mut packet, true, true)
                .unwrap();
            assert!(super::super::h264_wire::hardware_has_idr(kind, &packet));
            decoder.reset();
            let mut fresh = Decoder::new(kind, width, height).unwrap();
            for receiver in [&mut decoder, &mut fresh] {
                let (rgb, work) = decode_test_image(receiver, &packet).unwrap();
                assert_eq!(work.input, RgbInput::Download(DownloadTarget::Populated));
                assert!(
                    rgb.get_pixel(width / 2, height / 2)
                        .0
                        .into_iter()
                        .zip(color)
                        .all(|(a, b)| a.abs_diff(b) <= 10)
                );
            }

            let mut mismatched = Decoder::new(kind, 240, 256).unwrap();
            assert!(
                decode_test_image(&mut mismatched, Format::from_codec(kind).unwrap().probe)
                    .is_err()
            );
        }
        for kind in [FrameCodec::H264, FrameCodec::Hevc444] {
            assert_odd_visible_crop(kind);
        }
    }
}
