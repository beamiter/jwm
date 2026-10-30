use crate::{
    config::Config,
    executor::{command, query},
    session::{self, Update},
    store::{Store, private_dir},
};
use jwm_eva_core::protocol::AssistantPermit;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot, watch},
};

struct ErrorExpiry {
    permit: AssistantPermit,
    deadline: std::time::Instant,
}

const NATIVE_SNAPSHOT_MAX_AGE: Duration = Duration::from_millis(750);
const WAKE_HEARTBEAT_PERIOD: Duration = Duration::from_millis(500);
const WAKE_LEASE: Duration = Duration::from_secs(3);

struct NativeRead {
    issued: std::time::Instant,
    epoch: u64,
    task: tokio::task::JoinHandle<io::Result<Value>>,
}
#[derive(Default)]
struct NativeStatusPoll {
    epoch: u64,
    pending: Option<NativeRead>,
    cached: Option<(std::time::Instant, Value)>,
}
impl NativeStatusPoll {
    fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.cached = None;
    }
    fn start(&mut self) {
        self.start_with(async { query("get_assistant_status", json!({})).await });
    }
    fn start_with(
        &mut self,
        future: impl std::future::Future<Output = io::Result<Value>> + Send + 'static,
    ) {
        if self.pending.is_none() {
            self.pending = Some(NativeRead {
                issued: std::time::Instant::now(),
                epoch: self.epoch,
                task: tokio::spawn(future),
            });
        }
    }
    async fn take_ready(&mut self) -> Option<io::Result<Value>> {
        if !self
            .pending
            .as_ref()
            .is_some_and(|read| read.task.is_finished())
        {
            return None;
        }
        let read = self.pending.take().unwrap();
        let result = read
            .task
            .await
            .map_err(io::Error::other)
            .and_then(|result| result);
        if read.epoch != self.epoch {
            return None;
        }
        if read.issued.elapsed() > NATIVE_SNAPSHOT_MAX_AGE {
            return Some(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "native_snapshot_expired",
            )));
        }
        Some(result.and_then(|value| {
            if jwm_eva_core::protocol::validate_assistant_protocol(&value).is_err() {
                self.invalidate();
                return Err(io::Error::other("version_incompatible"));
            }
            if !valid_native_snapshot(&value) {
                return Err(io::Error::other("native_snapshot_invalid"));
            }
            self.cached = Some((read.issued, value.clone()));
            Ok(value)
        }))
    }
    fn cached(&self) -> Option<&Value> {
        self.cached
            .as_ref()
            .filter(|(at, _)| at.elapsed() <= NATIVE_SNAPSHOT_MAX_AGE)
            .map(|(_, value)| value)
    }
    fn activated(&mut self, value: &Value) -> bool {
        if !valid_native_snapshot(value) || value["active"] != true {
            return false;
        }
        // A successful wake_activate response is the native, atomically
        // created permit. Ordinary heartbeat responses never call this path.
        self.invalidate();
        self.cached = Some((std::time::Instant::now(), value.clone()));
        true
    }
}
impl Drop for NativeStatusPoll {
    fn drop(&mut self) {
        if let Some(read) = self.pending.take() {
            read.task.abort();
        }
    }
}
fn valid_native_snapshot(value: &Value) -> bool {
    let Some(source) = wake_source(value) else {
        return false;
    };
    if !["active", "wake_requested", "standby_allowed"]
        .iter()
        .all(|field| value[*field].is_boolean())
    {
        return false;
    }
    if value["active"] == true {
        serde_json::from_value::<AssistantPermit>(value["assistant_permit"].clone())
            .is_ok_and(|permit| permit.generation == source.assistant_generation)
    } else {
        value["assistant_permit"].is_null()
    }
}
fn invalidates_wake_observation(event: &Value) -> bool {
    matches!(
        event["event"].as_str(),
        Some("assistant/activated" | "assistant/cancelled" | "assistant/wake_action")
    )
}
fn wake_heartbeat_matches(source: &crate::handoff::Source, ack: &Value) -> bool {
    valid_native_snapshot(ack)
        && wake_source(ack).as_ref() == Some(source)
        && ack["active"] == false
        && ack["wake_requested"] == true
        && ack["standby_allowed"] == true
}

struct WakeLeaseProof {
    source: crate::handoff::Source,
    epoch: u64,
    owner: u64,
    deadline: std::time::Instant,
}
struct WakeHeartbeatFlight {
    source: crate::handoff::Source,
    epoch: u64,
    owner: u64,
    sent: std::time::Instant,
    eligible: bool,
    task: tokio::task::JoinHandle<io::Result<Value>>,
}
struct WakeHeartbeats {
    pending: Option<WakeHeartbeatFlight>,
    proof: Option<WakeLeaseProof>,
    owner: u64,
    last_sent: std::time::Instant,
}
#[derive(Debug, PartialEq)]
enum WakeHeartbeatResult {
    Renewed,
    Observed,
    Ignored,
    Unavailable,
    Rejected,
}
impl Default for WakeHeartbeats {
    fn default() -> Self {
        Self {
            pending: None,
            proof: None,
            owner: 0,
            last_sent: std::time::Instant::now() - WAKE_HEARTBEAT_PERIOD,
        }
    }
}
impl WakeHeartbeats {
    fn invalidate(&mut self) {
        self.owner = self.owner.wrapping_add(1);
        self.proof = None;
        // Retain the single pending write until it settles: aborting its async
        // handle cannot cancel the underlying blocking IPC mutation.
    }
    fn valid(&self, source: &crate::handoff::Source, epoch: u64) -> bool {
        self.proof.as_ref().is_some_and(|p| {
            p.source == *source
                && p.epoch == epoch
                && p.owner == self.owner
                && std::time::Instant::now() < p.deadline
        })
    }
    fn deadline(&self) -> Option<std::time::Instant> {
        self.proof.as_ref().map(|p| p.deadline)
    }
    fn start(
        &mut self,
        source: crate::handoff::Source,
        epoch: u64,
        eligible: bool,
        future: impl std::future::Future<Output = io::Result<Value>> + Send + 'static,
    ) {
        assert!(self.pending.is_none());
        let sent = std::time::Instant::now();
        self.last_sent = sent;
        self.pending = Some(WakeHeartbeatFlight {
            source,
            epoch,
            owner: self.owner,
            sent,
            eligible,
            task: tokio::spawn(future),
        });
    }
    async fn ready(&mut self) -> io::Result<Value> {
        match self.pending.as_mut() {
            Some(flight) => (&mut flight.task)
                .await
                .map_err(io::Error::other)
                .and_then(|r| r),
            None => std::future::pending().await,
        }
    }
    fn complete(&mut self, result: io::Result<Value>, epoch: u64) -> WakeHeartbeatResult {
        let flight = self.pending.take().unwrap();
        if flight.epoch != epoch || flight.owner != self.owner {
            return WakeHeartbeatResult::Ignored;
        }
        match result {
            Ok(ack) => {
                if !valid_native_snapshot(&ack)
                    || wake_source(&ack).as_ref() != Some(&flight.source)
                    || (flight.eligible && !wake_heartbeat_matches(&flight.source, &ack))
                {
                    return WakeHeartbeatResult::Rejected;
                }
                // ACK arrival never extends trust. Reads do not renew leases.
                // This proof alone cannot authorize a new worker or handoff.
                let deadline = flight.sent + WAKE_LEASE;
                if flight.eligible && std::time::Instant::now() < deadline {
                    self.proof = Some(WakeLeaseProof {
                        source: flight.source,
                        epoch: flight.epoch,
                        owner: flight.owner,
                        deadline,
                    });
                    WakeHeartbeatResult::Renewed
                } else {
                    WakeHeartbeatResult::Observed
                }
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                WakeHeartbeatResult::Rejected
            }
            Err(_) => WakeHeartbeatResult::Unavailable,
        }
    }
}
fn wake_start_authorized(
    poll: &NativeStatusPoll,
    heartbeats: &WakeHeartbeats,
    source: &crate::handoff::Source,
) -> bool {
    // Recheck age at the actual start boundary: a preceding cleanup may have
    // consumed time since this tick first received the observation.
    heartbeats.pending.is_none()
        && heartbeats.valid(source, poll.epoch)
        && poll
            .cached()
            .is_some_and(|snapshot| wake_heartbeat_matches(source, snapshot))
}
fn signal_cancel(cancel: &Option<watch::Sender<bool>>) {
    if let Some(cancel) = cancel {
        let _ = cancel.send(true);
    }
}
async fn wake_deadline(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}
async fn within_wake_lease(
    deadline: Option<std::time::Instant>,
    future: impl std::future::Future<Output = io::Result<Value>>,
) -> io::Result<Value> {
    let deadline = deadline.ok_or_else(|| io::Error::other("wake_lease_expired"))?;
    if std::time::Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "wake_lease_expired",
        ));
    }
    tokio::time::timeout_at(deadline.into(), future)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "wake_lease_expired"))?
}
async fn wake_activation_before(
    deadline: Option<std::time::Instant>,
    activation: Value,
) -> io::Result<Value> {
    // The old capture is retained only until this deadline. A late native
    // activation may be observed afresh, but never receives the old handoff.
    within_wake_lease(deadline, command("assistant_control", activation)).await
}
impl ErrorExpiry {
    fn closes(&self, now: std::time::Instant, current: Option<&AssistantPermit>) -> bool {
        now >= self.deadline && current == Some(&self.permit)
    }
}
fn keeps_verified_feedback(error: &io::Error, status: &Value) -> bool {
    error.to_string() == "feedback_history_repair_failed"
        && status["state"] == "responding"
        && status["voice_fallback"] == true
        && status["result_status"].as_str().is_some_and(|status| {
            [
                "succeeded",
                "partial",
                "clarify",
                "noop",
                "unknown",
                "failed",
                "cancelled",
            ]
            .contains(&status)
        })
}

