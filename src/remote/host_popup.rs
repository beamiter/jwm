//! Dedicated authenticated popup-pointer session. No legacy input/credit state.
use super::*;
use crate::remote::source_wire::{FramePurpose, MAX_FRAME_INNER_PAYLOAD_LEN, encode_frame_payload};
use crate::remote::tile::{TileEncodeRequest, TileEncoder};
use crate::remote::wayland_capture::WaylandCapture;
use crate::remote::wayland_popup_wire::{
    AckDisposition, Binding, Click, ClickResolution, FrameKey, Ledger, METADATA_LEN, Metadata,
    Opportunity, Region, Status, StatusKind,
};
use std::sync::mpsc::{self, TryRecvError};

// SessionReader caps sealed plaintext (including padding), not the decoded
// application payload. The largest allowed request is the 80-byte Click,
// which occupies the protocol's 256-byte bucket. Per-kind decoders below
// still enforce their exact unpadded lengths.
const POPUP_REQUEST_RECORD_LIMIT: usize = 256;

#[cfg(all(test, feature = "backend-wayland-udev"))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct PopupHostLifetimeSnapshot {
    pub created: Instant,
    pub deadline: Instant,
    pub binding: Binding,
    pub current_captured_at: Option<Instant>,
    pub current_sequence: Option<u64>,
    pub current_acked: bool,
    pub exited_at: Option<Instant>,
}

#[cfg(all(test, feature = "backend-wayland-udev"))]
thread_local! {
    static LIFETIME_OBSERVER: std::cell::RefCell<Option<Arc<Mutex<Option<PopupHostLifetimeSnapshot>>>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) fn with_popup_lifetime_observer<T>(
    observer: Arc<Mutex<Option<PopupHostLifetimeSnapshot>>>,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Arc<Mutex<Option<PopupHostLifetimeSnapshot>>>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LIFETIME_OBSERVER.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(LIFETIME_OBSERVER.with(|slot| slot.replace(Some(observer))));
    run()
}

#[cfg(all(test, feature = "backend-wayland-udev"))]
fn observe_lifetime(update: impl FnOnce(&mut Option<PopupHostLifetimeSnapshot>)) {
    LIFETIME_OBSERVER.with(|slot| {
        if let Some(observer) = slot.borrow().as_ref() {
            update(&mut observer.lock().unwrap());
        }
    });
}

#[cfg(all(test, feature = "backend-wayland-udev"))]
fn observe_current(current: Option<(u64, Instant)>, acked: bool) {
    observe_lifetime(|snapshot| {
        if let Some(snapshot) = snapshot {
            snapshot.current_captured_at = current.map(|(_, at)| at);
            snapshot.current_sequence = current.map(|(sequence, _)| sequence);
            snapshot.current_acked = acked;
        }
    });
}

