//! Test the production SHM producer against a real consumer on a private graph.
use super::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

const WIDTH: u32 = 32;
const HEIGHT: u32 = 16;
const STRIDE: u32 = WIDTH * 4;
const SIZE: u32 = STRIDE * HEIGHT;
const WAIT: Duration = Duration::from_secs(5);
const CLOCK_GROUP: &str = "jwm-ci-shm-clock";

fn pixels(seed: u8) -> Vec<u8> {
    (0..HEIGHT)
        .flat_map(|y| {
            (0..WIDTH).flat_map(move |x| {
                let (b, g, r) = (seed.wrapping_add(x as u8), y as u8, (x ^ y) as u8);
                if cfg!(target_endian = "little") {
                    [b, g, r, 255]
                } else {
                    [255, r, g, b]
                }
            })
        })
        .collect()
}

#[derive(Default)]
struct Observation {
    format: Option<spa::param::video::VideoInfoRaw>,
    process: usize,
    no_buffer: usize,
    no_data: usize,
    empty: usize,
    last_chunk: Option<(u32, u32, i32)>,
    link: Option<String>,
    a: usize,
    b: usize,
    error: Option<String>,
}

fn consumer_buffers() -> OwnedPod {
    use spa::pod::{Property, Value, serialize::PodSerializer};

    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(libspa_sys::SPA_PARAM_BUFFERS_buffers, Value::Int(4)),
            Property::new(libspa_sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(libspa_sys::SPA_PARAM_BUFFERS_size, Value::Int(SIZE as i32)),
            Property::new(
                libspa_sys::SPA_PARAM_BUFFERS_stride,
                Value::Int(STRIDE as i32),
            ),
        ],
    };
    OwnedPod(
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
            .expect("consumer Buffers")
            .0
            .into_inner(),
    )
}

fn poll_until(loop_: &pw::loop_::Loop, mut ready: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while !ready() {
        if started.elapsed() >= WAIT {
            return false;
        }
        loop_.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(20)));
    }
    true
}