pub fn runtime_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/jwm-{}", unsafe { libc::geteuid() })));
    private_dir(&base)?;
    let dir = base.join("jwm-eva");
    private_dir(&dir)?;
    Ok(dir)
}
struct SocketGuard {
    path: PathBuf,
    _lock: File,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
fn listen() -> io::Result<(UnixListener, SocketGuard)> {
    let dir = runtime_dir()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("daemon.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::other("jwm-eva is already running"));
    }
    let path = dir.join("control.sock");
    if path.exists() {
        fs::remove_file(&path)?;
    }
    let socket = std::os::unix::net::UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    socket.set_nonblocking(true)?;
    Ok((
        UnixListener::from_std(socket)?,
        SocketGuard { path, _lock: lock },
    ))
}
pub async fn control(request: Value) -> io::Result<Value> {
    let mut socket = tokio::time::timeout(
        Duration::from_secs(1),
        UnixStream::connect(runtime_dir()?.join("control.sock")),
    )
    .await
    .map_err(|_| io::Error::other("daemon_connect_timeout"))??;
    check_peer(&socket)?;
    write_frame(&mut socket, &request).await?;
    let response = if request["command"] == "dry_run" {
        read_frame_for(&mut socket, Duration::from_secs(60)).await?
    } else {
        read_frame(&mut socket).await?
    };
    if response["success"] == false {
        return Err(io::Error::other(
            response["error"]
                .as_str()
                .unwrap_or("Eva request rejected")
                .to_owned(),
        ));
    }
    Ok(response)
}
fn check_peer(socket: &UnixStream) -> io::Result<()> {
    if socket.peer_cred()?.uid() != unsafe { libc::geteuid() } {
        Err(io::Error::other("Eva peer user mismatch"))
    } else {
        Ok(())
    }
}
async fn read_frame(socket: &mut UnixStream) -> io::Result<Value> {
    read_frame_for(socket, Duration::from_secs(2)).await
}
async fn read_frame_for(socket: &mut UnixStream, timeout: Duration) -> io::Result<Value> {
    tokio::time::timeout(timeout, async {
        let mut bytes = Vec::new();
        loop {
            let byte = socket.read_u8().await?;
            if byte == b'\n' {
                break;
            }
            if bytes.len() >= 1024 * 1024 {
                return Err(io::Error::other("control_frame_too_large"));
            }
            bytes.push(byte);
        }
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    })
    .await
    .map_err(|_| io::Error::other("control_read_timeout"))?
}
async fn write_frame(socket: &mut UnixStream, value: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() > 1024 * 1024 {
        return Err(io::Error::other("control_frame_too_large"));
    }
    tokio::time::timeout(Duration::from_secs(2), socket.write_all(&bytes))
        .await
        .map_err(|_| io::Error::other("control_write_timeout"))?
}
struct Request {
    request: Value,
    reply: oneshot::Sender<Value>,
}
async fn control_reply(reply: oneshot::Receiver<Value>) -> Value {
    match tokio::time::timeout(Duration::from_secs(60), reply).await {
        Ok(Ok(value)) => value,
        Ok(Err(_)) => json!({"success":false,"error":"daemon_worker_failed"}),
        Err(_) => json!({"success":false,"error":"daemon_request_timeout"}),
    }
}
fn local_undo_request(event: &Value, permit: Option<&AssistantPermit>) -> Option<Value> {
    let expected = permit?;
    let observed =
        serde_json::from_value::<AssistantPermit>(event["data"]["assistant_permit"].clone())
            .ok()?;
    (event["event"] == "assistant/local_action"
        && event["data"]["action"] == "undo"
        && observed == *expected)
        .then(|| json!({"command":"undo","assistant_permit":observed}))
}
fn undo_available(
    config: &Config,
    ledger: &jwm_eva_core::ledger::Ledger,
    wm: Option<&str>,
) -> bool {
    config.undo.enabled
        && ledger.latest_undo_plan().is_ok_and(|plan| {
            !plan.steps.is_empty()
                && Some(plan.wm_instance_id.as_str()) == wm
                && crate::store::wall_ms().saturating_sub(plan.created_ms)
                    <= config.undo.max_age_seconds.saturating_mul(1000)
        })
}
fn heartbeat_undo_available(config: &Config, store: &Mutex<Store>, wm: Option<&str>) -> bool {
    // This is only an optional UI hint. A slow durable write must not block
    // cancellation, JWM lease renewal or the daemon's watchdog heartbeat.
    store
        .try_lock()
        .is_ok_and(|storage| undo_available(config, &storage.ledger, wm))
}
pub async fn run(c: Config) -> io::Result<()> {
    if !c.service.enabled {
        return Err(io::Error::other("Eva service disabled in configuration"));
    }
    let (listener, _guard) = listen()?;
    crate::companion::start();
    // A restarted daemon discards old streams and plans. A still-live native
    // wake request may establish a fresh, non-authoritative standby session.
    let _ = command("assistant_control", json!({"action":"cancel"})).await;
    let store = Arc::new(Mutex::new(Store::open()?));
    // The warm keeper is enabled from the actor loop only while nothing else
    // owns the standard model; it never activates or uploads on its own.
    let warm = crate::warm::Warm::spawn(c.clone());
    let (request_tx, mut requests) = mpsc::channel::<Request>(16);
    let local_request_tx = request_tx.clone();
    let accept = tokio::spawn(async move {
        let slots = Arc::new(tokio::sync::Semaphore::new(16));
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            if check_peer(&socket).is_err() {
                continue;
            }
            let Ok(slot) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let tx = request_tx.clone();
            tokio::spawn(async move {
                let _slot = slot;
                if let Ok(request) = read_frame(&mut socket).await {
                    let (reply, rx) = oneshot::channel();
                    let value = if tx.try_send(Request { request, reply }).is_ok() {
                        control_reply(rx).await
                    } else {
                        json!({"success":false,"error":"daemon_request_queue_full"})
                    };
                    let _ = write_frame(&mut socket, &value).await;
                }
            });
        }
    });
    let (updates_tx, mut updates) = mpsc::channel::<Update>(64);
    let (wake_tx, mut wake_updates) = mpsc::channel::<crate::wake::Update>(16);
    let mut wake_worker = None;
    let mut wake_cancel = None;
    let mut wake_generation = 0;
    let mut active_wake_source: Option<crate::handoff::Source> = None;
    let mut wake_status = json!({"state":"disabled","microphone_active":false});
    let mut wake_requested = false;
    let recovery_clock = std::time::Instant::now();
    let mut wake_recovery = crate::retry::WakeRecovery::new(
        c.wake_word.reconnect_ms,
        crate::store::wall_ms() ^ ((std::process::id() as u64) << 32),
    );
    let (events_tx, mut events) = mpsc::channel(64);
    jwm_ipc_client::subscribe(jwm_ipc_client::JwmIpc::new(), &["assistant"], events_tx);
    let watchdog = Watchdog::start(c.service.watchdog_seconds)?;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut worker: Option<tokio::task::JoinHandle<io::Result<()>>> = None;
    let mut cancellation: Option<watch::Sender<bool>> = None;
    let mut permit: Option<AssistantPermit> = None;
    let mut wm_instance: Option<String> = None;
    let mut status = json!({"state":"idle","text":"","level":0.0,"microphone_active":false});
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut reconciliation: Option<tokio::task::JoinHandle<io::Result<()>>> = None;
    let mut last_reconcile = std::time::Instant::now();
    let mut error_until: Option<ErrorExpiry> = None;
    let mut last_heartbeat = std::time::Instant::now() - Duration::from_secs(1);
    // Store recovery includes durable fsyncs and can wait on host I/O. Do not
    // start systemd's runtime watchdog until recovery, the control listener,
    // actor channels, signal handler and our own watchdog are all ready.
    notify_systemd("READY=1");
    if c.wake_word.enabled
        && query("get_assistant_status", json!({}))
            .await
            .is_ok_and(|status| wake_source(&status).is_some())
    {
        let _ = command("assistant_control", json!({"action":"wake_enable"})).await;
    }
    let mut native_poll = NativeStatusPoll::default();
    let mut wake_heartbeats = WakeHeartbeats::default();
    native_poll.start();
    loop {
        let lease_deadline = wake_heartbeats.deadline();
        // Fresh reads authorize preflight/start; an existing owner may publish
        // from its lease proof while a read stalls. Neither permits new actions.
        let heartbeat_target = native_poll
            .cached()
            .and_then(|snapshot| {
                let source = wake_source(snapshot)?;
                Some((
                    source,
                    snapshot["wake_requested"] == true,
                    snapshot["active"] == false
                        && snapshot["wake_requested"] == true
                        && snapshot["standby_allowed"] == true,
                ))
            })
            .or_else(|| {
                active_wake_source
                    .as_ref()
                    .filter(|source| {
                        wake_worker.is_some() && wake_heartbeats.valid(source, native_poll.epoch)
                    })
                    .map(|source| (source.clone(), true, true))
            });
        let heartbeat_due = if wake_heartbeats.pending.is_none() && heartbeat_target.is_some() {
            Some(wake_heartbeats.last_sent + WAKE_HEARTBEAT_PERIOD)
        } else {
            None
        };
        tokio::select! {
            biased;
            _=wake_deadline(lease_deadline)=>{
                signal_cancel(&wake_cancel);
                wake_event("wake_lease_expired","proof_expired",active_wake_source.as_ref());
                wake_heartbeats.invalidate();native_poll.invalidate();
                stop_wake(&mut wake_worker,&mut wake_cancel).await;
                while wake_updates.try_recv().is_ok(){}
                active_wake_source=None;wake_recovery.pause();
                wake_status=json!({"state":"paused","microphone_active":false});
            },
            result=wake_heartbeats.ready()=>{
                let source=wake_heartbeats.pending.as_ref().map(|f|f.source.clone());
                let outcome=wake_heartbeats.complete(result,native_poll.epoch);
                let label=match outcome {WakeHeartbeatResult::Renewed=>"lease_confirmed",WakeHeartbeatResult::Observed=>"observed",WakeHeartbeatResult::Ignored=>"stale_result_discarded",WakeHeartbeatResult::Unavailable=>"transport_unavailable",WakeHeartbeatResult::Rejected=>"source_rejected"};
                if wake_requested || wake_worker.is_some(){wake_event("wake_heartbeat_completed",label,source.as_ref());}
                if outcome==WakeHeartbeatResult::Rejected {
                    signal_cancel(&wake_cancel);wake_heartbeats.invalidate();native_poll.invalidate();
                    stop_wake(&mut wake_worker,&mut wake_cancel).await;
                    while wake_updates.try_recv().is_ok(){}
                    active_wake_source=None;wake_recovery.pause();
                    wake_status=json!({"state":"paused","microphone_active":false});
                }
                // Transport loss consumes only the previously confirmed proof.
                // A late write cannot start capture: it must still match the
                // frozen epoch/owner and a separate fresh read is required.
                tick.reset_immediately();
            },
            _=wake_deadline(heartbeat_due)=>{
                if let Some((source,enabled,eligible))=heartbeat_target {
                    let mut update=wake_status.clone();
                    if wake_worker.is_none() && eligible {update=json!({"state":"connecting","microphone_active":false,"text":"正在连接语音唤醒"});}
                    update["wake_generation"]=json!(source.wake_generation);update["enabled"]=json!(enabled);update["wm_instance_id"]=json!(source.wm_instance_id);
                    if enabled || wake_worker.is_some(){wake_event("wake_heartbeat_started","sending",Some(&source));}
                    wake_heartbeats.start(source,native_poll.epoch,eligible,command("set_assistant_wake_status",update));
                }
            },
            _=tokio::signal::ctrl_c()=>break,
            _=terminate.recv()=>break,
            Some(request)=requests.recv()=>{
                if request.request["command"]=="dry_run" {
                    if worker.is_some()||permit.is_some(){let _=request.reply.send(json!({"success":false,"error":"Eva is busy; finish the active session before dry-run"}));continue;}
                    let Some(text)=request.request["text"].as_str().filter(|s|!s.is_empty()&&s.len()<=4096).map(str::to_owned)else{let _=request.reply.send(json!({"success":false,"error":"dry_run text must be 1..4096 bytes"}));continue;};
                    signal_cancel(&wake_cancel);wake_heartbeats.invalidate();native_poll.invalidate();
                    stop_wake(&mut wake_worker,&mut wake_cancel).await;
                    wake_status=json!({"state":"paused","microphone_active":false,"text":"正在演练"});
                    let config=c.clone();let storage=store.clone();status=json!({"state":"planning","text":"正在生成演练计划","microphone_active":false,"execution_enabled":false});
                    worker=Some(tokio::spawn(async move{
                        let result=crate::session::dry_run(&config,&text,storage.clone()).await;
                        let value=match &result{Ok(v)=>json!({"success":true,"data":v}),Err(e)=>json!({"success":false,"error":e.to_string()})};
                        let _=request.reply.send(value);crate::store::unknown_pending(&storage).await?;result.map(|_|())
                    }));continue;
                }
                let response=match request.request["command"].as_str().unwrap_or(""){
                    "status"=>json!({"success":true,"data":status,"assistant_permit":permit,"pid":std::process::id(),"configuration_id":c.fingerprint(),"model":c.openai.model,"wake_requested":wake_requested,"wake_generation":wake_generation,"wake_model":c.wake_word.model,"wake":wake_status,"warm":warm.status()}),
                    "undo" if permit.is_some() && request.request.get("assistant_permit").is_none_or(|requested|serde_json::to_value(permit.as_ref().unwrap()).is_ok_and(|p|p==*requested)) && ["listening","follow_up","responding"].contains(&status["state"].as_str().unwrap_or(""))=>{
                        error_until=None;
                        stop(&mut worker,&mut cancellation).await;
                        // Both workers use this permit. Joining first and
                        // draining the old worker's pending UI patches prevents
                        // a delayed FollowUp from reopening the undo UI state.
                        while updates.try_recv().is_ok(){}
                        let p=permit.clone().unwrap();let cfg=c.clone();let storage=store.clone();let tx=updates_tx.clone();
                        status=json!({"state":"executing","text":"正在撤销","microphone_active":false,"task_progress":null});
                        worker=Some(tokio::spawn(async move {
                            let result=crate::executor::undo(&p,storage,&cfg).await?;
                            let _=tx.send(Update{permit:p.clone(),state:json!({"state":"responding","text":if result["status"]=="succeeded"{"已撤销可逆更改。"}else{"可逆更改未全部撤销。"},"microphone_active":false})}).await;
                            tokio::time::sleep(Duration::from_millis(1200)).await;Ok(())
                        }));json!({"success":true,"data":{"status":"accepted","action":"undo"}})
                    },
                    "undo"=>json!({"success":false,"error":"activate Eva and wait for Listening before undo"}),
                    "cancel"=>{
                        signal_cancel(&wake_cancel);signal_cancel(&cancellation);
                        wake_heartbeats.invalidate();native_poll.invalidate();
                        wake_event("wake_stop_requested","local_cancel",active_wake_source.as_ref());
                        let _=command("assistant_control",json!({"action":"wake_disable"})).await;
                        stop_wake(&mut wake_worker,&mut wake_cancel).await;
                        wake_recovery.disable();
                        wake_status=json!({"state":"disabled","microphone_active":false});
                        let result=command("assistant_control",json!({"action":"cancel"})).await;
                        stop(&mut worker,&mut cancellation).await;
                        // Audio and native authority are already stopped. Cost
                        // cleanup may wait on disk; it cannot delay control or
                        // watchdog progress and grants no execution authority.
                        let storage=store.clone();tokio::spawn(async move {
                            if let Err(error)=crate::store::unknown_pending(&storage).await {
                                crate::telemetry::event("usage_cleanup_failed",&json!({"result":crate::errors::session_code(&error)}));
                            }
                        });
                        match result{Ok(v)=>json!({"success":true,"data":v}),Err(e)=>json!({"success":false,"error":e.to_string()})}
                    },
                    _=>json!({"success":false,"error":"unsupported_control_command"}),
                };let _=request.reply.send(response);
            },
            Some(update)=wake_updates.recv()=>{
                if Some(&update.source)==active_wake_source.as_ref() && wake_worker.is_some() {
                    if update.state["state"]=="standby" && update.state["microphone_active"]==true {wake_recovery.standby(recovery_ms(recovery_clock));}
                    wake_status=update.state;
                }
            },
            Some(update)=updates.recv()=>{if permit.as_ref().is_some_and(|p|update.apply(p,&mut status)){last_heartbeat=std::time::Instant::now()-Duration::from_secs(1);}},
            Some(event)=events.recv()=>{
                if invalidates_wake_observation(&event) {
                    signal_cancel(&wake_cancel);wake_heartbeats.invalidate();native_poll.invalidate();
                    stop_wake(&mut wake_worker,&mut wake_cancel).await;
                    while wake_updates.try_recv().is_ok(){}
                    active_wake_source=None;
                    wake_recovery.pause();
                    wake_status=json!({"state":if wake_requested{"paused"}else{"disabled"},"microphone_active":false});
                }
                if let Some(request)=local_undo_request(&event,permit.as_ref()){
                    let (reply,_)=oneshot::channel();
                    let _=local_request_tx.try_send(Request{request,reply});
                }
                tick.reset_immediately();
            }, // Native events wake the actor; authority remains JWM's snapshot.
            _=tick.tick()=>{
                watchdog.beat();notify_systemd("WATCHDOG=1");
                if last_reconcile.elapsed()>=Duration::from_secs(2)&&reconciliation.as_ref().is_none_or(|t|t.is_finished()){
                    reconciliation=Some(tokio::spawn(crate::executor::reconcile(store.clone())));last_reconcile=std::time::Instant::now();
                }
                if error_until.as_ref().is_some_and(|expiry|std::time::Instant::now()>=expiry.deadline){
                    if error_until.as_ref().is_some_and(|expiry|expiry.closes(std::time::Instant::now(),permit.as_ref()))
                        && let Some(p)=permit.take(){let _=command("set_assistant_status",json!({"assistant_permit":p,"state":"closing"})).await;}
                    error_until=None;
                }
                if worker.as_ref().is_some_and(|w|w.is_finished()) {
                    // A final verified result is queued before a fatal voice
                    // delivery error. Consume it before deciding what to show.
                    while let Ok(update)=updates.try_recv(){if let Some(p)=permit.as_ref(){update.apply(p,&mut status);}}
                    let mut keep_feedback=false;
                    if let Some(w)=worker.take(){match w.await{
                        Ok(Ok(()))=>{},
                        Ok(Err(e)) if keeps_verified_feedback(&e,&status)=>{status["microphone_active"]=json!(false);keep_feedback=true;},
                        Ok(Err(e))=>{let (code,text)=crate::errors::describe(&e);status=json!({"state":"error","text":text,"error_code":code,"retryable":crate::errors::recovery(&e)!=crate::errors::Recovery::Permanent,"microphone_active":false});},
                        Err(_)=>{status=json!({"state":"error","text":"Eva 会话已停止","microphone_active":false});}
                    }}
                    if status["state"]=="error" || keep_feedback {
                        if let Some(p)=permit.as_ref(){let mut snapshot=status.clone();snapshot["assistant_permit"]=json!(p);let _=command("set_assistant_status",snapshot).await;}
                        error_until=permit.clone().map(|permit|ErrorExpiry{permit,deadline:std::time::Instant::now()+Duration::from_secs(6)});
                    }else if let Some(p)=permit.take(){let _=command("set_assistant_status",json!({"assistant_permit":p,"state":"closing"})).await;}
                    cancellation=None;
                    if status["state"]!="error" && !keep_feedback{status=json!({"state":"idle","text":"","microphone_active":false});}
                }
                if let Some(p)=permit.as_ref() && last_heartbeat.elapsed()>=Duration::from_secs(1){
                    status["undo_available"]=json!(heartbeat_undo_available(&c,&store,wm_instance.as_deref()));
                    let mut update=status.clone();update["assistant_permit"]=serde_json::to_value(p)?;
                    if command("set_assistant_status",update).await.is_err(){stop(&mut worker,&mut cancellation).await;permit=None;status=json!({"state":"idle","microphone_active":false});}
                    last_heartbeat=std::time::Instant::now();
                }
                // Never let a retried, read-only query block lease renewal or
                // turn an absent observation into a native disable request.
                let sample=native_poll.take_ready().await;
                if let Some(Err(error))=sample.as_ref() {
                    wake_event("wake_snapshot_failed",wake_diagnostic_error_code(error),active_wake_source.as_ref());
                }
                let mut observed=sample.unwrap_or_else(||Err(io::Error::other("native_snapshot_pending")));
                native_poll.start();
                let fresh_observation=observed.is_ok();
                let cached=native_poll.cached().cloned();
                let standby_allowed=observed.as_ref().ok().or(cached.as_ref()).is_some_and(|s|s["standby_allowed"]==true);
                let current_source=observed.as_ref().ok().and_then(wake_source);
                let wake_protocol_ready=current_source.is_some();
                if fresh_observation {
                    let native_generation=current_source.as_ref().unwrap().wake_generation;
                    let previous_wake_requested=wake_requested;
                    wake_requested=observed.as_ref().unwrap()["wake_requested"]==true;
                    if previous_wake_requested && !wake_requested {
                        let reason=match observed.as_ref().ok().and_then(|s|s["standby_blocked_reason"].as_str()) {
                            Some("session_locked")=>"session_locked",
                            Some("overlay_unavailable")=>"overlay_unavailable",
                            Some("session_exiting")=>"session_exiting",
                            _=>"native_disabled",
                        };
                        wake_event("wake_disabled_observed",reason,current_source.as_ref());
                    }
                    let recovery_changed=wake_recovery.observe(current_source.clone(),wake_requested);
                    if native_generation!=wake_generation || recovery_changed || active_wake_source.as_ref().is_some_and(|source|Some(source)!=current_source.as_ref()) {
                        signal_cancel(&wake_cancel);wake_heartbeats.invalidate();
                        stop_wake(&mut wake_worker,&mut wake_cancel).await;
                        while wake_updates.try_recv().is_ok(){}
                        wake_generation=native_generation;active_wake_source=None;
                        wake_status=wake_recovery.status(recovery_ms(recovery_clock),crate::store::wall_ms()).unwrap_or_else(||json!({"state":if wake_requested{"paused"}else{"disabled"},"microphone_active":false}));
                    }
                }
                let mut handoff_for_next=None;
                if fresh_observation && wake_worker.as_ref().is_some_and(|w|w.is_finished()) {
                    let handoff_deadline=wake_heartbeats.deadline();
                    wake_heartbeats.invalidate();
                    let finished=match wake_worker.take().unwrap().await {Ok(result)=>result,Err(_)=>Err(io::Error::other("wake_worker_failed"))};wake_cancel=None;
                    while wake_updates.try_recv().is_ok(){}
                    wake_status=json!({"state":"paused","microphone_active":false});
                    match finished {
                        Ok(crate::wake::Outcome::Detected(mut handoff)) if wake_requested && standby_allowed && worker.is_none() && observed.as_ref().is_ok_and(|s|s["active"]!=true) => {
                            wake_recovery.successful();
                            // Preserve the one native microphone owner while
                            // fresh native authority fences the original source.
                            let current=observed.as_ref().ok().and_then(wake_source);
                            if current.as_ref().is_some_and(|source|handoff.current(source)) {
                                let source=&handoff.source;
                                let mut activation=json!({"action":"wake_activate","wake_generation":source.wake_generation,"wm_instance_id":source.wm_instance_id,"assistant_generation":source.assistant_generation});
                                if let Some(seed)=handoff.seed.as_ref() {activation["source_monitor_token"]=json!(seed.monitor_token);activation["source_onset_monotonic_ns"]=json!(seed.binding.onset_monotonic_ns);}
                                let mut activated=wake_activation_before(handoff_deadline,activation.clone()).await;
                                if activated.as_ref().is_err_and(|error|error.kind()==io::ErrorKind::InvalidInput) && handoff.seed.is_some() {
                                    // A gone/changed source may still wake a
                                    // fresh listening session, never replay it.
                                    handoff.discard_seed();
                                    activation.as_object_mut().unwrap().remove("source_monitor_token");
                                    activation.as_object_mut().unwrap().remove("source_onset_monotonic_ns");
                                    activated=wake_activation_before(handoff_deadline,activation).await;
                                }
                                if let Ok(native)=activated && native_poll.activated(&native) {
                                    observed=Ok(native);
                                    wake_status=json!({"state":"paused","microphone_active":false});
                                    handoff_for_next=Some(handoff);
                                }
                            }
                        },
                        Err(error)=>{
                            let failed_source=active_wake_source.clone();
                            if let Some(native)=recover_wake_failure(&mut wake_recovery,&error,recovery_ms(recovery_clock),failed_source.as_ref()).await {
                                wake_generation=native["wake_generation"].as_u64().unwrap_or(wake_generation);
                                wake_requested=false;
                                wake_recovery.observe(wake_source(&native),false);active_wake_source=None;observed=Ok(native);
                            }
                            wake_status=wake_recovery.status(recovery_ms(recovery_clock),crate::store::wall_ms()).unwrap_or_else(||json!({"state":"paused","microphone_active":false}));
                        },
                        Ok(crate::wake::Outcome::Rotate)=>wake_recovery.successful(),
                        Ok(crate::wake::Outcome::Cancelled)=>wake_recovery.pause(),
                        _=>{},
                    }
                }
                let active=observed.as_ref().ok().or(cached.as_ref()).is_some_and(|s|s["active"]==true);
                if fresh_observation && (!wake_requested || !standby_allowed || active || worker.is_some()) {
                    if wake_worker.is_some() || wake_heartbeats.proof.is_some() {signal_cancel(&wake_cancel);wake_heartbeats.invalidate();}
                    stop_wake(&mut wake_worker,&mut wake_cancel).await;
                    while wake_updates.try_recv().is_ok(){}
                    wake_recovery.pause();
                    wake_status=wake_recovery.status(recovery_ms(recovery_clock),crate::store::wall_ms()).unwrap_or_else(||json!({"state":if wake_requested{"paused"}else{"disabled"},"text":if wake_requested{"对话期间暂停唤醒监听"}else{"语音唤醒已关闭"},"microphone_active":false}));
                } else if wake_protocol_ready && wake_requested && standby_allowed && !active && worker.is_none() && current_source.as_ref().is_some_and(|source|wake_start_authorized(&native_poll,&wake_heartbeats,source)) && wake_worker.is_none() && wake_recovery.ready(recovery_ms(recovery_clock)) {
                    let (cancel_tx,cancel_rx)=watch::channel(false);wake_cancel=Some(cancel_tx);
                    wake_status=json!({"state":"connecting","text":"正在连接语音唤醒","microphone_active":false});
                    if let Some(source)=observed.as_ref().ok().and_then(wake_source) {
                        active_wake_source=Some(source.clone());wake_recovery.started();
                        wake_worker=Some(tokio::spawn(crate::wake::run(c.clone(),source,wake_tx.clone(),store.clone(),cancel_rx)));
                    }
                } else if wake_worker.is_none() && let Some(waiting)=wake_recovery.status(recovery_ms(recovery_clock),crate::store::wall_ms()) {wake_status=waiting;}
                if observed.as_ref().is_err_and(|error|error.to_string()=="version_incompatible") {
                    wake_status=json!({"state":"error","text":"当前 JWM 缺少新版唤醒协议，请更新 JWM 并重新登录，再运行 jwm-eva doctor。","error_code":"version_incompatible","microphone_active":false,"retryable":false});
                }
                if observed.is_ok() {
                  wm_instance=observed.as_ref().ok().and_then(|s|s["wm_instance_id"].as_str()).map(str::to_owned);
                  let next=observed.as_ref().ok().filter(|s|s["active"]==true).and_then(|s|serde_json::from_value::<AssistantPermit>(s["assistant_permit"].clone()).ok());
                  if next!=permit {
                    error_until=None;
                    stop(&mut worker,&mut cancellation).await;permit=next;
                    status=json!({"state":"idle","text":"","microphone_active":false});
                    if let Some(p)=permit.clone(){
                        crate::telemetry::event("activation_received",&json!({"generation":p.generation,"source_activation_ns":observed.as_ref().ok().map(|s|s["activated_monotonic_ns"].clone()),"model":c.openai.model}));
                        let microphone_active=handoff_for_next.as_ref().is_some_and(|handoff|handoff.capture.is_some());
                        status=json!({"state":"connecting","text":"正在连接","microphone_active":microphone_active});
                        if microphone_active {let _=command("set_assistant_status",json!({"assistant_permit":p,"state":"connecting","text":"正在处理唤醒指令","microphone_active":true})).await;}
                        let (cancel_tx,cancel_rx)=watch::channel(false);cancellation=Some(cancel_tx);
                        let warm_socket=if c.openai.keep_warm {warm.take().await} else {None};
                        worker=Some(tokio::spawn(session::run_with_handoff(c.clone(),p,updates_tx.clone(),store.clone(),cancel_rx,handoff_for_next.take(),warm_socket)));
                    }
                  }
                }
                warm.set_enabled(c.openai.keep_warm && worker.is_none());
            }
        }
    }
    warm.stop();
    if let Some(flight) = wake_heartbeats.pending.take() {
        flight.task.abort();
    }
    let shutdown_source = wake_recovery
        .source()
        .cloned()
        .or_else(|| active_wake_source.clone());
    stop_wake(&mut wake_worker, &mut wake_cancel).await;
    if let Some(source) = shutdown_source
        && let Ok(native) = command("assistant_control", guarded_wake_disable(&source)).await
    {
        let _=command("set_assistant_wake_status",json!({"wake_generation":native["wake_generation"],"wm_instance_id":native["wm_instance_id"],"enabled":false,"microphone_active":false,"state":"disabled"})).await;
    }
    stop(&mut worker, &mut cancellation).await;
    if let Some(p) = permit {
        let _ = command(
            "set_assistant_status",
            json!({"assistant_permit":p,"state":"closing"}),
        )
        .await;
    }
    accept.abort();
    if let Some(t) = reconciliation {
        t.abort();
    }
    Ok(())
}
fn recovery_ms(start: std::time::Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}
async fn recover_wake_failure(
    recovery: &mut crate::retry::WakeRecovery,
    error: &io::Error,
    now_ms: u64,
    source: Option<&crate::handoff::Source>,
) -> Option<Value> {
    let decision = recovery.failed(error, now_ms);
    wake_event("wake_failed", wake_diagnostic_error_code(error), source);
    if decision != crate::retry::Decision::Stop {
        return None;
    }
    let reason = if crate::errors::recovery(error) == crate::errors::Recovery::Permanent {
        "permanent_error"
    } else {
        "retry_exhausted"
    };
    wake_event("wake_stop_requested", reason, source);
    let source = source?;
    match command("assistant_control", guarded_wake_disable(source)).await {
        Ok(native) => {
            wake_event("wake_stopped", "disabled", Some(source));
            Some(native)
        }
        Err(error) => {
            wake_event(
                "wake_stop_failed",
                wake_diagnostic_error_code(&error),
                Some(source),
            );
            None
        }
    }
}
fn wake_diagnostic_error_code(error: &io::Error) -> &'static str {
    match error.to_string().as_str() {
        "native_snapshot_pending" => "native_snapshot_pending",
        "native_snapshot_expired" => "native_snapshot_expired",
        "native_snapshot_invalid" => "native_snapshot_invalid",
        "version_incompatible" => "version_incompatible",
        "wake_status_timeout" => "wake_status_timeout",
        "wake_status_unavailable" => "wake_status_unavailable",
        "wake_response_missing" => "wake_response_missing",
        "wake_response_invalid" => "wake_response_invalid",
        "wake_response_timeout" => "wake_response_timeout",
        "wake_configuration_mismatch" => "wake_configuration_mismatch",
        "wake_input_queue_full" => "wake_input_queue_full",
        _ => crate::errors::session_code(error),
    }
}
fn wake_diagnostic_labels(result: &'static str, source: Option<&crate::handoff::Source>) -> Value {
    let mut labels = json!({"phase":"wake","result":result});
    if let Some(source) = source {
        labels["generation"] = json!(source.assistant_generation);
        labels["wake_generation"] = json!(source.wake_generation);
    }
    labels
}
fn wake_event(stage: &'static str, result: &'static str, source: Option<&crate::handoff::Source>) {
    crate::telemetry::event(stage, &wake_diagnostic_labels(result, source));
}
fn guarded_wake_disable(source: &crate::handoff::Source) -> Value {
    json!({"action":"wake_disable","wm_instance_id":source.wm_instance_id,"wake_generation":source.wake_generation,"assistant_generation":source.assistant_generation})
}
fn wake_source(snapshot: &Value) -> Option<crate::handoff::Source> {
    jwm_eva_core::protocol::validate_assistant_protocol(snapshot).ok()?;
    Some(crate::handoff::Source {
        wm_instance_id: snapshot["wm_instance_id"]
            .as_str()
            .filter(|id| !id.is_empty())?
            .to_owned(),
        wake_generation: snapshot["wake_generation"].as_u64()?,
        assistant_generation: snapshot["assistant_generation"].as_u64()?,
    })
}