#[cfg(all(test, feature = "backend-wayland-udev"))]
#[derive(Clone, Copy, Debug)]
pub(crate) enum PopupHostTestEvent {
    NativeCleanupConfirmed {
        binding: Binding,
        at: Instant,
        deadline: Instant,
    },
    AckWritten {
        binding: Binding,
        at: Instant,
    },
    ReadyPublished(FrameKey),
    BeforeIdleRefresh(FrameKey),
    DiscardedAck {
        sequence: u64,
        at: Instant,
    },
    ClickQueued {
        key: FrameKey,
        sequence: u32,
    },
    ClickDequeued {
        key: FrameKey,
        sequence: u32,
        at: Instant,
    },
    FrameCaptured {
        key: FrameKey,
        at: Instant,
    },
    RejectedStale {
        key: FrameKey,
        sequence: u32,
        at: Instant,
    },
    Accepted {
        key: FrameKey,
        sequence: u32,
        at: Instant,
    },
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
type ScheduleObserver = Arc<dyn Fn(PopupHostTestEvent) + Send + Sync>;
#[cfg(all(test, feature = "backend-wayland-udev"))]
thread_local! {
    static SCHEDULE_OBSERVER: std::cell::RefCell<Option<ScheduleObserver>> = const { std::cell::RefCell::new(None) };
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) fn with_popup_schedule_observer<T>(
    observer: ScheduleObserver,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<ScheduleObserver>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCHEDULE_OBSERVER.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(SCHEDULE_OBSERVER.with(|slot| slot.replace(Some(observer))));
    run()
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
fn observe_schedule(event: PopupHostTestEvent) {
    let observer = SCHEDULE_OBSERVER.with(|slot| slot.borrow().clone());
    if let Some(observer) = observer {
        observer(event);
    }
}

pub(super) fn serve(
    mut reader: SessionReader<TcpStream>,
    mut writer: SessionWriter<DeadlineWriter<TcpStream>>,
    hello: ClientHello,
    options: &HostOptions,
    shutdown: &Arc<AtomicBool>,
    mut capture: Box<WaylandCapture>,
) -> RemoteResult<()> {
    let socket = reader.get_ref().try_clone()?;
    let broken = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::sync_channel(32);
    reader.set_max_payload_len(POPUP_REQUEST_RECORD_LIMIT);
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(31)))?;
    let failed = broken.clone();
    let reader_socket = socket.try_clone()?;
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    let schedule_observer = SCHEDULE_OBSERVER.with(|slot| slot.borrow().clone());
    let receiver = thread::spawn(move || {
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        SCHEDULE_OBSERVER.with(|slot| *slot.borrow_mut() = schedule_observer);
        if !receive_popup_requests(&mut reader, &tx) {
            failed.store(true, Ordering::Release);
            let _ = reader_socket.shutdown(Shutdown::Both);
        }
    });
    let result = (|| -> RemoteResult<()> {
        let mut stream = hello.stream;
        stream.frame_limits = stream.frame_limits.intersect(options.frame_limits);
        stream.frame_limits.validate()?;
        capture.set_frame_limits(stream.frame_limits);
        capture.start_popup_pointer()?;
        let (epoch, created, deadline) = capture.popup_lease_info()?;
        let binding = Binding {
            source_instance: 1,
            source_epoch: 1,
            lease_epoch: epoch,
        };
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        observe_lifetime(|snapshot| {
            *snapshot = Some(PopupHostLifetimeSnapshot {
                created,
                deadline,
                binding,
                current_captured_at: None,
                current_sequence: None,
                current_acked: false,
                exited_at: None,
            });
        });
        stream.cursor = stream.cursor.negotiate(false);
        let header = ServerHello {
            stream,
            pointer_enabled: false,
            keyboard_enabled: false,
            clipboard_enabled: false,
            clipboard_files_enabled: false,
        };
        write_session_record(
            &mut writer,
            MessageKind::HelloAck,
            &header.encode(),
            remaining(deadline)?,
        )?;
        let catalog = SourceCatalog {
            current: SourceVersion {
                source: SourceId(1),
                epoch: 1,
            },
            entries: vec![SourceEntry {
                id: SourceId(1),
                label: "Authorized popup pointer surface".into(),
            }],
        };
        write_session_record(
            &mut writer,
            MessageKind::SourceCatalog,
            &catalog.encode()?,
            remaining(deadline)?,
        )?;
        let mut codec = TileEncoder::new();
        let mut ledger: Option<Ledger> = None;
        let mut current: Option<(Metadata, Instant)> = None;
        let mut wire_sequence = 0_u64;
        let mut proof_id = 0_u64;
        let mut action_sequence = 0_u32;
        let mut capture_after = None;
        let mut announced = false;
        let interval = Duration::from_secs_f64(1.0 / f64::from(options.fps.max(1)));
        let mut next_capture = Instant::now();
        loop {
            if shutdown.load(Ordering::Acquire) || broken.load(Ordering::Acquire) {
                return Err(invalid_data("popup transport closed").into());
            }
            remaining(deadline)?;
            capture.popup_lease_info()?;
            // Poll the same freshness/lease authority even when no ACK arrives.
            // Merely observing readiness never mints or consumes a permit.
            if let Some(authority) = ledger.as_mut() {
                let _ = authority.ready(Instant::now())?;
            }
            if current.is_none() {
                if Instant::now() < next_capture {
                    capture.wait_for_activity_with_wake(
                        Duration::from_millis(10)
                            .min(next_capture.saturating_duration_since(Instant::now()))
                            .min(remaining(deadline)?),
                        None,
                    )?;
                    continue;
                }
                next_capture = Instant::now() + interval;
                let exported = capture.capture_interactive_popup_frame()?;
                let captured_at = capture.popup_capture_started_at()?;
                if capture_after.is_some_and(|floor| captured_at <= floor) {
                    return Err(invalid_data("popup capture predates rejected command").into());
                }
                let frame = exported.frame;
                let proof = exported.proof;
                stream
                    .frame_limits
                    .validate_dimensions(frame.source_width, frame.source_height)?;
                if proof.epoch != epoch || frame.image.dimensions() != proof.root_size {
                    return Err(invalid_data("popup capture identity changed").into());
                }
                proof_id = proof_id
                    .checked_add(1)
                    .ok_or_else(|| invalid_data("popup proof overflow"))?;
                let meta = Metadata {
                    key: FrameKey {
                        binding,
                        geometry_epoch: frame.geometry_epoch,
                        wire_sequence,
                        proof_id,
                        native_frame_id: proof.capture_sequence,
                        native_ready_serial: proof.native_ready_serial,
                        family_generation: proof.generation,
                    },
                    canvas: proof.root_size,
                    popup: proof.popup_rect.map(|(x, y, width, height)| Region {
                        x,
                        y,
                        width,
                        height,
                    }),
                };
                #[cfg(all(test, feature = "backend-wayland-udev"))]
                observe_schedule(PopupHostTestEvent::FrameCaptured {
                    key: meta.key,
                    at: captured_at,
                });
                if ledger.is_none() {
                    ledger = Some(Ledger::new(binding, meta.canvas, created)?);
                }
                let authority = ledger.as_mut().unwrap();
                authority.reserve(meta, captured_at, Instant::now())?;
                codec.plan(&frame, TileEncodeRequest::Keyframe)?;
                let mut tile = Vec::new();
                codec.encode_into(&mut tile, wire_sequence, &frame, options.jpeg_quality)?;
                if tile.len() > MAX_FRAME_INNER_PAYLOAD_LEN - METADATA_LEN {
                    codec.discard();
                    return Err(invalid_data("popup frame exceeds bounded envelope").into());
                }
                let mut inner = meta.encode()?.to_vec();
                inner.extend_from_slice(&tile);
                let payload = encode_frame_payload(1, FramePurpose::PopupFamilyProof, &inner)?;
                if let Err(error) = write_frame_record(&mut writer, &payload, remaining(deadline)?)
                {
                    codec.discard();
                    let _ = authority.write_failed();
                    return Err(error.into());
                }
                authority.commit_write(meta.key, Instant::now())?;
                codec.commit(&frame);
                current = Some((meta, captured_at));
                #[cfg(all(test, feature = "backend-wayland-udev"))]
                observe_current(Some((meta.key.wire_sequence, captured_at)), false);
                wire_sequence = wire_sequence
                    .checked_add(1)
                    .ok_or_else(|| invalid_data("popup sequence overflow"))?;
                announced = false;
                status(
                    &mut writer,
                    StatusKind::Waiting,
                    meta,
                    action_sequence,
                    deadline,
                )?;
            }
            for _ in 0..32 {
                let (kind, bytes) = match rx.try_recv() {
                    Ok(value) => value,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        return Err(invalid_data("popup reader ended").into());
                    }
                };
                let dequeued_at = Instant::now();
                check_popup_transport(&broken)?;
                let authority = ledger.as_mut().unwrap();
                let meta = current
                    .ok_or_else(|| invalid_data("popup message without current proof"))?
                    .0;
                match kind {
                    MessageKind::FrameAck => {
                        let (source, sequence) = decode_source_frame_ack(&bytes)?;
                        if source != binding.source_epoch {
                            return Err(invalid_data("popup ACK source mismatch").into());
                        }
                        if authority.acknowledge_frame(binding, sequence, Instant::now())?
                            == AckDisposition::Discarded
                        {
                            #[cfg(all(test, feature = "backend-wayland-udev"))]
                            observe_schedule(PopupHostTestEvent::DiscardedAck {
                                sequence,
                                at: Instant::now(),
                            });
                            continue;
                        }
                        #[cfg(all(test, feature = "backend-wayland-udev"))]
                        observe_current(
                            current.map(|(meta, at)| (meta.key.wire_sequence, at)),
                            true,
                        );
                        match authority.ready(Instant::now())? {
                            Some(Opportunity::Arm(key)) => {
                                let permit = authority.authorize_arm(key, Instant::now())?;
                                check_popup_transport(&broken)?;
                                capture.dispatch_host_popup(permit)?;
                                authority.complete_arm(binding, Instant::now())?;
                                status(&mut writer, StatusKind::Armed, meta, 0, deadline)?;
                                current = None;
                                #[cfg(all(test, feature = "backend-wayland-udev"))]
                                observe_current(None, false);
                            }
                            Some(Opportunity::Click(_)) => {
                                status(
                                    &mut writer,
                                    StatusKind::Ready,
                                    meta,
                                    action_sequence,
                                    deadline,
                                )?;
                                announced = true;
                                #[cfg(all(test, feature = "backend-wayland-udev"))]
                                observe_schedule(PopupHostTestEvent::ReadyPublished(meta.key));
                            }
                            None => {
                                return Err(invalid_data("popup ACK lacks committed proof").into());
                            }
                        }
                    }
                    MessageKind::WaylandPopupClick => {
                        let request = Click::decode(&bytes)?;
                        #[cfg(all(test, feature = "backend-wayland-udev"))]
                        observe_schedule(PopupHostTestEvent::ClickDequeued {
                            key: request.key,
                            sequence: request.sequence,
                            at: dequeued_at,
                        });
                        match authority.resolve_click(request, dequeued_at)? {
                            ClickResolution::Dispatch(permit) => {
                                check_popup_transport(&broken)?;
                                capture.dispatch_host_popup(permit)?;
                                authority.complete_click(
                                    binding,
                                    request.sequence,
                                    Instant::now(),
                                )?;
                                status(
                                    &mut writer,
                                    StatusKind::Accepted,
                                    meta,
                                    request.sequence,
                                    deadline,
                                )?;
                                #[cfg(all(test, feature = "backend-wayland-udev"))]
                                observe_schedule(PopupHostTestEvent::Accepted {
                                    key: request.key,
                                    sequence: request.sequence,
                                    at: Instant::now(),
                                });
                            }
                            ClickResolution::RejectedStale { metadata, sequence } => {
                                check_popup_transport(&broken)?;
                                capture.discard_popup_frame();
                                capture_after = Some(dequeued_at);
                                // The rejection precedes every replacement frame on this writer.
                                status(
                                    &mut writer,
                                    StatusKind::RejectedStale,
                                    metadata,
                                    sequence,
                                    deadline,
                                )?;
                                #[cfg(all(test, feature = "backend-wayland-udev"))]
                                observe_schedule(PopupHostTestEvent::RejectedStale {
                                    key: metadata.key,
                                    sequence,
                                    at: Instant::now(),
                                });
                            }
                        }
                        action_sequence = request.sequence;
                        current = None;
                        #[cfg(all(test, feature = "backend-wayland-udev"))]
                        observe_current(None, false);
                        announced = false;
                    }
                    MessageKind::WaylandPopupRevoke => {
                        validate_popup_revoke(&bytes, binding)?;
                        let close_deadline = deadline.min(Instant::now() + Duration::from_secs(2));
                        capture.finish_interactive_popup_confirmed(close_deadline)?;
                        #[cfg(all(test, feature = "backend-wayland-udev"))]
                        observe_schedule(PopupHostTestEvent::NativeCleanupConfirmed {
                            binding,
                            at: Instant::now(),
                            deadline: close_deadline,
                        });
                        write_session_record(
                            &mut writer,
                            MessageKind::WaylandPopupRevokeAck,
                            &binding.encode()?,
                            remaining(close_deadline)?,
                        )?;
                        #[cfg(all(test, feature = "backend-wayland-udev"))]
                        observe_schedule(PopupHostTestEvent::AckWritten {
                            binding,
                            at: Instant::now(),
                        });
                        return Ok(());
                    }
                    MessageKind::Heartbeat => {
                        accept_popup_heartbeat(&bytes)?;
                    }
                    _ => return Err(invalid_data("unexpected popup session message").into()),
                }
                if current.is_none() {
                    break;
                }
            }
            if let Some((meta, captured_at)) = current {
                if announced && captured_at.elapsed() >= Duration::from_millis(500) {
                    #[cfg(all(test, feature = "backend-wayland-udev"))]
                    observe_schedule(PopupHostTestEvent::BeforeIdleRefresh(meta.key));
                    status(
                        &mut writer,
                        StatusKind::Waiting,
                        meta,
                        action_sequence,
                        deadline,
                    )?;
                    ledger
                        .as_mut()
                        .unwrap()
                        .refresh_presented(meta.key, Instant::now())?;
                    capture.discard_popup_frame();
                    current = None;
                    #[cfg(all(test, feature = "backend-wayland-udev"))]
                    observe_current(None, false);
                    announced = false;
                }
            }
            capture.wait_for_activity_with_wake(
                Duration::from_millis(10).min(remaining(deadline)?),
                None,
            )?;
        }
    })();
    #[cfg(all(test, feature = "backend-wayland-udev"))]
    observe_lifetime(|snapshot| {
        if let Some(snapshot) = snapshot {
            snapshot.exited_at = Some(Instant::now());
        }
    });
    // Revoke native authority on every exit, before joining the network reader.
    capture.finish_interactive_popup();
    if result.is_err() && !broken.load(Ordering::Acquire) {
        // Fixed, bounded diagnostic only; no native identity or error text.
        // Cleanup already revoked authority. This does not extend its lease.
        let close = SessionClose::new(
            SessionCloseCode::CaptureUnavailable,
            "Popup pointer session ended or its presentation proof expired",
        );
        let _ = write_session_record(
            &mut writer,
            MessageKind::SessionClose,
            &close.encode(),
            Duration::from_millis(100),
        );
    }
    let _ = socket.shutdown(Shutdown::Both);
    let joined = receiver.join();
    if joined.is_err() && result.is_ok() {
        return Err(invalid_data("popup reader panicked").into());
    }
    result
}
// Re-check after dequeue and immediately before native dispatch. This rejects
// an already observed read/authentication failure, not a future concurrent EOF.
fn check_popup_transport(broken: &AtomicBool) -> RemoteResult<()> {
    if broken.load(Ordering::Acquire) {
        return Err(invalid_data("popup transport closed").into());
    }
    Ok(())
}