#[test]
#[ignore = "requires scripts/test-portal-pipewire.sh's private PipeWire daemon"]
fn private_daemon_shm_delivers_two_frames_and_retires_producer() {
    assert_eq!(
        std::env::var("JWM_PORTAL_TEST_PIPEWIRE").as_deref(),
        Ok("1")
    );
    let runtime = std::env::var_os("PIPEWIRE_RUNTIME_DIR").expect("private daemon runtime");
    let runtime = std::path::Path::new(&runtime);
    assert!(runtime.is_absolute());
    assert_eq!(
        std::env::var("PIPEWIRE_REMOTE").as_deref(),
        Ok("pipewire-0")
    );
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    assert_eq!(
        runtime.metadata().unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(
        runtime
            .join("pipewire-0")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_socket()
    );

    let first = pixels(0x11);
    let second = pixels(0x91);
    assert_ne!(first, second);
    let slot = Arc::new(Mutex::new(crate::capture::FrameSlot {
        width: WIDTH,
        height: HEIGHT,
        stride: STRIDE,
        has_alpha: false,
        data: first.clone(),
        seq: 1,
    }));
    let spec = StreamSpec {
        width: WIDTH,
        height: HEIGHT,
        framerate_num: 30,
        framerate_den: 1,
    };
    let source = Source::Shm(slot.clone());
    let mut format = build_connect_params(&spec, &source).expect("consumer EnumFormat");
    let producer = spawn(spec, "jwm-ci-shm-producer".to_owned(), source).expect("producer node");
    let producer_id = producer.node_id;

    let loop_ = pw::main_loop::MainLoopRc::new(None).expect("consumer main loop");
    let context = pw::context::ContextRc::new(&loop_, None).expect("consumer context");
    let core = context
        .connect_rc(None)
        .expect("private consumer connection");
    let registry = core.get_registry().expect("private registry");
    let seen = Rc::new(Cell::new(false));
    let removed = Rc::new(Cell::new(false));
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let seen = seen.clone();
            move |object| {
                if object.id == producer_id && object.type_ == pw::types::ObjectType::Node {
                    seen.set(true);
                }
            }
        })
        .global_remove({
            let removed = removed.clone();
            move |id| {
                if id == producer_id {
                    removed.set(true);
                }
            }
        })
        .register();
    let observed = Rc::new(RefCell::new(Observation::default()));
    let _core_listener = core
        .add_listener_local()
        .error({
            let observed = observed.clone();
            move |id, _, result, message| {
                observed.borrow_mut().error = Some(format!("core {id} {result}: {message}"));
            }
        })
        .register();
    let consumer = pw::stream::StreamBox::new(
        &core,
        "jwm-ci-shm-consumer",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_CLASS => "Stream/Input/Video",
            // The real producer advertises DRIVER. Without group membership,
            // the isolated timer driver may not own this linked graph at all.
            *pw::keys::NODE_GROUP => CLOCK_GROUP,
        },
    )
    .expect("consumer stream");
    let _consumer_listener = consumer
        .add_local_listener_with_user_data(observed.clone())
        .param_changed({
            let mut buffers = consumer_buffers();
            move |stream, observed, id, param| {
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else { return };
                let mut info = spa::param::video::VideoInfoRaw::default();
                // A normal SHM consumer negotiates a mapped buffer sized for
                // one raw video frame; it does not allocate or inject pixels.
                let result = info
                    .parse(param)
                    .map(|_| ())
                    .map_err(|err| format!("negotiated format: {err:?}"))
                    .and_then(|()| {
                        stream
                            .update_params(&mut [buffers.as_pod()])
                            .map_err(|err| format!("negotiate Buffers: {err}"))
                    });
                let mut observed = observed.borrow_mut();
                match result {
                    Ok(()) => observed.format = Some(info),
                    Err(err) => observed.error = Some(err),
                }
            }
        })
        .process({
            let second = second.clone();
            move |consumer, observed| {
                observed.borrow_mut().process += 1;
                let Some(mut buffer) = consumer.dequeue_buffer() else {
                    observed.borrow_mut().no_buffer += 1;
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    observed.borrow_mut().no_data += 1;
                    return;
                };
                let (raw_offset, size, stride) = {
                    let chunk = data.chunk();
                    (chunk.offset(), chunk.size(), chunk.stride())
                };
                let mut observed = observed.borrow_mut();
                observed.last_chunk = Some((raw_offset, size, stride));
                if size == 0 {
                    observed.empty += 1;
                    return;
                }
                if size != SIZE || stride != STRIDE as i32 {
                    observed.error = Some(format!(
                        "expected frame bytes={SIZE} stride={STRIDE}; got {size}/{stride}"
                    ));
                    return;
                }
                let offset = raw_offset as usize;
                let payload = data.data().and_then(|mapped| {
                    offset
                        .checked_add(size as usize)
                        .and_then(|end| mapped.get(offset..end))
                });
                match payload {
                    Some(bytes) if bytes == first.as_slice() => observed.a += 1,
                    Some(bytes) if bytes == second.as_slice() => observed.b += 1,
                    _ => observed.error = Some("unknown or unmapped frame payload".into()),
                }
            }
        })
        .register()
        .expect("consumer listener");

    consumer
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [format[0].as_pod()],
        )
        .expect("publish consumer input port");
    let snapshot = || {
        let observed = observed.borrow();
        let format = observed
            .format
            .as_ref()
            .map(|info| (info.format(), info.size()));
        format!(
            "producer {producer_id} seen={} removed={}; consumer {} {:?} driving={};              link={:?} format={format:?} process={} no_buffer={} no_data={}              empty={} last_chunk={:?} matched=[{}, {}] error={:?}",
            seen.get(),
            removed.get(),
            consumer.node_id(),
            consumer.state(),
            consumer.is_driving(),
            observed.link,
            observed.process,
            observed.no_buffer,
            observed.no_data,
            observed.empty,
            observed.last_chunk,
            observed.a,
            observed.b,
            observed.error,
        )
    };
    let check = || {
        assert!(
            observed.borrow().error.is_none()
                && !matches!(consumer.state(), pw::stream::StreamState::Error(_)),
            "private PipeWire graph failure: {}",
            snapshot()
        );
    };
    let lp = loop_.loop_();
    assert!(
        poll_until(lp, || {
            check();
            seen.get()
                && consumer.state() == pw::stream::StreamState::Paused
                && consumer.node_id() != pw::constants::ID_ANY
        }),
        "timed out waiting for both node IDs: {}",
        snapshot()
    );
    // Explicit graph ownership: there is no WirePlumber/session manager and
    // each stream has one port. Omit port IDs so the link factory picks those
    // ports on the exact published nodes rather than global default devices.
    let link = core
        .create_object::<pw::link::Link>(
            "link-factory",
            &pw::properties::properties! {
                "link.output.node" => producer_id.to_string(),
                "link.input.node" => consumer.node_id().to_string(),
            },
        )
        .expect("link producer to consumer");
    let _link_listener = link
        .add_listener_local()
        .info({
            let observed = observed.clone();
            move |info| {
                let state = info.state();
                let mut observed = observed.borrow_mut();
                observed.link = Some(format!(
                    "{state:?} ({}:{} -> {}:{})",
                    info.output_node_id(),
                    info.output_port_id(),
                    info.input_node_id(),
                    info.input_port_id()
                ));
                if let pw::link::LinkState::Error(err) = state {
                    observed.error = Some(format!("link refused the graph: {err}"));
                }
            }
        })
        .register();
    assert!(
        poll_until(lp, || {
            check();
            observed.borrow().a > 0
        }),
        "timed out waiting for exact first frame: {}",
        snapshot()
    );
    {
        let observed = observed.borrow();
        let format = observed
            .format
            .as_ref()
            .expect("consumer format negotiated");
        use spa::param::video::VideoFormat;
        let expected = if cfg!(target_endian = "little") {
            VideoFormat::BGRx
        } else {
            VideoFormat::xRGB
        };
        assert_eq!(format.format(), expected);
        assert_eq!((format.size().width, format.size().height), (WIDTH, HEIGHT));
        assert_eq!(observed.b, 0, "second image arrived before publication");
    }

    {
        let mut frame = slot.lock().unwrap();
        frame.data.copy_from_slice(&second);
        frame.seq += 1;
    }
    assert!(
        poll_until(lp, || {
            check();
            observed.borrow().b > 0
        }),
        "timed out waiting for exact updated frame: {}",
        snapshot()
    );
    eprintln!(
        "private PipeWire: verified two {WIDTH}x{HEIGHT} frames, {SIZE} bytes/stride {STRIDE}"
    );

    // Producer Drop wakes and joins the worker. Verify its actual registry
    // removal and consumer leaving Streaming, not just successful Drop.
    drop(producer);
    assert!(
        poll_until(lp, || {
            removed.get() && consumer.state() != pw::stream::StreamState::Streaming
        }),
        "timed out waiting for producer registry removal: {}",
        snapshot()
    );
    let count = {
        let frames = observed.borrow();
        frames.a + frames.b
    };
    let quiet = Instant::now();
    while quiet.elapsed() < Duration::from_millis(150) {
        lp.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(20)));
    }
    let frames = observed.borrow();
    assert_eq!(frames.a + frames.b, count, "frame received after teardown");
    assert_ne!(consumer.state(), pw::stream::StreamState::Streaming);
    assert!(
        frames.error.is_none(),
        "consumer error after teardown: {:?}",
        frames.error
    );
    eprintln!("private PipeWire: producer removed and no nonempty frames after teardown");
}
