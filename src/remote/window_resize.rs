//! Fixed-size, explicitly authorized window-resize messages and admission state.
//!
//! No record contains an XID, IPC command, focus operation or input grant. Native
//! configuration acknowledgement alone is never evidence of a captured result.
use super::RemoteResult;
use super::frame::validate_dimensions;
use std::io;
use std::time::{Duration, Instant};

pub const RESIZE_REQUEST_LEN: usize = 28;
pub const RESIZE_RESULT_LEN: usize = 37;
pub const RESIZE_TIMEOUT: Duration = Duration::from_secs(3);
const MIN_NATIVE_RESIZE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResizeRequest {
    pub request_id: u64,
    pub source_epoch: u64,
    pub geometry_epoch: u64,
    pub width: u16,
    pub height: u16,
}

impl ResizeRequest {
    pub fn validate(self) -> RemoteResult<()> {
        validate_identity(self.request_id, self.source_epoch, self.geometry_epoch)?;
        validate_dimensions(self.width, self.height)
    }

    pub fn encode(self) -> RemoteResult<[u8; RESIZE_REQUEST_LEN]> {
        self.validate()?;
        let mut bytes = [0; RESIZE_REQUEST_LEN];
        bytes[..8].copy_from_slice(&self.request_id.to_be_bytes());
        bytes[8..16].copy_from_slice(&self.source_epoch.to_be_bytes());
        bytes[16..24].copy_from_slice(&self.geometry_epoch.to_be_bytes());
        bytes[24..26].copy_from_slice(&self.width.to_be_bytes());
        bytes[26..28].copy_from_slice(&self.height.to_be_bytes());
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> RemoteResult<Self> {
        if bytes.len() != RESIZE_REQUEST_LEN {
            return Err(invalid("invalid window resize request length").into());
        }
        let request = Self {
            request_id: read_u64(&bytes[..8]),
            source_epoch: read_u64(&bytes[8..16]),
            geometry_epoch: read_u64(&bytes[16..24]),
            width: read_u16(&bytes[24..26]),
            height: read_u16(&bytes[26..28]),
        };
        request.validate()?;
        Ok(request)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ResizeStatus {
    Applied = 1,
    Unchanged = 2,
    Denied = 3,
    Stale = 4,
    Busy = 5,
    Failed = 6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResizeResult {
    pub request_id: u64,
    pub source_epoch: u64,
    /// Geometry against which the request was issued.
    pub geometry_epoch: u64,
    pub observed_geometry_epoch: u64,
    pub width: u16,
    pub height: u16,
    pub status: ResizeStatus,
}

impl ResizeResult {
    pub fn validate(self) -> RemoteResult<()> {
        validate_identity(self.request_id, self.source_epoch, self.geometry_epoch)?;
        if self.observed_geometry_epoch == 0
            || (self.status == ResizeStatus::Applied
                && self.observed_geometry_epoch <= self.geometry_epoch)
        {
            return Err(invalid("resize result lacks a valid observed geometry epoch").into());
        }
        validate_dimensions(self.width, self.height)
    }

    pub fn encode(self) -> RemoteResult<[u8; RESIZE_RESULT_LEN]> {
        self.validate()?;
        let mut bytes = [0; RESIZE_RESULT_LEN];
        bytes[..8].copy_from_slice(&self.request_id.to_be_bytes());
        bytes[8..16].copy_from_slice(&self.source_epoch.to_be_bytes());
        bytes[16..24].copy_from_slice(&self.geometry_epoch.to_be_bytes());
        bytes[24..32].copy_from_slice(&self.observed_geometry_epoch.to_be_bytes());
        bytes[32..34].copy_from_slice(&self.width.to_be_bytes());
        bytes[34..36].copy_from_slice(&self.height.to_be_bytes());
        bytes[36] = self.status as u8;
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> RemoteResult<Self> {
        if bytes.len() != RESIZE_RESULT_LEN {
            return Err(invalid("invalid window resize result length").into());
        }
        let status = match bytes[36] {
            1 => ResizeStatus::Applied,
            2 => ResizeStatus::Unchanged,
            3 => ResizeStatus::Denied,
            4 => ResizeStatus::Stale,
            5 => ResizeStatus::Busy,
            6 => ResizeStatus::Failed,
            _ => return Err(invalid("invalid window resize status").into()),
        };
        let result = Self {
            request_id: read_u64(&bytes[..8]),
            source_epoch: read_u64(&bytes[8..16]),
            geometry_epoch: read_u64(&bytes[16..24]),
            observed_geometry_epoch: read_u64(&bytes[24..32]),
            width: read_u16(&bytes[32..34]),
            height: read_u16(&bytes[34..36]),
            status,
        };
        result.validate()?;
        Ok(result)
    }
}

#[derive(Debug)]
struct PendingResize {
    request: ResizeRequest,
    original_size: (u16, u16),
    configured_size: Option<(u16, u16)>,
    deadline: Instant,
}

/// One in-flight request. Failures/timeouts permanently revoke resize on this
/// connection; the enclosing session may continue view-only. No implicit retry.
#[derive(Debug)]
pub(crate) struct HostResizeState {
    enabled: bool,
    highest_request_id: u64,
    next_native_request: Option<Instant>,
    pending: Option<PendingResize>,
}

impl HostResizeState {
    #[must_use]
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            highest_request_id: 0,
            next_native_request: None,
            pending: None,
        }
    }

    #[must_use]
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn pending_request(&self) -> Option<ResizeRequest> {
        self.pending.as_ref().map(|pending| pending.request)
    }

    /// None admits one native IPC call; Some MUST NOT cause a native mutation.
    /// A cached same-size request follows exactly the same native validation.
    pub(crate) fn accept(
        &mut self,
        request: ResizeRequest,
        current_source: u64,
        current_geometry: u64,
        current_size: (u16, u16),
        now: Instant,
    ) -> RemoteResult<Option<ResizeResult>> {
        request.validate()?;
        validate_identity(1, current_source, current_geometry)?;
        validate_dimensions(current_size.0, current_size.1)?;
        let reject = |status| Some(response(request, current_geometry, current_size, status));
        if request.request_id <= self.highest_request_id {
            return Ok(reject(ResizeStatus::Stale));
        }
        self.highest_request_id = request.request_id;
        if !self.enabled {
            return Ok(reject(ResizeStatus::Denied));
        }
        if request.source_epoch != current_source || request.geometry_epoch != current_geometry {
            return Ok(reject(ResizeStatus::Stale));
        }
        if self.pending.is_some() || self.next_native_request.is_some_and(|next| now < next) {
            return Ok(reject(ResizeStatus::Busy));
        }
        let deadline = now
            .checked_add(RESIZE_TIMEOUT)
            .ok_or_else(|| invalid("window resize deadline overflow"))?;
        let next = now
            .checked_add(MIN_NATIVE_RESIZE_INTERVAL)
            .ok_or_else(|| invalid("window resize admission deadline overflow"))?;
        self.next_native_request = Some(next);
        self.pending = Some(PendingResize {
            request,
            original_size: current_size,
            configured_size: None,
            deadline,
        });
        Ok(None)
    }

    /// Record the native adapter's actual (possibly constrained) target only.
    /// Neither Applied nor Unchanged may be inferred from an IPC ack. Caller
    /// must expire() after a blocking native operation, before recording its ack.
    pub(crate) fn configured(
        &mut self,
        actual_size: (u16, u16),
    ) -> RemoteResult<Option<ResizeResult>> {
        validate_dimensions(actual_size.0, actual_size.1)?;
        let pending = self
            .pending
            .as_mut()
            .ok_or_else(|| invalid("window resize has no pending request"))?;
        if pending.configured_size.is_some() {
            return Err(invalid("window resize was already configured").into());
        }
        pending.configured_size = Some(actual_size);
        Ok(None)
    }

    /// Feed only a successful capture taken AFTER native acknowledgement, not
    /// a queued pre-IPC frame, viewport update, or native reply. Applied requires
    /// newer geometry and matching size. Unchanged may retain the base epoch.
    pub(crate) fn observe_frame(
        &mut self,
        source_epoch: u64,
        geometry_epoch: u64,
        size: (u16, u16),
        now: Instant,
    ) -> Option<ResizeResult> {
        if let Some(expired) = self.expire(now) {
            return Some(expired);
        }
        let pending = self.pending.as_ref()?;
        if source_epoch != pending.request.source_epoch {
            return self.fail();
        }
        let configured = pending.configured_size?;
        if size != configured || geometry_epoch < pending.request.geometry_epoch {
            return None;
        }
        let status = if configured == pending.original_size {
            ResizeStatus::Unchanged
        } else if geometry_epoch > pending.request.geometry_epoch {
            ResizeStatus::Applied
        } else {
            return None;
        };
        let request = self.pending.take()?.request;
        Some(response(request, geometry_epoch, size, status))
    }

    pub(crate) fn expire(&mut self, now: Instant) -> Option<ResizeResult> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline)
        {
            self.fail()
        } else {
            None
        }
    }

    pub(crate) fn fail(&mut self) -> Option<ResizeResult> {
        self.enabled = false;
        self.pending.take().map(|pending| {
            response(
                pending.request,
                pending.request.geometry_epoch,
                pending.original_size,
                ResizeStatus::Failed,
            )
        })
    }
}

fn response(
    request: ResizeRequest,
    geometry: u64,
    size: (u16, u16),
    status: ResizeStatus,
) -> ResizeResult {
    ResizeResult {
        request_id: request.request_id,
        source_epoch: request.source_epoch,
        geometry_epoch: request.geometry_epoch,
        observed_geometry_epoch: geometry,
        width: size.0,
        height: size.1,
        status,
    }
}

fn validate_identity(id: u64, source: u64, geometry: u64) -> RemoteResult<()> {
    if id == 0 || source == 0 || geometry == 0 {
        return Err(invalid("window resize identifiers and epochs must be nonzero").into());
    }
    Ok(())
}
fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().expect("checked wire length"))
}
fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes(bytes.try_into().expect("checked wire length"))
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u64) -> ResizeRequest {
        ResizeRequest {
            request_id: id,
            source_epoch: 3,
            geometry_epoch: 7,
            width: 900,
            height: 700,
        }
    }
    fn admit(state: &mut HostResizeState, now: Instant) {
        assert!(
            state
                .accept(request(1), 3, 7, (800, 600), now)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn exact_wire_lengths_and_roundtrips() {
        let request = request(1);
        assert_eq!(
            ResizeRequest::decode(&request.encode().unwrap()).unwrap(),
            request
        );
        for status in [
            ResizeStatus::Applied,
            ResizeStatus::Unchanged,
            ResizeStatus::Denied,
            ResizeStatus::Stale,
            ResizeStatus::Busy,
            ResizeStatus::Failed,
        ] {
            let result = response(request, 8, (900, 700), status);
            assert_eq!(
                ResizeResult::decode(&result.encode().unwrap()).unwrap(),
                result
            );
        }
        for length in [0, RESIZE_REQUEST_LEN - 1, RESIZE_REQUEST_LEN + 1] {
            assert!(ResizeRequest::decode(&vec![0; length]).is_err());
        }
        for length in [0, RESIZE_RESULT_LEN - 1, RESIZE_RESULT_LEN + 1] {
            assert!(ResizeResult::decode(&vec![0; length]).is_err());
        }
    }

    #[test]
    fn invalid_ids_dimensions_status_and_applied_epoch_are_rejected() {
        for mut req in [request(0), request(1), request(2), request(3), request(4)] {
            match req.request_id {
                1 => req.source_epoch = 0,
                2 => req.geometry_epoch = 0,
                3 => req.width = 0,
                4 => req.width = u16::MAX,
                _ => {}
            }
            assert!(req.encode().is_err());
        }
        assert!(
            response(request(1), 7, (900, 700), ResizeStatus::Applied)
                .encode()
                .is_err()
        );
        assert!(
            response(request(1), 0, (900, 700), ResizeStatus::Failed)
                .encode()
                .is_err()
        );
        let mut bytes = response(request(1), 8, (900, 700), ResizeStatus::Applied)
            .encode()
            .unwrap();
        bytes[36] = 255;
        assert!(ResizeResult::decode(&bytes).is_err());
    }

    #[test]
    fn configured_is_not_applied_and_old_or_wrong_frames_do_not_complete() {
        let now = Instant::now();
        let mut state = HostResizeState::new(true);
        admit(&mut state, now);
        assert!(state.observe_frame(3, 8, (900, 700), now).is_none());
        assert!(state.configured((900, 700)).unwrap().is_none());
        assert!(state.observe_frame(3, 7, (900, 700), now).is_none());
        assert!(state.observe_frame(3, 8, (800, 600), now).is_none());
        assert_eq!(
            state.observe_frame(3, 8, (900, 700), now).unwrap().status,
            ResizeStatus::Applied
        );
        assert!(state.pending_request().is_none());
    }

    #[test]
    fn permission_epochs_and_monotonic_ids_fence_admission() {
        let now = Instant::now();
        let mut denied = HostResizeState::new(false);
        assert_eq!(
            denied
                .accept(request(1), 3, 7, (800, 600), now)
                .unwrap()
                .unwrap()
                .status,
            ResizeStatus::Denied
        );
        let mut state = HostResizeState::new(true);
        assert_eq!(
            state
                .accept(request(1), 4, 7, (800, 600), now)
                .unwrap()
                .unwrap()
                .status,
            ResizeStatus::Stale
        );
        assert_eq!(
            state
                .accept(request(2), 3, 8, (800, 600), now)
                .unwrap()
                .unwrap()
                .status,
            ResizeStatus::Stale
        );
        assert_eq!(
            state
                .accept(request(2), 3, 7, (800, 600), now)
                .unwrap()
                .unwrap()
                .status,
            ResizeStatus::Stale
        );
        assert!(state.pending_request().is_none());
    }

    #[test]
    fn one_inflight_and_replay_of_busy_request_are_rejected() {
        let now = Instant::now();
        let mut state = HostResizeState::new(true);
        admit(&mut state, now);
        assert_eq!(
            state
                .accept(request(2), 3, 7, (800, 600), now)
                .unwrap()
                .unwrap()
                .status,
            ResizeStatus::Busy
        );
        assert_eq!(state.pending_request(), Some(request(1)));
        assert!(state.configured((900, 700)).unwrap().is_none());
        state.observe_frame(3, 8, (900, 700), now).unwrap();
        assert_eq!(
            state
                .accept(request(2), 3, 8, (900, 700), now)
                .unwrap()
                .unwrap()
                .status,
            ResizeStatus::Stale
        );
    }

    #[test]
    fn same_size_and_constrained_unchanged_require_native_ack_and_fresh_capture() {
        let now = Instant::now();
        for desired in [(800, 600), (900, 700)] {
            let mut state = HostResizeState::new(true);
            let mut req = request(1);
            (req.width, req.height) = desired;
            assert!(state.accept(req, 3, 7, (800, 600), now).unwrap().is_none());
            assert!(state.observe_frame(3, 7, (800, 600), now).is_none());
            assert!(state.configured((800, 600)).unwrap().is_none());
            assert_eq!(state.pending_request(), Some(req));
            assert!(state.observe_frame(3, 6, (800, 600), now).is_none());
            assert!(state.observe_frame(3, 7, (801, 600), now).is_none());
            assert_eq!(
                state.observe_frame(3, 7, (800, 600), now).unwrap().status,
                ResizeStatus::Unchanged
            );
            assert!(state.pending_request().is_none());
        }
    }

    #[test]
    fn distinct_id_flood_cannot_exceed_four_native_admissions_per_second() {
        let now = Instant::now();
        let mut state = HostResizeState::new(true);
        let mut admitted = Vec::new();
        for millisecond in 0..1000_u64 {
            let mut req = request(millisecond + 1);
            req.width = 800;
            req.height = 600;
            let at = now + Duration::from_millis(millisecond);
            match state.accept(req, 3, 7, (800, 600), at).unwrap() {
                None => {
                    admitted.push(millisecond);
                    assert!(state.configured((800, 600)).unwrap().is_none());
                    assert_eq!(
                        state.observe_frame(3, 7, (800, 600), at).unwrap().status,
                        ResizeStatus::Unchanged
                    );
                }
                Some(result) => {
                    assert_eq!(result.status, ResizeStatus::Busy);
                    assert!(state.pending_request().is_none());
                }
            }
        }
        assert_eq!(admitted, [0, 250, 500, 750]);
        assert_eq!(
            state
                .accept(
                    request(1000),
                    3,
                    7,
                    (800, 600),
                    now + Duration::from_secs(1)
                )
                .unwrap()
                .unwrap()
                .status,
            ResizeStatus::Stale
        );
        assert!(
            state
                .accept(
                    request(1001),
                    3,
                    7,
                    (800, 600),
                    now + Duration::from_secs(1)
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn timeout_and_source_change_disable_resize_permanently() {
        let now = Instant::now();
        let mut state = HostResizeState::new(true);
        admit(&mut state, now);
        assert!(
            state
                .expire(now + RESIZE_TIMEOUT - Duration::from_nanos(1))
                .is_none()
        );
        assert_eq!(
            state.expire(now + RESIZE_TIMEOUT).unwrap().status,
            ResizeStatus::Failed
        );
        assert!(!state.enabled());
        assert_eq!(
            state
                .accept(request(2), 3, 7, (800, 600), now)
                .unwrap()
                .unwrap()
                .status,
            ResizeStatus::Denied
        );
        let mut state = HostResizeState::new(true);
        admit(&mut state, now);
        assert_eq!(
            state.observe_frame(4, 8, (900, 700), now).unwrap().status,
            ResizeStatus::Failed
        );
        assert!(!state.enabled());
    }
}