// Stop only after an authenticated terminal record is successfully queued.
// Its binding is still checked by the main loop. Reading beyond it could turn
// an orderly peer EOF into `broken` before that queued revoke was processed.
fn receive_popup_requests<R: io::Read>(
    reader: &mut SessionReader<R>,
    tx: &mpsc::SyncSender<(MessageKind, Vec<u8>)>,
) -> bool {
    loop {
        let message = match reader.read_message() {
            Ok(message) => message,
            Err(_) => return false,
        };
        let terminal = message.0 == MessageKind::WaylandPopupRevoke;
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        let queued_click = (message.0 == MessageKind::WaylandPopupClick)
            .then(|| Click::decode(&message.1).ok())
            .flatten();
        if tx.try_send(message).is_err() {
            return false;
        }
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        if let Some(click) = queued_click {
            observe_schedule(PopupHostTestEvent::ClickQueued {
                key: click.key,
                sequence: click.sequence,
            });
        }
        if terminal {
            return true;
        }
    }
}

fn validate_popup_revoke(bytes: &[u8], binding: Binding) -> RemoteResult<()> {
    if Binding::decode(bytes)? != binding {
        return Err(invalid_data("popup revoke binding mismatch").into());
    }
    Ok(())
}

// Heartbeats are authenticated client-to-host activity only, as in the
// ordinary host reader. There is deliberately no writer or authority argument:
// accepting one neither echoes a record nor extends proof/lease deadlines.
fn accept_popup_heartbeat(bytes: &[u8]) -> RemoteResult<()> {
    if !bytes.is_empty() {
        return Err(invalid_data("invalid popup heartbeat").into());
    }
    Ok(())
}

