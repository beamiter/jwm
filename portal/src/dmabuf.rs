//! GBM dmabuf allocator + 3-buffer pool for the screen-capture transport.
//!
//! Owns a `gbm::Device` opened on whatever DRM render node the compositor
//! announced via `ext_image_copy_capture_session_v1::dmabuf_device`, and
//! allocates `BufferObject`s using LINEAR Argb8888 / Xrgb8888 — the only
//! format/modifier combination this slice supports.
//!
//! Pool synchronization model (Stage 1+2+3, "full sync"):
//!   * Three slots, fixed at startup. State per slot:
//!       Free        — neither side touching it
//!       Filling     — capture thread has it attached to a wayland frame
//!       Ready       — capture wrote pixels; PW side hasn't picked yet
//!       PwInFlight  — PW dequeued + queued this slot, hasn't released it
//!   * Capture picks `Free` (or the oldest `Ready`, replacing it) → Filling
//!     → on Ready event, atomic transition to Ready w/ a monotonic seq.
//!   * PW `on_process` scans for highest-seq Ready → marks PwInFlight,
//!     binds its fd into the dequeued pw_buffer's data[0], queues.
//!     On the *following* process call, all PwInFlight slots whose
//!     pw_buffer differs from the just-dequeued one transition back to Free
//!     (PW is guaranteed to have released them by then in the
//!     produce-then-immediately-consume model with `MAP_BUFFERS` disabled).

#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gbm::{BufferObject, BufferObjectFlags, Device as GbmDevice, Format as GbmFormat, Modifier};
use log::{debug, info, warn};
use wayland_client::{
    Dispatch, QueueHandle,
    protocol::wl_buffer::WlBuffer,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

/// Linear, non-modified — the only modifier we declare support for in this
/// slice. Equivalent to `DRM_FORMAT_MOD_LINEAR` (0).
pub const MOD_LINEAR: u64 = 0;

/// Fourcc constants we accept. Names mirror DRM (NOT Wayland wl_shm naming),
/// so Argb8888 here == `wl_shm::Format::Argb8888` == `DRM_FORMAT_ARGB8888`.
pub mod fourcc {
    pub const ARGB8888: u32 = 0x34325241; // 'AR24' little-endian
    pub const XRGB8888: u32 = 0x34325258; // 'XR24' little-endian
}

/// One allocated GBM buffer object plus its exported dmabuf fd. The fd lives
/// for the lifetime of this struct; closing it on drop is implicit (OwnedFd).
pub struct DmabufBuffer {
    bo: BufferObject<()>,
    fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    pub fourcc: u32,
}

impl DmabufBuffer {
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.fd)
    }

    /// Number of planes. We only support single-plane formats in this slice,
    /// so this should always be 1; the field exists so the linux-dmabuf
    /// `params.add` loop is correct if we ever extend to NV12/etc.
    pub fn plane_count(&self) -> u32 {
        self.bo.plane_count()
    }
}

/// Owning handle to a GBM device opened on a specific DRM node. Cloning this
/// is cheap (Arc internally via `Device<File>` deref) — but we don't bother
/// since the capture thread is single-owner.
pub struct GbmContext {
    pub node_path: PathBuf,
    device: GbmDevice<File>,
}

impl GbmContext {
    /// Open GBM on a DRM render node that matches `dmabuf_device` from the
    /// session. `dev` is the dev_t the compositor announced (interpreted
    /// little-endian from the protocol's opaque byte array — see capture.rs).
    ///
    /// When `dev` is `None` we fall back to scanning `/dev/dri/renderD*` and
    /// taking the first openable node. That's correct for single-GPU systems
    /// and "good enough" for prime/multi-GPU (the compositor will have
    /// allocated *its* dmabuf on its render node, so an import on the same
    /// fd will at worst trigger a slow path inside Mesa).
    pub fn open(dev: Option<u64>) -> io::Result<Self> {
        let path = pick_render_node(dev)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(&path)?;
        let device = GbmDevice::new(file)?;
        info!("dmabuf: opened GBM device on {}", path.display());
        Ok(Self { node_path: path, device })
    }

    /// Allocate one LINEAR buffer for the given dimensions + format. Fails
    /// (returns IoError) if the driver can't satisfy the request — caller is
    /// responsible for falling back to SHM.
    pub fn allocate_linear(
        &self,
        width: u32,
        height: u32,
        fourcc: u32,
    ) -> io::Result<DmabufBuffer> {
        let format = match fourcc {
            self::fourcc::ARGB8888 => GbmFormat::Argb8888,
            self::fourcc::XRGB8888 => GbmFormat::Xrgb8888,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsupported fourcc 0x{:x}", other),
                ));
            }
        };
        // With an explicit modifier list, usage flags are largely
        // descriptive — i915 in particular rejects `RENDERING` combined with
        // an explicit modifier (EINVAL). The modifier itself (LINEAR) fully
        // specifies the layout; we just declare "no special usage" here.
        let usage = BufferObjectFlags::empty();
        let mods = [Modifier::Linear].into_iter();
        let bo = self
            .device
            .create_buffer_object_with_modifiers2::<()>(width, height, format, mods, usage)
            .map_err(|e| io::Error::new(e.kind(), format!("gbm alloc: {e}")))?;
        let fd = bo
            .fd()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("gbm bo fd: {e}")))?;
        let stride = bo.stride();
        let offset = bo.offset(0);
        let modifier: u64 = bo.modifier().into();
        debug!(
            "dmabuf: allocated {width}x{height} fourcc=0x{fourcc:x} \
             stride={stride} offset={offset} modifier=0x{modifier:x}"
        );
        Ok(DmabufBuffer {
            bo,
            fd,
            width,
            height,
            stride,
            offset,
            modifier,
            fourcc,
        })
    }
}