async fn stop_wake(
    worker: &mut Option<tokio::task::JoinHandle<io::Result<crate::wake::Outcome>>>,
    cancel: &mut Option<watch::Sender<bool>>,
) {
    if let Some(c) = cancel.take() {
        let _ = c.send(true);
    }
    if let Some(mut w) = worker.take()
        && tokio::time::timeout(Duration::from_millis(600), &mut w)
            .await
            .is_err()
    {
        w.abort();
        let _ = w.await;
    }
}
async fn stop(
    worker: &mut Option<tokio::task::JoinHandle<io::Result<()>>>,
    cancel: &mut Option<watch::Sender<bool>>,
) {
    if let Some(c) = cancel.take() {
        let _ = c.send(true);
    }
    if let Some(mut w) = worker.take()
        && tokio::time::timeout(Duration::from_millis(600), &mut w)
            .await
            .is_err()
    {
        w.abort();
        let _ = w.await;
    }
}
pub async fn ensure_running(config: &Config) -> io::Result<()> {
    fn matches(config: &Config, status: &Value) -> io::Result<()> {
        if status["configuration_id"] == config.fingerprint() {
            Ok(())
        } else {
            Err(io::Error::other(
                "running Eva daemon configuration differs; restart the user service with the selected configuration before starting new work",
            ))
        }
    }
    if let Ok(status) = control(json!({"command":"status"})).await {
        return matches(config, &status);
    }
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new("systemctl")
            .args(["--user", "start", "jwm-eva.service"])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    if !matches!(result,Ok(Ok(out))if out.status.success()) {
        return Err(io::Error::other(
            "Eva daemon is not running; install the user service or run jwm-eva daemon",
        ));
    }
    for _ in 0..10 {
        if let Ok(status) = control(json!({"command":"status"})).await {
            return matches(config, &status);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(io::Error::other("Eva daemon start timeout"))
}

fn notify_systemd(message: &str) {
    use std::os::unix::{ffi::OsStrExt, net::UnixDatagram};
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let bytes = path.as_os_str().as_bytes();
    let Ok(socket) = UnixDatagram::unbound() else {
        return;
    };
    let _ = socket.set_nonblocking(true);
    if bytes.first() == Some(&b'@') {
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if bytes.len() >= address.sun_path.len() {
            return;
        }
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (target, byte) in address.sun_path[1..].iter_mut().zip(&bytes[1..]) {
            *target = *byte as libc::c_char;
        }
        unsafe {
            libc::sendto(
                socket.as_raw_fd(),
                message.as_ptr() as *const libc::c_void,
                message.len(),
                libc::MSG_DONTWAIT,
                &address as *const _ as *const libc::sockaddr,
                (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len())
                    as libc::socklen_t,
            );
        }
    } else {
        let _ = socket.send_to(message.as_bytes(), PathBuf::from(path));
    }
}

/// Standalone launches get the same stalled-actor microphone boundary as the
/// systemd unit. Exit terminates all native streams without producing a core
/// dump containing audio or credentials.
struct Watchdog {
    progress: Arc<std::sync::atomic::AtomicU64>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    start: std::time::Instant,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Watchdog {
    fn start(seconds: u64) -> io::Result<Self> {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        let progress = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let start = std::time::Instant::now();
        let (p, s) = (progress.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("eva-watchdog".into())
            .spawn(move || {
                while !s.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(100));
                    if start
                        .elapsed()
                        .as_millis()
                        .saturating_sub(p.load(Ordering::Acquire) as u128)
                        > seconds as u128 * 1000
                    {
                        eprintln!(
                            "jwm-eva: watchdog expired; terminating audio and outstanding work"
                        );
                        std::process::exit(70);
                    }
                }
            })?;
        Ok(Self {
            progress,
            stop,
            start,
            thread: Some(thread),
        })
    }
    fn beat(&self) {
        self.progress.store(
            self.start.elapsed().as_millis().min(u64::MAX as u128) as u64,
            std::sync::atomic::Ordering::Release,
        );
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    fn native_fixture(wake: u64, assistant: u64, active: bool) -> serde_json::Value {
        serde_json::json!({
            "assistant_protocol":jwm_eva_core::protocol::assistant_protocol(),
            "wm_instance_id":"fixture-wm", "wake_generation":wake,
            "assistant_generation":assistant, "wake_requested":true,
            "standby_allowed":true, "active":active,
            "assistant_permit":if active {serde_json::json!({"assistant_session_id":"fixture-session","generation":assistant})}else{serde_json::Value::Null}
        })
    }

    #[test]
    fn cached_wake_heartbeats_reject_new_authority_and_incomplete_snapshots() {
        let snapshot = native_fixture(7, 9, false);
        let source = super::wake_source(&snapshot).unwrap();
        assert!(super::wake_heartbeat_matches(&source, &snapshot));
        for field in [
            "active",
            "wake_requested",
            "standby_allowed",
            "assistant_generation",
            "wake_generation",
            "assistant_protocol",
        ] {
            let mut missing = snapshot.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(!super::valid_native_snapshot(&missing), "{field}");
            assert!(!super::wake_heartbeat_matches(&source, &missing));
        }
        for (field, value) in [
            ("wm_instance_id", serde_json::json!("new-wm")),
            ("wake_generation", serde_json::json!(8)),
            ("assistant_generation", serde_json::json!(10)),
            ("wake_requested", serde_json::json!(false)),
            ("standby_allowed", serde_json::json!(false)),
        ] {
            let mut changed = snapshot.clone();
            changed[field] = value;
            assert!(!super::wake_heartbeat_matches(&source, &changed), "{field}");
        }
        assert!(!super::wake_heartbeat_matches(
            &source,
            &native_fixture(7, 10, true)
        ));
    }

    #[tokio::test]
    async fn cancelled_or_replaced_observation_cannot_revive_old_wake_request() {
        for event in [
            "assistant/activated",
            "assistant/cancelled",
            "assistant/wake_action",
        ] {
            let mut poll = super::NativeStatusPoll::default();
            let (send, receive) = tokio::sync::oneshot::channel();
            poll.start_with(async { Ok(receive.await.unwrap()) });
            assert!(super::invalidates_wake_observation(
                &serde_json::json!({"event":event})
            ));
            poll.invalidate();
            send.send(native_fixture(7, 9, false)).unwrap();
            tokio::task::yield_now().await;
            assert!(poll.take_ready().await.is_none());
            assert!(poll.cached().is_none());
        }
        assert!(!super::invalidates_wake_observation(
            &serde_json::json!({"event":"assistant/execution"})
        ));
    }

    async fn lease_fixture() -> (
        super::WakeHeartbeats,
        super::NativeStatusPoll,
        crate::handoff::Source,
    ) {
        let snapshot = native_fixture(7, 9, false);
        let source = super::wake_source(&snapshot).unwrap();
        let mut poll = super::NativeStatusPoll::default();
        poll.start_with({
            let snapshot = snapshot.clone();
            async move { Ok(snapshot) }
        });
        tokio::task::yield_now().await;
        assert!(poll.take_ready().await.unwrap().is_ok());
        let mut beats = super::WakeHeartbeats::default();
        beats.start(
            source.clone(),
            poll.epoch,
            true,
            async move { Ok(snapshot) },
        );
        let result = beats.ready().await;
        assert_eq!(
            beats.complete(result, poll.epoch),
            super::WakeHeartbeatResult::Renewed
        );
        (beats, poll, source)
    }

    #[tokio::test]
    async fn slow_heartbeat_keeps_capture_until_confirmed_deadline_and_actor_cancel_reachable() {
        use std::time::{Duration, Instant};
        let (mut beats, poll, source) = lease_fixture().await;
        tokio::time::sleep(super::WAKE_HEARTBEAT_PERIOD).await;
        let sent = Instant::now();
        beats.start(source.clone(), poll.epoch, true, async {
            tokio::time::sleep(Duration::from_millis(2160)).await;
            Ok(native_fixture(7, 9, false))
        });
        let mut ticks = 0;
        let mut pulse = tokio::time::interval(Duration::from_millis(20));
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            super::signal_cancel(&Some(cancel));
        });
        let mut cancel_seen = false;
        let result = loop {
            let deadline = beats.deadline();
            tokio::select! {
                result=beats.ready()=>break result,
                _=cancelled.changed(),if !cancel_seen=>{cancel_seen=true;assert!(sent.elapsed()<Duration::from_millis(900));},
                _=pulse.tick()=>{ticks+=1;assert!(beats.valid(&source,poll.epoch));},
                _=super::wake_deadline(deadline)=>panic!("2.16s heartbeat consumed the confirmed lease"),
            }
        };
        cancel_task.await.unwrap();
        assert!(cancel_seen && ticks > 80);
        assert_eq!(
            beats.complete(result, poll.epoch),
            super::WakeHeartbeatResult::Renewed
        );
        assert!(
            beats
                .deadline()
                .unwrap()
                .duration_since(sent + super::WAKE_LEASE)
                < Duration::from_millis(20)
        );
        assert!(
            beats.last_sent.elapsed() > super::WAKE_HEARTBEAT_PERIOD,
            "next write is immediately due, without waiting after ACK"
        );
        beats.start(source.clone(), poll.epoch, true, async {
            Ok(native_fixture(7, 9, false))
        });
        let result = beats.ready().await;
        assert_eq!(
            beats.complete(result, poll.epoch),
            super::WakeHeartbeatResult::Renewed
        );
        assert!(beats.valid(&source, poll.epoch));
    }

    #[tokio::test]
    async fn four_second_read_does_not_block_lease_renewals_or_spawn_more_reads() {
        use std::time::Duration;
        let (mut beats, mut poll, source) = lease_fixture().await;
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = reads.clone();
        poll.start_with(async move {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(4)).await;
            Ok(native_fixture(7, 9, false))
        });
        let mut pulse = tokio::time::interval(super::WAKE_HEARTBEAT_PERIOD);
        let mut renewals = 0;
        loop {
            pulse.tick().await;
            assert!(beats.valid(&source, poll.epoch));
            if let Some(result) = poll.take_ready().await {
                assert_eq!(
                    super::wake_diagnostic_error_code(&result.unwrap_err()),
                    "native_snapshot_expired"
                );
                break;
            }
            let count = reads.clone();
            poll.start_with(async move {
                count.fetch_add(100, std::sync::atomic::Ordering::Relaxed);
                Ok(native_fixture(7, 9, false))
            });
            beats.start(source.clone(), poll.epoch, true, async {
                Ok(native_fixture(7, 9, false))
            });
            let result = beats.ready().await;
            assert_eq!(
                beats.complete(result, poll.epoch),
                super::WakeHeartbeatResult::Renewed
            );
            renewals += 1;
        }
        assert!(renewals >= 7);
        assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(
            poll.cached().is_none(),
            "read ACKs never refresh authorization"
        );
    }

    #[tokio::test]
    async fn expired_lease_and_old_epoch_or_owner_ack_cannot_revive_capture() {
        use std::time::{Duration, Instant};
        for invalidation in ["epoch", "owner", "late"] {
            let (mut beats, mut poll, source) = lease_fixture().await;
            let (send, receive) = tokio::sync::oneshot::channel();
            beats.start(source.clone(), poll.epoch, true, async {
                Ok(receive.await.unwrap())
            });
            match invalidation {
                "epoch" => poll.invalidate(),
                "owner" => beats.invalidate(),
                _ => {
                    beats.proof.as_mut().unwrap().deadline =
                        Instant::now() - Duration::from_millis(1);
                    beats.pending.as_mut().unwrap().sent = Instant::now() - Duration::from_secs(4);
                    assert!(!beats.valid(&source, poll.epoch));
                    // The expiry branch revokes the owner before awaiting IPC.
                    super::wake_deadline(beats.deadline()).await;
                }
            }
            send.send(native_fixture(7, 9, false)).unwrap();
            let result = beats.ready().await;
            let outcome = beats.complete(result, poll.epoch);
            assert!(matches!(
                outcome,
                super::WakeHeartbeatResult::Ignored | super::WakeHeartbeatResult::Observed
            ));
            assert!(!beats.valid(&source, poll.epoch));
        }
    }

    #[tokio::test]
    async fn fresh_read_and_lease_proof_are_separate_start_requirements() {
        use std::time::{Duration, Instant};
        let (mut beats, mut poll, source) = lease_fixture().await;
        assert!(super::wake_start_authorized(&poll, &beats, &source));
        poll.cached.as_mut().unwrap().0 = Instant::now() - Duration::from_millis(751);
        assert!(poll.cached().is_none());
        assert!(!super::wake_start_authorized(&poll, &beats, &source));
        assert!(
            beats.valid(&source, poll.epoch),
            "proof alone supplies no fresh snapshot"
        );
        poll.cached.as_mut().unwrap().0 = Instant::now();
        beats.proof.as_mut().unwrap().deadline = Instant::now() - Duration::from_millis(1);
        assert!(poll.cached().is_some());
        assert!(!super::wake_start_authorized(&poll, &beats, &source));
        assert!(
            !beats.valid(&source, poll.epoch),
            "fresh reads cannot extend native lease"
        );
        beats.invalidate();
        beats.start(source.clone(), poll.epoch, true, async {
            Ok(native_fixture(7, 9, false))
        });
        beats.pending.as_mut().unwrap().sent = Instant::now() - Duration::from_millis(2160);
        poll.cached.as_mut().unwrap().0 = Instant::now() - Duration::from_millis(2160);
        let result = beats.ready().await;
        assert_eq!(
            beats.complete(result, poll.epoch),
            super::WakeHeartbeatResult::Renewed
        );
        assert!(
            poll.cached().is_none(),
            "slow preflight must wait for a new fresh query"
        );
        assert!(!super::wake_start_authorized(&poll, &beats, &source));
    }

    #[tokio::test]
    async fn transport_loss_preserves_only_old_proof_but_native_rejection_does_not_authorize() {
        let (mut beats, poll, source) = lease_fixture().await;
        let deadline = beats.deadline();
        beats.start(source.clone(), poll.epoch, true, async {
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "private"))
        });
        let result = beats.ready().await;
        assert_eq!(
            beats.complete(result, poll.epoch),
            super::WakeHeartbeatResult::Unavailable
        );
        assert_eq!(beats.deadline(), deadline);
        for ack in [native_fixture(8, 9, false), native_fixture(7, 10, true)] {
            beats.start(source.clone(), poll.epoch, true, async move { Ok(ack) });
            let result = beats.ready().await;
            assert_eq!(
                beats.complete(result, poll.epoch),
                super::WakeHeartbeatResult::Rejected
            );
            beats.invalidate(); // actor signals capture and invalidates on rejection
            assert!(!beats.valid(&source, poll.epoch));
        }
    }

    #[tokio::test]
    async fn local_cancel_reaches_both_owners_before_slow_native_revoke() {
        let (wake, wake_rx) = tokio::sync::watch::channel(false);
        let (session, session_rx) = tokio::sync::watch::channel(false);
        super::signal_cancel(&Some(wake));
        super::signal_cancel(&Some(session));
        let native_revoke = async {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        };
        tokio::pin!(native_revoke);
        assert!(*wake_rx.borrow() && *session_rx.borrow());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut native_revoke)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn activation_deadline_drops_old_capture_and_late_mutation_has_no_handoff() {
        use std::{
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
            time::{Duration, Instant},
        };
        let dispatched = Arc::new(AtomicBool::new(false));
        let flag = dispatched.clone();
        let result = super::within_wake_lease(
            Some(Instant::now() - Duration::from_millis(1)),
            async move {
                flag.store(true, Ordering::SeqCst);
                Ok(native_fixture(7, 10, true))
            },
        )
        .await;
        assert!(result.is_err());
        assert!(!dispatched.load(Ordering::SeqCst));
        struct CaptureOwner(Arc<AtomicBool>);
        impl Drop for CaptureOwner {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let mutated = Arc::new(AtomicBool::new(false));
        let flag = mutated.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        let mutation = tokio::spawn(async move {
            receive.await.unwrap();
            flag.store(true, Ordering::SeqCst);
            Ok(native_fixture(7, 10, true))
        });
        let outcome = {
            let _capture = CaptureOwner(released.clone());
            super::within_wake_lease(
                Some(Instant::now() + Duration::from_millis(10)),
                async move { mutation.await.unwrap() },
            )
            .await
        };
        assert_eq!(outcome.unwrap_err().to_string(), "wake_lease_expired");
        assert!(released.load(Ordering::SeqCst));
        send.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(
            mutated.load(Ordering::SeqCst),
            "underlying native mutation may finish late"
        );
        let poll = super::NativeStatusPoll::default();
        assert!(
            poll.cached().is_none(),
            "late mutation cannot attach capture or create fresh query authority"
        );
    }

    #[tokio::test]
    async fn only_complete_activation_ack_can_replace_old_pending_observation() {
        let mut poll = super::NativeStatusPoll::default();
        assert!(!poll.activated(&native_fixture(7, 9, false)));
        let (send, receive) = tokio::sync::oneshot::channel();
        poll.start_with(async { Ok(receive.await.unwrap()) });
        let activated = native_fixture(7, 10, true);
        assert!(poll.activated(&activated));
        send.send(native_fixture(7, 9, false)).unwrap();
        tokio::task::yield_now().await;
        assert!(poll.take_ready().await.is_none());
        assert_eq!(poll.cached(), Some(&activated));
    }

    #[tokio::test]
    async fn incompatible_snapshot_is_diagnostic_without_cached_authority() {
        let mut poll = super::NativeStatusPoll::default();
        let mut old = native_fixture(7, 9, false);
        old.as_object_mut().unwrap().remove("assistant_protocol");
        poll.start_with(async move { Ok(old) });
        tokio::task::yield_now().await;
        let error = poll.take_ready().await.unwrap().unwrap_err();
        assert_eq!(
            super::wake_diagnostic_error_code(&error),
            "version_incompatible"
        );
        assert!(poll.cached().is_none());
        for code in [
            "native_snapshot_expired",
            "native_snapshot_invalid",
            "wake_status_timeout",
        ] {
            assert_eq!(
                super::wake_diagnostic_error_code(&std::io::Error::other(code)),
                code
            );
        }
    }

    #[test]
    fn wake_diagnostics_keep_only_category_and_numeric_source() {
        let source = crate::handoff::Source {
            wm_instance_id: "private-window-title-or-token".into(),
            wake_generation: 7,
            assistant_generation: 9,
        };
        let error = std::io::Error::other("budget_exceeded private detail");
        let labels =
            super::wake_diagnostic_labels(crate::errors::session_code(&error), Some(&source));
        assert_eq!(
            crate::telemetry::fields(&labels),
            serde_json::json!({"phase":"wake","result":"budget_exceeded","generation":9,"wake_generation":7})
        );
        assert_eq!(
            super::wake_diagnostic_labels("native_disabled", None),
            serde_json::json!({"phase":"wake","result":"native_disabled"})
        );
        assert!(!labels.to_string().contains("private"));
    }

    #[tokio::test]
    async fn wake_failure_diagnostics_preserve_retry_and_stop_decisions() {
        for (error, expected) in [
            ("temporary failure private detail", "waiting"),
            ("rate_limited private detail", "waiting"),
            ("budget_exceeded private detail", "stopped"),
            ("realtime_request_rejected private detail", "stopped"),
        ] {
            let mut recovery = crate::retry::WakeRecovery::new(3000, 1);
            recovery.observe(
                Some(crate::handoff::Source {
                    wm_instance_id: "fixture".into(),
                    wake_generation: 7,
                    assistant_generation: 9,
                }),
                true,
            );
            // No source is supplied to the native disable path: this tests
            // local classification and emits no IPC or desktop request.
            assert!(
                super::recover_wake_failure(
                    &mut recovery,
                    &std::io::Error::other(error),
                    100,
                    None,
                )
                .await
                .is_none()
            );
            let status = recovery.status(100, 1000).unwrap();
            assert_eq!(status["retry_state"], expected);
            assert_eq!(status["retry_attempt"], 1);
        }
    }

    #[test]
    fn failed_voice_delivery_preserves_only_an_already_published_verified_outcome() {
        let mut status = serde_json::json!({"state":"responding","result_status":"succeeded","voice_fallback":true,"text":"已切到第3个工作区"});
        let failure = std::io::Error::other("feedback_history_repair_failed");
        assert!(super::keeps_verified_feedback(&failure, &status));
        assert!(!super::keeps_verified_feedback(
            &std::io::Error::other("auth_failed"),
            &status
        ));
        status["result_status"] = serde_json::json!("unknown");
        assert!(super::keeps_verified_feedback(&failure, &status));
        status["voice_fallback"] = serde_json::json!(false);
        assert!(!super::keeps_verified_feedback(&failure, &status));
        status["voice_fallback"] = serde_json::json!(true);
        status["state"] = serde_json::json!("planning");
        assert!(!super::keeps_verified_feedback(&failure, &status));
    }
    #[test]
    fn reading_an_error_card_never_ends_a_new_retry_or_reactivated_session() {
        let now = std::time::Instant::now();
        let original = jwm_eva_core::protocol::AssistantPermit {
            assistant_session_id: "original".into(),
            generation: 1,
        };
        let retry = jwm_eva_core::protocol::AssistantPermit {
            assistant_session_id: "retry".into(),
            generation: 2,
        };
        let expiry = super::ErrorExpiry {
            permit: original.clone(),
            deadline: now + std::time::Duration::from_secs(6),
        };
        assert!(!expiry.closes(now + std::time::Duration::from_secs(5), Some(&original)));
        assert!(expiry.closes(now + std::time::Duration::from_secs(6), Some(&original)));
        assert!(!expiry.closes(now + std::time::Duration::from_secs(60), Some(&retry)));
        assert!(!expiry.closes(now + std::time::Duration::from_secs(60), None));
    }
    #[test]
    fn complete_source_fields_without_the_required_contract_never_open_wake_audio() {
        let mut source =
            serde_json::json!({"wm_instance_id":"wm","wake_generation":3,"assistant_generation":7});
        assert!(super::wake_source(&source).is_none());
        source["assistant_protocol"] = jwm_eva_core::protocol::assistant_protocol();
        assert!(super::wake_source(&source).is_some());
        source["assistant_protocol"]["features"] =
            serde_json::json!(jwm_eva_core::protocol::ASSISTANT_WAKE_FEATURES);
        assert!(
            super::wake_source(&source).is_none(),
            "wake without a compatible conversation cannot start"
        );
    }
    #[test]
    fn automatic_wake_stop_uses_the_failed_workers_complete_source() {
        let old = serde_json::json!({"assistant_protocol":jwm_eva_core::protocol::assistant_protocol(),"wm_instance_id":"old-wm","wake_generation":3,"assistant_generation":7});
        let frozen = super::wake_source(&old).unwrap();
        let newer = serde_json::json!({"assistant_protocol":jwm_eva_core::protocol::assistant_protocol(),"wm_instance_id":"restarted-wm","wake_generation":3,"assistant_generation":7});
        assert_ne!(super::wake_source(&newer).unwrap(), frozen);
        let command = super::guarded_wake_disable(&frozen);
        assert_eq!(
            command,
            serde_json::json!({"action":"wake_disable","wm_instance_id":"old-wm","wake_generation":3,"assistant_generation":7})
        );
        assert!(
            super::wake_source(&serde_json::json!({"wm_instance_id":"old-wm","wake_generation":3}))
                .is_none()
        );
    }
    #[test]
    fn stopped_worker_cannot_retry_after_cancel_or_a_source_change_until_authorized() {
        let source=super::wake_source(&serde_json::json!({"assistant_protocol":jwm_eva_core::protocol::assistant_protocol(),"wm_instance_id":"wm","wake_generation":3,"assistant_generation":7})).unwrap();
        let mut recovery = crate::retry::WakeRecovery::new(3000, 99);
        recovery.observe(Some(source.clone()), true);
        recovery.failed(&std::io::Error::other("connect_timeout"), 0);
        recovery.observe(
            Some(crate::handoff::Source {
                wake_generation: 4,
                ..source.clone()
            }),
            false,
        );
        assert!(!recovery.ready(u64::MAX));
        recovery.disable();
        assert!(!recovery.ready(u64::MAX));
        recovery.observe(
            Some(crate::handoff::Source {
                wake_generation: 5,
                ..source.clone()
            }),
            true,
        );
        assert!(recovery.ready(0));
        recovery.failed(&std::io::Error::other("auth_failed"), 0);
        recovery.observe(
            Some(crate::handoff::Source {
                wake_generation: 5,
                ..source.clone()
            }),
            true,
        );
        assert!(!recovery.ready(u64::MAX));
        recovery.observe(
            Some(crate::handoff::Source {
                wm_instance_id: "new-wm".into(),
                ..source
            }),
            false,
        );
        assert!(!recovery.ready(u64::MAX));
    }
    #[test]
    fn busy_store_does_not_block_undo_heartbeat() {
        let base = if std::path::Path::new("/dev/shm").is_dir() {
            std::path::PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        let dir = base.join(format!(
            "eva-heartbeat-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::store::Store::open_at(dir.clone()).unwrap(),
        ));
        let held = store.lock().unwrap();
        let probe_store = store.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let probe = std::thread::spawn(move || {
            send.send(super::heartbeat_undo_available(
                &crate::config::Config::default(),
                &probe_store,
                Some("wm"),
            ))
            .unwrap();
        });
        let result = receive.recv_timeout(std::time::Duration::from_millis(200));
        // Release before joining even on failure, so a blocking-lock regression
        // fails this assertion rather than hanging the complete test process.
        drop(held);
        probe.join().unwrap();
        assert!(!result.unwrap());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn dropped_worker_reply_returns_a_structured_failure_over_the_socket() {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let (mut client, mut server) = tokio::net::UnixStream::pair().unwrap();
        let task = tokio::spawn(async move {
            let result = super::control_reply(receiver).await;
            super::write_frame(&mut server, &result).await.unwrap();
        });
        // A panicking worker drops its sender without producing a response.
        drop(sender);
        let response = super::read_frame(&mut client).await.unwrap();
        assert_eq!(response["success"], false);
        assert_eq!(response["error"], "daemon_worker_failed");
        task.await.unwrap();
    }

    #[test]
    fn local_undo_is_bound_to_the_full_active_permit_and_latest_mutation() {
        use jwm_eva_core::{
            ledger::{Ledger, UndoPlan, UndoStep},
            protocol::{Action, AssistantPermit},
        };
        use serde_json::json;
        let permit = AssistantPermit {
            assistant_session_id: "current-session".into(),
            generation: 1,
        };
        let mut event = json!({"event":"assistant/local_action","data":{"action":"undo","assistant_permit":permit}});
        assert!(super::local_undo_request(&event, Some(&permit)).is_some());
        event["data"]["assistant_permit"]["assistant_session_id"] = json!("previous-wm-session");
        assert!(super::local_undo_request(&event, Some(&permit)).is_none());
        assert!(super::local_undo_request(&event, None).is_none());
        let config = crate::config::Config::default();
        let mut ledger = Ledger::default();
        ledger.remember_undo(
            UndoPlan {
                plan_hash: "reversible".into(),
                previous_mutating_plan_hash: None,
                wm_instance_id: "wm".into(),
                created_ms: crate::store::wall_ms(),
                steps: vec![UndoStep {
                    inverse: Action {
                        name: "switch_tags".into(),
                        arguments: json!({"monitor_token":"m","tag_mask":1}),
                    },
                    after: json!({"tag_mask":4}),
                }],
            },
            crate::store::wall_ms(),
        );
        assert!(super::undo_available(&config, &ledger, Some("wm")));
        assert!(!super::undo_available(
            &config,
            &ledger,
            Some("restarted-wm")
        ));
        ledger.last_mutating_plan_hash = Some("newer-unknown-operation".into());
        assert!(!super::undo_available(&config, &ledger, Some("wm")));
    }
    #[test]
    fn watchdog_stalled_actor_exits_and_live_actor_does_not() {
        for (mode, expected) in [
            ("stalled", Some(70)),
            ("alive", Some(0)),
            ("slow_store", Some(0)),
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "daemon::tests::watchdog_child", "--nocapture"])
                .env("EVA_WATCHDOG_TEST", mode)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                expected,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    #[test]
    fn watchdog_child() {
        let Ok(mode) = std::env::var("EVA_WATCHDOG_TEST") else {
            return;
        };
        if mode == "slow_store" {
            use crate::{config::Config, store::Store};
            use std::{
                sync::{Arc, Mutex},
                time::Duration,
            };
            let dir = [std::path::PathBuf::from("/dev/shm"), std::env::temp_dir()]
                .into_iter()
                .find_map(|base| {
                    let p = base.join(format!("eva-watchdog-io-{}", std::process::id()));
                    crate::store::private_dir(&p).ok().map(|()| p)
                })
                .unwrap();
            let store = Arc::new(Mutex::new(Store::open_at(dir.clone()).unwrap()));
            let watchdog = super::Watchdog::start(1).unwrap();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let storage = store.clone();
                    let write = tokio::spawn(async move {
                        crate::store::access(&storage, |storage| {
                            std::thread::sleep(Duration::from_millis(1500));
                            storage.reserve("slow-watchdog-write", "plan", &Config::default())
                        })
                        .await
                    });
                    while !write.is_finished() {
                        watchdog.beat();
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    write.await.unwrap().unwrap();
                });
            drop(watchdog);
            drop(store);
            std::fs::remove_dir_all(dir).unwrap();
            return;
        }
        let watchdog = super::Watchdog::start(1).unwrap();
        if mode == "stalled" {
            std::thread::sleep(std::time::Duration::from_secs(3));
            panic!("watchdog failed to terminate stalled actor");
        }
        for _ in 0..6 {
            watchdog.beat();
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}