fn remaining(deadline: Instant) -> RemoteResult<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| invalid_data("popup original lease expired").into())
}
fn status(
    writer: &mut SessionWriter<DeadlineWriter<TcpStream>>,
    kind: StatusKind,
    metadata: Metadata,
    action_sequence: u32,
    deadline: Instant,
) -> RemoteResult<()> {
    let message = Status {
        kind,
        metadata,
        action_sequence,
        ttl_ms: u32::try_from(remaining(deadline)?.as_millis())
            .unwrap_or(u32::MAX)
            .min(30_000),
    };
    write_session_record(
        writer,
        MessageKind::WaylandPopupStatus,
        &message.encode()?,
        remaining(deadline)?,
    )?;
    Ok(())
}

#[cfg(test)]
mod record_budget_tests {
    use super::*;
    use crate::remote::protocol::ProtocolError;
    use std::io::Cursor;

    fn click() -> Click {
        Click {
            key: FrameKey {
                binding: Binding {
                    source_instance: 1,
                    source_epoch: 1,
                    lease_epoch: 1,
                },
                geometry_epoch: 1,
                wire_sequence: 0,
                proof_id: 1,
                native_frame_id: 1,
                native_ready_serial: 1,
                family_generation: 1,
            },
            sequence: 1,
            x: 10,
            y: 10,
        }
    }
    fn record(payload: &[u8]) -> Vec<u8> {
        let mut writer = SessionWriter::new(Vec::new(), [0x42; 32]);
        writer
            .write_message(MessageKind::WaylandPopupClick, payload)
            .unwrap();
        writer.flush().unwrap();
        writer.into_inner()
    }

