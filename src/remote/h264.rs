//! Per-session low-delay hardware video using FFmpeg's packet and frame APIs.

use std::ffi::{c_char, c_void};
use std::io;
use std::ptr;
use std::sync::OnceLock;

use ffmpeg::{codec, ffi, format::Pixel, frame::Video, software::scaling};
use ffmpeg_next as ffmpeg;
use image::RgbImage;

use super::messages::FrameCodec;

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
    let start = bytes
        .strip_prefix(&[0, 0, 1])
        .or_else(|| bytes.strip_prefix(&[0, 0, 0, 1]));
    if bytes.len() > MAX_PACKET_BYTES || !start.is_some_and(|nal| !nal.is_empty()) {
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
    let mut flags = scaling::Flags::FAST_BILINEAR;
    if input == Pixel::YUV444P || output == Pixel::YUV444P {
        // libswscale may otherwise average adjacent RGB samples before the
        // nominal 4:4:4 conversion. Preserve single-pixel chroma edges.
        flags |= scaling::Flags::FULL_CHR_H_INP | scaling::Flags::FULL_CHR_H_INT;
    }
    scaling::Context::get(input, width, height, output, width, height, flags)
        .map_err(|error| codec_error("opening video pixel converter", error))
}

fn convert(scaler: &mut scaling::Context, input: &Video, output: &mut Video) -> io::Result<()> {
    let source = scaler.input();
    let target = scaler.output();
    if input.format() != source.format
        || input.width() != source.width
        || input.height() != source.height
        || output.format() != target.format
        || output.width() != target.width
        || output.height() != target.height
    {
        return Err(invalid("unexpected video pixel conversion format"));
    }

    // Both frames have FFmpeg-allocated buffers of the dimensions and pixel
    // formats used to initialize this scaler. The destination is writable.
    let rows = unsafe {
        let input = &*input.as_ptr();
        let output = &mut *output.as_mut_ptr();
        ffi::sws_scale(
            scaler.as_mut_ptr(),
            input.data.as_ptr().cast(),
            input.linesize.as_ptr(),
            0,
            input.height,
            output.data.as_ptr().cast(),
            output.linesize.as_ptr(),
        )
    };
    checked_av("converting video pixels", rows)?;
    if rows as u32 != output.height() {
        return Err(invalid("incomplete video pixel conversion"));
    }

    Ok(())
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
    converter: scaling::Context,
    sequence: i64,
}

impl Encoder {
    pub(crate) fn new(kind: FrameCodec, width: u32, height: u32, fps: u32) -> io::Result<Self> {
        validate_dimensions(width, height)?;
        let format = Format::from_codec(kind)?;
        let fps = i32::try_from(fps)
            .ok()
            .filter(|fps| (1..=120).contains(fps))
            .ok_or_else(|| invalid("unsupported hardware video frame rate"))?;
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
        encoder.set_bit_rate(format.bitrate);
        encoder.set_max_bit_rate(format.bitrate);
        encoder.set_flags(codec::Flags::LOW_DELAY);
        // The codec context still belongs to encoder.
        unsafe {
            (*encoder.as_mut_ptr()).rc_buffer_size = format.rate_control_buffer_bits;
        }

        let mut options = ffmpeg::Dictionary::new();
        for (key, value) in [
            ("preset", format.preset),
            ("tune", "ull"),
            ("rc", "cbr"),
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
        let codec = encoder
            .open_as_with(codec, options)
            .map_err(|error| codec_error("opening NVENC encoder", error))?;

        Ok(Self {
            codec,
            rgb: video_buffer(Pixel::RGB24, width, height)?,
            yuv: video_buffer(format.pixels, width, height)?,
            converter: scaler(Pixel::RGB24, format.pixels, width, height)?,
            sequence: 0,
        })
    }

    /// Append one complete Annex B access unit to the existing output.
    pub(crate) fn encode_into(&mut self, rgb: &RgbImage, output: &mut Vec<u8>) -> io::Result<()> {
        if rgb.dimensions() != (self.rgb.width(), self.rgb.height()) {
            return Err(invalid("capture size changed during video encoding"));
        }
        let next = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("video encoder sequence exhausted"))?;
        let row_bytes = rgb.width() as usize * 3;
        let stride = self.rgb.stride(0);
        for (source, row) in rgb
            .as_raw()
            .chunks_exact(row_bytes)
            .zip(self.rgb.data_mut(0).chunks_exact_mut(stride))
        {
            row[..row_bytes].copy_from_slice(source);
        }

        // Sending an AVFrame can retain its allocation. Make it independent
        // before libswscale writes the next frame into the same AVFrame.
        checked_av("making NVENC input writable", unsafe {
            ffi::av_frame_make_writable(self.yuv.as_mut_ptr())
        })?;
        convert(&mut self.converter, &self.rgb, &mut self.yuv)?;
        self.yuv.set_pts(Some(self.sequence));
        self.yuv.set_kind(if self.sequence == 0 {
            ffmpeg::picture::Type::I
        } else {
            ffmpeg::picture::Type::None
        });
        self.codec
            .send_frame(&self.yuv)
            .map_err(|error| codec_error("submitting frame to NVENC", error))?;

        let mut packet = ffmpeg::Packet::empty();
        self.codec
            .receive_packet(&mut packet)
            .map_err(|error| codec_error("waiting for low-delay NVENC output", error))?;
        if packet.pts() != Some(self.sequence) || (self.sequence == 0 && !packet.is_key()) {
            return Err(invalid(
                "unexpected NVENC packet timestamp or missing keyframe",
            ));
        }
        let bytes = packet.data().ok_or_else(|| invalid("empty NVENC packet"))?;
        validate_packet(bytes)?;
        require_no_queued_output(self.codec.receive_packet(&mut ffmpeg::Packet::empty()))?;

        output.extend_from_slice(bytes);
        self.sequence = next;
        Ok(())
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
    rgb: Video,
    converter: Option<scaling::Context>,
    first: bool,
}

// Independent, exclusively owned FFmpeg and libswscale contexts have no thread
// affinity. The boxed callback state keeps the same address during a move.
unsafe impl Send for Decoder {}

impl Decoder {
    pub(crate) fn new(kind: FrameCodec, width: u32, height: u32) -> io::Result<Self> {
        validate_dimensions(width, height)?;
        let format = Format::from_codec(kind)?;
        Self::open(format, width, height, probe_device(format)?)
    }

