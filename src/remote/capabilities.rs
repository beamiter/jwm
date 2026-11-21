//! Build and local codec capability checks used by the remote helper.

use super::{RemoteResult, messages::FrameCodec};
use std::path::{Path, PathBuf};

pub const NATIVE_WAYLAND: bool = cfg!(feature = "remote-wayland");
pub const HARDWARE_VIDEO: bool = cfg!(feature = "remote-h264");
pub const FAST_JPEG: bool = cfg!(feature = "remote-fast-jpeg");

pub const HARDWARE_CODECS: [FrameCodec; 2] = [FrameCodec::H264, FrameCodec::Hevc444];

/// Submit a local test picture through the same backend used by a session.
pub fn probe_encoder(codec: FrameCodec) -> std::io::Result<&'static str> {
    #[cfg(feature = "remote-h264")]
    {
        super::h264::probe_encode(codec)?;
        Ok("NVENC")
    }
    #[cfg(not(feature = "remote-h264"))]
    {
        let _ = codec;
        Err(std::io::Error::other(
            "remote-h264 was disabled at build time",
        ))
    }
}

/// Decode a local test bitstream and identify the chosen hardware backend.
pub fn probe_decoder(codec: FrameCodec) -> std::io::Result<&'static str> {
    #[cfg(feature = "remote-h264")]
    {
        super::h264::probe_decode(codec)
    }
    #[cfg(not(feature = "remote-h264"))]
    {
        let _ = codec;
        Err(std::io::Error::other(
            "remote-h264 was disabled at build time",
        ))
    }
}

pub fn viewer_wayland_socket(display: Option<&str>) -> RemoteResult<PathBuf> {
    #[cfg(feature = "remote-wayland")]
    {
        super::wayland_viewer::socket_path(display)
    }
    #[cfg(not(feature = "remote-wayland"))]
    {
        let _ = display;
        Err("native Wayland requires the remote-wayland build feature".into())
    }
}

pub fn probe_gpu_import(render_node: &Path) -> RemoteResult<String> {
    #[cfg(feature = "remote-h264")]
    {
        super::gpu_import::probe(render_node)
    }
    #[cfg(not(feature = "remote-h264"))]
    {
        let _ = render_node;
        Err("GPU import probe requires the remote-h264 build feature".into())
    }
}

pub fn probe_gpu_present(render_node: &Path, display: Option<&str>) -> RemoteResult<String> {
    #[cfg(feature = "remote-h264")]
    {
        super::gpu_present::probe(render_node, display)
    }
    #[cfg(not(feature = "remote-h264"))]
    {
        let _ = (render_node, display);
        Err("GPU rendering probe requires the remote-h264 build feature".into())
    }
}

pub fn probe_gpu_encode(render_node: &Path) -> RemoteResult<String> {
    #[cfg(feature = "remote-h264")]
    {
        super::gpu_encode::probe(render_node)
    }
    #[cfg(not(feature = "remote-h264"))]
    {
        let _ = render_node;
        Err("GPU encode import probe requires the remote-h264 build feature".into())
    }
}