    #[test]
    fn popup_observed_transport_failure_blocks_queued_dispatch() {
        let broken = AtomicBool::new(false);
        check_popup_transport(&broken).unwrap(); // outer loop was healthy
        broken.store(true, Ordering::Release); // reader fails after that check
        let mut dispatched = false;
        let result = (|| -> RemoteResult<()> {
            check_popup_transport(&broken)?; // production post-dequeue gate
            dispatched = true;
            Ok(())
        })();
        assert_eq!(result.unwrap_err().to_string(), "popup transport closed");
        assert!(!dispatched);
        // Once observed, consuming a different queued record cannot clear it.
        assert!(check_popup_transport(&broken).is_err());
    }

    #[test]
    fn popup_authenticated_revoke_stops_before_eof_but_checks_binding() {
        let binding = click().key.binding;
        for wrong in [false, true] {
            let mut sent = binding;
            if wrong {
                sent.source_instance += 1;
            }
            let mut writer = SessionWriter::new(Vec::new(), [0x42; 32]);
            writer
                .write_message(MessageKind::WaylandPopupRevoke, &sent.encode().unwrap())
                .unwrap();
            let bytes = writer.into_inner();
            let length = bytes.len();
            let mut reader = SessionReader::new(Cursor::new(bytes), [0x42; 32]);
            reader.set_max_payload_len(POPUP_REQUEST_RECORD_LIMIT);
            let (tx, rx) = mpsc::sync_channel(32);
            assert!(receive_popup_requests(&mut reader, &tx));
            assert_eq!(reader.into_inner().position(), length as u64);
            let (kind, payload) = rx.try_recv().unwrap();
            assert_eq!(kind, MessageKind::WaylandPopupRevoke);
            assert_eq!(validate_popup_revoke(&payload, binding).is_err(), wrong);
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn popup_unverified_revoke_and_full_queue_remain_fail_closed() {
        let mut writer = SessionWriter::new(Vec::new(), [0x42; 32]);
        writer
            .write_message(
                MessageKind::WaylandPopupRevoke,
                &click().key.binding.encode().unwrap(),
            )
            .unwrap();
        let bytes = writer.into_inner();
        let mut truncated = bytes.clone();
        truncated.pop(); // no complete authentication tag
        let (tx, rx) = mpsc::sync_channel(32);
        assert!(!receive_popup_requests(
            &mut SessionReader::new(Cursor::new(truncated), [0x42; 32]),
            &tx
        ));
        assert!(rx.try_recv().is_err());
        let (tx, _rx) = mpsc::sync_channel(0);
        assert!(!receive_popup_requests(
            &mut SessionReader::new(Cursor::new(bytes), [0x42; 32]),
            &tx
        ));
        assert!(validate_popup_revoke(&[0; 19], click().key.binding).is_err());
    }

    #[test]
    fn popup_authenticated_heartbeat_is_one_way_and_payload_strict() {
        let key = [0x42; 32];
        let mut writer = SessionWriter::new(Vec::new(), key);
        writer.write_message(MessageKind::Heartbeat, &[]).unwrap();
        writer.write_message(MessageKind::Heartbeat, &[0]).unwrap();
        let mut reader = SessionReader::new(Cursor::new(writer.into_inner()), key);
        reader.set_max_payload_len(POPUP_REQUEST_RECORD_LIMIT);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::Heartbeat);
        accept_popup_heartbeat(&payload).unwrap();
        // Use the same production handler: it has no output channel and no
        // mutable ledger/clock access. The next record is still authenticated.
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::Heartbeat);
        let error = accept_popup_heartbeat(&payload).unwrap_err();
        assert_eq!(error.to_string(), "invalid popup heartbeat");
    }