    fn open(
        format: &'static Format,
        width: u32,
        height: u32,
        hardware: Hardware,
    ) -> io::Result<Self> {
        validate_dimensions(width, height)?;
        let codec = ffmpeg::decoder::find(format.decoder).ok_or_else(|| {
            io::Error::other(format!("FFmpeg {} decoder unavailable", format.name))
        })?;
        let rgb = video_buffer(Pixel::RGB24, width, height)?;
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
                    ptr::null::<c_char>(),
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
            rgb,
            converter: None,
            first: true,
        })
    }

    pub(crate) fn decode(&mut self, bytes: &[u8]) -> io::Result<RgbImage> {
        validate_packet(bytes)?;
        use ffmpeg::codec::packet::Mut;

        let mut packet = ffmpeg::Packet::empty();
        // av_new_packet overallocates and zeroes the bitreader padding.
        checked_av("allocating video packet", unsafe {
            ffi::av_new_packet(packet.as_mut_ptr(), bytes.len() as i32)
        })?;
        packet.data_mut().unwrap().copy_from_slice(bytes);
        self.codec
            .send_packet(&packet)
            .map_err(|error| codec_error("submitting video packet", error))?;

        let mut hardware = Video::empty();
        self.codec
            .receive_frame(&mut hardware)
            .map_err(|error| codec_error("waiting for low-delay video decode", error))?;
        let expected = (self.format.width as u32, self.format.height as u32);
        // AV_PIX_FMT_VAAPI has different spelling in the Rust enum across
        // supported FFmpeg versions, so compare libavutil's raw AVPixelFormat.
        if unsafe { (*hardware.as_ptr()).format } != self.format.pixel as i32
            || (hardware.width(), hardware.height()) != expected
            || (self.first && !hardware.is_key())
        {
            return Err(invalid(
                "unexpected video hardware frame or missing keyframe",
            ));
        }
        require_no_queued_output(self.codec.receive_frame(&mut Video::empty()))?;

        // Download into an FFmpeg-owned software AVFrame. JWM's existing
        // presentation paths accept packed RGB, so no hardware surface can
        // reach the viewer after it leaves this decoder.
        let mut downloaded = Video::empty();
        checked_av("downloading video hardware frame", unsafe {
            ffi::av_hwframe_transfer_data(downloaded.as_mut_ptr(), hardware.as_ptr(), 0)
        })?;
        let format = downloaded.format();
        if !self.format.format.accepts_transfer(format) {
            return Err(invalid(
                "hardware video did not preserve the negotiated chroma format",
            ));
        }
        if self
            .converter
            .as_ref()
            .is_none_or(|converter| converter.input().format != format)
        {
            self.converter = Some(scaler(format, Pixel::RGB24, expected.0, expected.1)?);
        }
        let converter = self.converter.as_mut().expect("converter initialized");
        convert(converter, &downloaded, &mut self.rgb)?;
        let row_bytes = expected.0 as usize * 3;
        let mut rgb = Vec::with_capacity(row_bytes * expected.1 as usize);
        for row in self
            .rgb
            .data(0)
            .chunks_exact(self.rgb.stride(0))
            .take(expected.1 as usize)
        {
            rgb.extend_from_slice(&row[..row_bytes]);
        }
        self.first = false;
        RgbImage::from_raw(expected.0, expected.1, rgb)
            .ok_or_else(|| invalid("invalid video RGB output"))
    }
}