/// Wrap a DmabufBuffer in a wl_buffer via linux-dmabuf-v1 `create_immed`.
/// Caller is responsible for binding `dmabuf` (must be v3+ so the modifier
/// args on `add` are honored, and v2+ for `create_immed` itself). The returned
/// wl_buffer is destroyed by caller when no longer needed.
///
/// The `params` proxy is created + immediately destroyed inside this helper;
/// `create_immed` returns the wl_buffer synchronously (no `created`/`failed`
/// event roundtrip needed) — failure surfaces as a protocol error on the
/// connection, which kills the capture thread on the next dispatch.
pub fn create_wl_buffer<D>(
    dmabuf: &ZwpLinuxDmabufV1,
    buffer: &DmabufBuffer,
    qh: &QueueHandle<D>,
) -> WlBuffer
where
    D: Dispatch<ZwpLinuxBufferParamsV1, ()> + Dispatch<WlBuffer, ()> + 'static,
{
    let params: ZwpLinuxBufferParamsV1 = dmabuf.create_params(qh, ());
    // Single-plane formats only in this slice. plane_idx 0, offset from BO,
    // stride from BO, modifier split into hi/lo as the protocol expects.
    let mod_hi = (buffer.modifier >> 32) as u32;
    let mod_lo = (buffer.modifier & 0xffff_ffff) as u32;
    params.add(buffer.fd(), 0, buffer.offset, buffer.stride, mod_hi, mod_lo);
    let wl_buf = params.create_immed(
        buffer.width as i32,
        buffer.height as i32,
        buffer.fourcc,
        wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::Flags::empty(),
        qh,
        (),
    );
    params.destroy();
    wl_buf
}

/// Resolve a usable DRM render node. Prefers the one matching the
/// compositor's announced dev_t; otherwise scans for any renderD* node.
fn pick_render_node(want: Option<u64>) -> io::Result<PathBuf> {
    let dir = std::fs::read_dir("/dev/dri")?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("renderD") {
            continue;
        }
        let path = entry.path();
        if let Some(want) = want {
            if let Ok(meta) = std::fs::metadata(&path) {
                use std::os::unix::fs::MetadataExt;
                if meta.rdev() == want {
                    return Ok(path);
                }
            }
        }
        candidates.push(path);
    }
    if let Some(want) = want {
        debug!(
            "dmabuf: no renderD* matches dev=0x{want:x}; falling back to first ({} candidates)",
            candidates.len()
        );
    }
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no /dev/dri/renderD* node"))
}

/// State of one pool slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Free,
    Filling,
    Ready,
    PwInFlight,
}

/// One pool entry. The wl_buffer reference is tracked outside this struct
/// (in capture.rs) because wayland-client proxies aren't Send/Sync-safe to
/// move into the pool's Arc<Mutex<...>>.
pub struct PoolSlot {
    pub buf: DmabufBuffer,
    pub state: SlotState,
    /// Monotonic frame sequence — the most recently filled Ready slot is the
    /// one PW should pick.
    pub seq: u64,
}

pub struct DmabufPool {
    pub slots: Vec<PoolSlot>,
    next_seq: u64,
}

pub type SharedPool = Arc<Mutex<DmabufPool>>;

impl DmabufPool {
    pub fn new(buffers: Vec<DmabufBuffer>) -> Self {
        let slots = buffers
            .into_iter()
            .map(|buf| PoolSlot { buf, state: SlotState::Free, seq: 0 })
            .collect();
        Self { slots, next_seq: 1 }
    }