    #[test]
    fn popup_click_real_record_reproduces_old_cap_and_round_trips() {
        let expected = click();
        let bytes = record(&expected.encode().unwrap());
        let mut old = SessionReader::new(Cursor::new(bytes.clone()), [0x42; 32]);
        old.set_max_payload_len(102);
        assert!(matches!(
            old.read_message(),
            Err(ProtocolError::PayloadTooLarge { len: 256, max: 102 })
        ));
        let mut reader = SessionReader::new(Cursor::new(bytes), [0x42; 32]);
        reader.set_max_payload_len(POPUP_REQUEST_RECORD_LIMIT);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::WaylandPopupClick);
        assert_eq!(payload.len(), 80);
        assert_eq!(Click::decode(&payload).unwrap(), expected);
    }

    #[test]
    fn popup_record_budget_does_not_widen_click_plaintext() {
        let mut payload = click().encode().unwrap().to_vec();
        payload.push(0);
        let mut reader = SessionReader::new(Cursor::new(record(&payload)), [0x42; 32]);
        reader.set_max_payload_len(POPUP_REQUEST_RECORD_LIMIT);
        let (_, decoded) = reader.read_message().unwrap();
        assert!(Click::decode(&decoded).is_err());
    }

    #[test]
    fn popup_oversized_sealed_record_rejected_before_body_read() {
        let mut bytes = record(&[0; 256]); // marker requires the 704-byte bucket
        bytes.truncate(13); // real authenticated-record header, no body/tag
        let mut reader = SessionReader::new(Cursor::new(bytes), [0x42; 32]);
        reader.set_max_payload_len(POPUP_REQUEST_RECORD_LIMIT);
        assert!(matches!(
            reader.read_message(),
            Err(ProtocolError::PayloadTooLarge { len: 704, max: 256 })
        ));
        assert_eq!(reader.into_inner().position(), 13);
    }
}