fn probe_device(format: &'static Format) -> io::Result<Hardware> {
    if let Some(&hardware) = format.device.get() {
        return Ok(hardware);
    }
    let mut failures = Vec::new();
    for hardware in format.devices {
        let result = Decoder::open(format, 256, 256, hardware)
            .and_then(|mut decoder| decoder.decode(format.probe));
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
            Err(error) => failures.push(error.to_string()),
        }
    }

    Err(io::Error::other(format!(
        "{} hardware decode unavailable: {}",
        format.name,
        failures.join("; ")
    )))
}

pub(crate) fn probe_decode(kind: FrameCodec) -> io::Result<()> {
    probe_device(Format::from_codec(kind)?).map(|_| ())
}

pub(crate) fn probe_encode(kind: FrameCodec) -> io::Result<()> {
    let mut encoder = Encoder::new(kind, 256, 256, 30)?;
    encoder.encode_into(&RgbImage::new(256, 256), &mut Vec::new())
}

// One 256x256 baseline IDR generated from FFmpeg's solid-color test source
// with libx264, ultrafast, zerolatency and the encoder-information SEI removed.
const H264_PROBE: &[u8] = b"\
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
        assert!(validate_packet(&too_large).is_err());
    }

    #[test]
    fn hevc_conversion_keeps_adjacent_pixels_full_chroma() {
        let mut rgb = video_buffer(Pixel::RGB24, 8, 8).unwrap();
        let mut yuv = video_buffer(Pixel::YUV444P, 8, 8).unwrap();
        let stride = rgb.stride(0);
        for row in rgb.data_mut(0).chunks_exact_mut(stride) {
            for pair in row[..24].chunks_exact_mut(6) {
                pair.copy_from_slice(&[220, 37, 35, 34, 98, 208]);
            }
        }
        let mut conversion = scaler(Pixel::RGB24, Pixel::YUV444P, 8, 8).unwrap();
        convert(&mut conversion, &rgb, &mut yuv).unwrap();
        assert!(yuv.data(1)[0].abs_diff(yuv.data(1)[1]) > 70);
        assert!(yuv.data(2)[0].abs_diff(yuv.data(2)[1]) > 70);
        assert!(!HEVC444.accepts_transfer(Pixel::NV12));
    }

    #[test]
    #[ignore = "requires NVENC and hardware decoders for H.264 and HEVC 4:4:4"]
    fn hardware_codec_uses_one_access_unit_per_frame_and_validates_sps_dimensions() {
        let (width, height) = (1392, 784);
        for kind in [FrameCodec::H264, FrameCodec::Hevc444] {
            let mut encoder = Encoder::new(kind, width, height, 30).unwrap();
            let mut decoder = Decoder::new(kind, width, height).unwrap();
            let mut packet = Vec::new();

            for color in [[56, 60, 68], [80, 96, 111], [90, 120, 135]] {
                packet.clear();
                let frame = RgbImage::from_pixel(width, height, image::Rgb(color));
                encoder.encode_into(&frame, &mut packet).unwrap();
                let decoded = decoder.decode(&packet).unwrap();
                let pixel = decoded.get_pixel(width / 2, height / 2).0;
                assert!(
                    pixel
                        .into_iter()
                        .zip(color)
                        .all(|(a, b)| a.abs_diff(b) <= 10)
                );
            }

            let mut mismatched = Decoder::new(kind, 240, 256).unwrap();
            assert!(
                mismatched
                    .decode(Format::from_codec(kind).unwrap().probe)
                    .is_err()
            );
        }
    }
}