    /// Pick the next slot the capture thread should write into. Prefer Free,
    /// then the oldest Ready (overwrite-on-write semantics — PW is too slow,
    /// drop the stale frame), and as a last resort skip this cycle.
    pub fn pick_for_capture(&mut self) -> Option<usize> {
        if let Some((i, _)) = self
            .slots
            .iter()
            .enumerate()
            .find(|(_, s)| s.state == SlotState::Free)
        {
            self.slots[i].state = SlotState::Filling;
            return Some(i);
        }
        // Steal the oldest Ready slot.
        let oldest = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.state == SlotState::Ready)
            .min_by_key(|(_, s)| s.seq)
            .map(|(i, _)| i);
        if let Some(i) = oldest {
            warn!("dmabuf: stealing Ready slot {i} for capture (PW too slow)");
            self.slots[i].state = SlotState::Filling;
            return Some(i);
        }
        None
    }

    /// Mark a slot's capture complete. Bumps seq.
    pub fn mark_ready(&mut self, idx: usize) {
        if let Some(slot) = self.slots.get_mut(idx) {
            slot.state = SlotState::Ready;
            slot.seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1).max(1);
        }
    }

    /// Mark a slot's capture failed — return it to Free without bumping seq.
    pub fn mark_failed(&mut self, idx: usize) {
        if let Some(slot) = self.slots.get_mut(idx) {
            slot.state = SlotState::Free;
        }
    }

    /// PW side: pick the freshest Ready slot (highest seq). Marks PwInFlight.
    pub fn pick_for_pw(&mut self) -> Option<usize> {
        let pick = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.state == SlotState::Ready)
            .max_by_key(|(_, s)| s.seq)
            .map(|(i, _)| i);
        if let Some(i) = pick {
            self.slots[i].state = SlotState::PwInFlight;
        }
        pick
    }

    /// PW side: reclaim all slots that were PwInFlight before the current
    /// `keep` slot. Called at the *start* of each on_process (before
    /// pick_for_pw) — by the time PW asks for another buffer, the previous
    /// one is guaranteed released.
    pub fn reclaim_in_flight_except(&mut self, keep: Option<usize>) {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.state == SlotState::PwInFlight && Some(i) != keep {
                slot.state = SlotState::Free;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// True iff the machine has at least one /dev/dri/renderD* node we can open.
    /// Tests that need actual GBM allocation skip themselves when this is false
    /// so the suite still passes in headless CI environments without DRM.
    fn has_render_node() -> bool {
        std::fs::read_dir("/dev/dri")
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
    }

    #[test]
    fn pick_render_node_finds_something() {
        if !has_render_node() {
            eprintln!("skip: no /dev/dri/renderD* on this host");
            return;
        }
        let path = pick_render_node(None).expect("pick_render_node");
        assert!(path.starts_with("/dev/dri/"), "got {:?}", path);
    }

    #[test]
    fn gbm_open_succeeds() {
        if !has_render_node() {
            eprintln!("skip: no DRM");
            return;
        }
        let _ctx = GbmContext::open(None).expect("GbmContext::open");
    }

    #[test]
    fn allocate_linear_argb8888() {
        if !has_render_node() {
            eprintln!("skip: no DRM");
            return;
        }
        let ctx = GbmContext::open(None).expect("GbmContext::open");
        let buf = ctx
            .allocate_linear(256, 256, fourcc::ARGB8888)
            .expect("allocate ARGB8888 256x256");
        assert!(buf.stride >= 256 * 4, "stride={}", buf.stride);
        assert_eq!(buf.modifier, MOD_LINEAR, "modifier should be LINEAR");
        assert_eq!(buf.plane_count(), 1, "single-plane expected");
    }

    #[test]
    fn allocate_linear_xrgb8888() {
        if !has_render_node() {
            eprintln!("skip: no DRM");
            return;
        }
        let ctx = GbmContext::open(None).expect("GbmContext::open");
        let buf = ctx
            .allocate_linear(640, 480, fourcc::XRGB8888)
            .expect("allocate XRGB8888 640x480");
        assert!(buf.stride >= 640 * 4);
        assert_eq!(buf.modifier, MOD_LINEAR);
    }

    #[test]
    fn allocate_rejects_bad_fourcc() {
        if !has_render_node() {
            eprintln!("skip: no DRM");
            return;
        }
        let ctx = GbmContext::open(None).expect("GbmContext::open");
        let err = ctx
            .allocate_linear(64, 64, 0xdeadbeef)
            .err()
            .expect("bad fourcc should error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn pool_state_machine() {
        // Build a pool from synthetic Vec<DmabufBuffer> — we can't construct
        // DmabufBuffer without GBM, so this test only runs when alloc works.
        if !has_render_node() {
            eprintln!("skip: no DRM");
            return;
        }
        let ctx = GbmContext::open(None).expect("GbmContext::open");
        let bufs = (0..3)
            .map(|_| ctx.allocate_linear(64, 64, fourcc::ARGB8888).unwrap())
            .collect::<Vec<_>>();
        let mut pool = DmabufPool::new(bufs);

        // All start Free; capture picks any.
        let a = pool.pick_for_capture().expect("pick a");
        assert_eq!(pool.slots[a].state, SlotState::Filling);
        pool.mark_ready(a);
        assert_eq!(pool.slots[a].state, SlotState::Ready);

        // PW picks the freshest Ready.
        let pw = pool.pick_for_pw().expect("pick pw");
        assert_eq!(pw, a);
        assert_eq!(pool.slots[pw].state, SlotState::PwInFlight);

        // Capture picks again — must be a Free slot since `a` is PwInFlight.
        let b = pool.pick_for_capture().expect("pick b");
        assert_ne!(b, a);

        // Reclaim previous PwInFlight when PW moves to a new slot.
        pool.reclaim_in_flight_except(None);
        assert_eq!(pool.slots[a].state, SlotState::Free);
    }
}
