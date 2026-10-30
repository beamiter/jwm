use crate::config::Config;
use jwm_eva_core::{
    ledger::Ledger,
    usage::{Rates, TokenUsage, UsageMeter},
};
use std::{
    fs::{self, OpenOptions},
    io,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Acquire the store and perform durable I/O outside the async runtime. The
/// closure sees current state under the same exclusive lock as synchronous
/// callers; no stale snapshot can overwrite a newer receipt. Cancellation may
/// leave bookkeeping running, but it never dispatches desktop/provider work.
pub async fn access<T: Send + 'static>(
    store: &Arc<Mutex<Store>>,
    work: impl FnOnce(&mut Store) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        let mut store = store
            .lock()
            .map_err(|_| io::Error::other("store_unavailable"))?;
        work(&mut store)
    })
    .await
    .map_err(|_| io::Error::other("store_worker_failed"))?
}

pub async fn reserve(
    store: &Arc<Mutex<Store>>,
    id: &str,
    phase: &str,
    c: &Config,
) -> io::Result<()> {
    let (id, phase, c) = (id.to_owned(), phase.to_owned(), c.clone());
    access(store, move |store| store.reserve(&id, &phase, &c)).await
}
pub async fn reserve_transcription(
    store: &Arc<Mutex<Store>>,
    id: &str,
    c: &Config,
) -> io::Result<()> {
    if !c.openai.input_transcription {
        return Ok(());
    }
    let (id, c) = (id.to_owned(), c.clone());
    access(store, move |store| store.reserve_transcription(&id, &c)).await
}
pub async fn associate(store: &Arc<Mutex<Store>>, request: &str, response: &str) -> io::Result<()> {
    let (request, response) = (request.to_owned(), response.to_owned());
    access(store, move |store| store.associate(&request, &response)).await
}
pub async fn receipt(store: &Arc<Mutex<Store>>, event: &serde_json::Value) -> io::Result<()> {
    if !matches!(
        event["type"].as_str(),
        Some(
            "response.created"
                | "response.done"
                | "conversation.item.input_audio_transcription.completed"
                | "conversation.item.input_audio_transcription.failed"
        )
    ) {
        return Ok(());
    }
    let event = event.clone();
    access(store, move |store| store.receipt(&event)).await
}
pub async fn unknown_pending(store: &Arc<Mutex<Store>>) -> io::Result<()> {
    access(store, Store::unknown_pending).await
}
pub async fn ledger(store: &Arc<Mutex<Store>>) -> io::Result<Ledger> {
    access(store, |store| Ok(store.ledger.clone())).await
}

pub struct Store {
    pub dir: PathBuf,
    pub ledger: Ledger,
    pub usage: UsageMeter,
    pending_transcription: std::collections::BTreeMap<String, serde_json::Value>,
    _lock: std::fs::File,
}
pub fn private_dir(path: &std::path::Path) -> io::Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "Eva directory must be user-owned, mode 0700, and not a symlink",
        ));
    }
    Ok(())
}
pub fn atomic_json(path: &std::path::Path, value: &impl serde::Serialize) -> io::Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let tmp = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = (|| {
        serde_json::to_writer(&mut f, value)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        OpenOptions::new()
            .read(true)
            .open(path.parent().unwrap())?
            .sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
impl Store {
    pub fn open() -> io::Result<Self> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
            });
        let dir = base.join("jwm/eva");
        Self::open_at(dir)
    }
    pub fn open_at(dir: PathBuf) -> io::Result<Self> {
        private_dir(&dir)?;
        use std::os::fd::AsRawFd;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("store.lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::other(
                "Eva state store is in use; send diagnostics to the running daemon",
            ));
        }
        let mut ledger = Ledger::load(&dir.join("operations.json")).map_err(io::Error::other)?;
        // Upgrade legacy full replies before the next atomic store write.
        for record in ledger.operations.values_mut() {
            if let Some(result) = record.result.as_ref() {
                record.result = Some(crate::result_projection::for_ledger(
                    result,
                    &record.action,
                    &serde_json::Value::Null,
                ));
            }
        }
        let usage = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("usage.json"))
        {
            Ok(file) => {
                let m = file.metadata()?;
                if !m.is_file()
                    || m.uid() != unsafe { libc::geteuid() }
                    || m.mode() & 0o077 != 0
                    || m.len() > 4 * 1024 * 1024
                {
                    return Err(io::Error::other("invalid usage ledger"));
                }
                let mut bytes = Vec::new();
                file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > 4 * 1024 * 1024 {
                    return Err(io::Error::other("invalid usage ledger"));
                }
                serde_json::from_slice(&bytes).map_err(io::Error::other)?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => UsageMeter::default(),
            Err(e) => return Err(e),
        };
        let mut store = Self {
            dir,
            ledger,
            usage,
            _lock: lock,
            pending_transcription: Default::default(),
        };
        for r in store.usage.records.values_mut() {
            if r.settled_microusd.is_none() {
                r.unknown = true;
            }
        }
        store.save()?;
        Ok(store)
    }
    pub fn save(&self) -> io::Result<()> {
        self.ledger
            .save(&self.dir.join("operations.json"))
            .map_err(io::Error::other)?;
        atomic_json(&self.dir.join("usage.json"), &self.usage)
    }
    pub fn reserve(&mut self, id: &str, phase: &str, c: &Config) -> io::Result<()> {
        self.reserve_amount(id, phase, 50_000, c)
    }
    fn reserve_amount(
        &mut self,
        id: &str,
        phase: &str,
        estimate: u64,
        c: &Config,
    ) -> io::Result<()> {
        let day = match c.budget.timezone.as_str() {
            "Asia/Shanghai" => chrono::Utc::now()
                .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
                .format("%Y-%m-%d")
                .to_string(),
            "UTC" => chrono::Utc::now().format("%Y-%m-%d").to_string(),
            _ => chrono::Local::now().format("%Y-%m-%d").to_string(),
        };
        // Reservation includes audio/history overhead, retained when usage is unknown.
        self.usage
            .reserve(
                id,
                &day,
                phase,
                estimate,
                if c.budget.enabled {
                    (c.budget.daily_limit_usd * 1_000_000.0) as u64
                } else {
                    u64::MAX
                },
            )
            .map_err(io::Error::other)?;
        let record = self.usage.records.get_mut(id).unwrap();
        record.model = c.openai.model.clone();
        record.price_version = c.pricing.version.clone();
        let convert = |v: f64| (v * 1_000_000.0).ceil() as u64;
        record.rates = Rates {
            input_text: convert(c.pricing.input_text),
            input_audio: convert(c.pricing.input_audio),
            cached_text: convert(c.pricing.cached_text),
            cached_audio: convert(c.pricing.cached_audio),
            output_text: convert(c.pricing.output_text),
            output_audio: convert(c.pricing.output_audio),
        };
        if phase == "transcription" {
            record.model = "gpt-transcribe".into();
            record.rates = Rates::default();
            record.duration_microusd_per_second =
                (c.pricing.transcription_per_minute * 1_000_000.0 / 60.0).ceil() as u64;
            record.price_version = format!("{}:gpt-transcribe-duration", c.pricing.version);
        }
        self.save()
    }
    /// Must run before audio upload, since VAD commit can independently start ASR.
    pub fn reserve_transcription(&mut self, id: &str, c: &Config) -> io::Result<()> {
        if !c.openai.input_transcription {
            return Ok(());
        }
        let rate = (c.pricing.transcription_per_minute * 1_000_000.0 / 60.0).ceil() as u64;
        let estimate = (c.interaction.max_utterance_ms as u128 * rate as u128)
            .div_ceil(1000)
            .min(u64::MAX as u128) as u64;
        self.reserve_amount(id, "transcription", estimate, c)
    }
    /// Audio already uploaded may produce additional VAD/ASR items which
    /// never belonged to a desktop turn. Their historical cost still counts;
    /// disabling budget admission here cannot authorize any new API request.
    pub fn register_foreign_transcription(&mut self, item: &str, c: &Config) -> io::Result<()> {
        if !c.openai.input_transcription {
            return Ok(());
        }
        if item.is_empty() || item.len() > 128 || item.chars().any(char::is_control) {
            return Err(io::Error::other("invalid_transcription_item"));
        }
        let response = format!("asr:{item}:0");
        if self.usage.records.contains_key(&response) || self.usage.is_archived(&response) {
            return Ok(());
        }
        // A deterministic per-item intent resumes a reservation whose durable
        // association was interrupted, rather than adding a second estimate.
        let request = format!("foreign-asr:{item}:0");
        if !self.usage.records.contains_key(&request) {
            let mut historical = c.clone();
            historical.budget.enabled = false;
            self.reserve_transcription(&request, &historical)?;
        }
        self.associate_transcription(&request, item)
    }
    pub fn associate_transcription(&mut self, request: &str, item: &str) -> io::Result<()> {
        let id = format!("asr:{item}:0");
        self.usage
            .associate_response(request, &id)
            .map_err(io::Error::other)?;
        if let Some(usage) = self.pending_transcription.remove(&id) {
            self.transcription_receipt(&id, &usage)?;
        }
        self.save()
    }
    fn transcription_receipt(&mut self, id: &str, usage: &serde_json::Value) -> io::Result<()> {
        if !self.usage.records.contains_key(id) {
            if self.usage.is_archived(id) {
                return Ok(());
            }
            if self.pending_transcription.len() >= 128 {
                return Err(io::Error::other(
                    "transcription_settlement_capacity_exceeded",
                ));
            }
            self.pending_transcription.insert(id.into(), usage.clone());
            return Ok(());
        }
        if usage["type"] == "duration"
            && let Some(seconds) = usage["seconds"].as_f64()
        {
            let settled = self
                .usage
                .settle_duration(id, seconds)
                .map_err(io::Error::other)?;
            if settled {
                self.emit_settlement(id);
            }
            return Ok(());
        }
        self.usage.mark_unknown(id).map_err(io::Error::other)
    }
    pub fn associate(&mut self, request: &str, response: &str) -> io::Result<()> {
        self.usage
            .associate_response(request, response)
            .map_err(io::Error::other)?;
        self.save()
    }
    fn emit_settlement(&self, id: &str) {
        if let Some(fields) = self.settlement_fields(id) {
            crate::telemetry::event("usage_settled", &fields);
        }
    }
    fn settlement_fields(&self, id: &str) -> Option<serde_json::Value> {
        let record = self.usage.records.get(id)?;
        let usage = if let Some(seconds) = record.duration_seconds {
            serde_json::json!({"type":"duration","seconds":seconds})
        } else {
            serde_json::json!({"type":"tokens","tokens":record.usage})
        };
        Some(
            serde_json::json!({"response_id":id,"phase":record.phase,"model":if record.model.is_empty(){"unknown_model"}else{&record.model},"price_version":record.price_version,"usage":usage,"cost_microusd":record.settled_microusd}),
        )
    }
    pub fn receipt(&mut self, event: &serde_json::Value) -> io::Result<()> {
        // A user can interrupt after response.create was sent but before the
        // turn actor sees response.created. Read-only drains must still bind
        // late usage to the already-admitted local reservation. Metadata never
        // admits work: an unknown request cannot create a billing record.
        if matches!(
            event["type"].as_str(),
            Some("response.created" | "response.done")
        ) {
            let response = &event["response"];
            if let (Some(request), Some(id)) = (
                response["metadata"]["eva_request_id"].as_str(),
                response["id"].as_str().filter(|id| {
                    !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
                }),
            ) {
                if request != id
                    && self.usage.records.contains_key(request)
                    && !self.usage.records.contains_key(id)
                    && !self.usage.is_archived(id)
                {
                    self.associate(request, id)?;
                }
            }
        }
        if matches!(
            event["type"].as_str(),
            Some(
                "conversation.item.input_audio_transcription.completed"
                    | "conversation.item.input_audio_transcription.failed"
            )
        ) {
            let item = event["item_id"]
                .as_str()
                .ok_or_else(|| io::Error::other("transcription_item_missing"))?;
            let index = event["content_index"].as_u64().unwrap_or(0);
            let id = format!("asr:{item}:{index}");
            self.transcription_receipt(&id, &event["usage"])?;
            return self.save();
        }
        if event["type"] != "response.done" {
            return Ok(());
        }
        let id = event["response"]["id"].as_str().unwrap_or("");
        if !self.usage.records.contains_key(id) {
            return Ok(());
        }
        let rates = self.usage.records[id].rates;
        match TokenUsage::from_realtime(&event["response"]["usage"]) {
            Ok(u) => {
                if self.usage.settle(id, u, rates).map_err(io::Error::other)? {
                    self.emit_settlement(id);
                }
            }
            Err(_) => {
                self.usage.mark_unknown(id).map_err(io::Error::other)?;
            }
        }
        self.save()
    }
    pub fn unknown_pending(&mut self) -> io::Result<()> {
        for r in self.usage.records.values_mut() {
            if r.settled_microusd.is_none() {
                r.unknown = true;
            }
        }
        self.save()
    }
}
pub fn wall_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    fn async_test_store() -> (PathBuf, Arc<Mutex<Store>>) {
        let dir = [PathBuf::from("/dev/shm"), std::env::temp_dir()]
            .into_iter()
            .find_map(|base| {
                let p = base.join(format!(
                    "eva-async-store-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                private_dir(&p).ok().map(|()| p)
            })
            .unwrap();
        let store = Arc::new(Mutex::new(Store::open_at(dir.clone()).unwrap()));
        (dir, store)
    }
    #[tokio::test(flavor = "current_thread")]
    async fn slow_store_work_does_not_block_runtime_heartbeat() {
        let (dir, store) = async_test_store();
        let (started, ready) = tokio::sync::oneshot::channel();
        let job_store = store.clone();
        let job = tokio::spawn(async move {
            access(&job_store, move |storage| {
                let _ = started.send(());
                std::thread::sleep(std::time::Duration::from_millis(180));
                storage.reserve("slow-reservation", "plan", &Config::default())
            })
            .await
        });
        ready.await.unwrap();
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_millis(10));
        let mut beats = 0;
        while !job.is_finished() {
            heartbeat.tick().await;
            beats += 1;
        }
        job.await.unwrap().unwrap();
        assert!(
            beats >= 5,
            "durable bookkeeping blocked the async heartbeat"
        );
        assert!(
            store
                .lock()
                .unwrap()
                .usage
                .records
                .contains_key("slow-reservation")
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_a_store_waiter_retains_bookkeeping_but_never_admits_the_next_step() {
        let (dir, store) = async_test_store();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (finished, saved) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let admitted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (job_store, next_step) = (store.clone(), admitted.clone());
        let job = tokio::spawn(async move {
            access(&job_store, move |storage| {
                let _ = started.send(());
                wait.recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
                let result = storage.reserve("cancelled-reservation", "plan", &Config::default());
                let _ = finished.send(result.is_ok());
                result
            })
            .await
            .unwrap();
            next_step.store(true, std::sync::atomic::Ordering::Release);
        });
        ready.await.unwrap();
        job.abort();
        let _ = job.await;
        release.send(()).unwrap();
        assert!(saved.await.unwrap());
        assert!(!admitted.load(std::sync::atomic::Ordering::Acquire));
        assert!(
            access(&store, |storage| Ok(storage
                .usage
                .records
                .contains_key("cancelled-reservation")))
            .await
            .unwrap()
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn interrupted_response_can_settle_from_late_correlated_receipts() {
        let dir = std::env::temp_dir().join(format!(
            "eva-late-response-{}-{}",
            std::process::id(),
            wall_ms()
        ));
        let mut store = Store::open_at(dir.clone()).unwrap();
        let config = Config::default();
        store.reserve("pending-turn", "plan", &config).unwrap();
        store.unknown_pending().unwrap();
        let created = serde_json::json!({"type":"response.created","response":{"id":"late-response","metadata":{"eva_request_id":"pending-turn"}}});
        store.receipt(&created).unwrap();
        assert!(!store.usage.records.contains_key("pending-turn"));
        assert!(store.usage.records["late-response"].unknown);
        let done = serde_json::json!({"type":"response.done","response":{"id":"late-response","metadata":{"eva_request_id":"pending-turn"},"status":"cancelled","usage":{"input_tokens":0,"output_tokens":1,"input_token_details":{"text_tokens":0,"audio_tokens":0,"cached_tokens":0},"output_token_details":{"text_tokens":1,"audio_tokens":0}}}});
        store.receipt(&done).unwrap();
        store.receipt(&done).unwrap();
        assert_eq!(
            store.usage.records["late-response"].settled_microusd,
            Some(16)
        );
        let foreign = serde_json::json!({"type":"response.created","response":{"id":"foreign","metadata":{"eva_request_id":"unreserved"}}});
        store.receipt(&foreign).unwrap();
        assert_eq!(store.usage.records.len(), 1);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn usage_load_is_bounded_and_does_not_follow_a_replaced_file() {
        let dir = std::env::temp_dir().join(format!(
            "eva-usage-load-{}-{}",
            std::process::id(),
            wall_ms()
        ));
        private_dir(&dir).unwrap();
        let path = dir.join("usage.json");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        assert!(Store::open_at(dir.clone()).is_err());
        fs::remove_file(&path).unwrap();
        let target = dir.join("other.json");
        atomic_json(&target, &UsageMeter::default()).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(Store::open_at(dir.clone()).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn optional_asr_reserves_before_commit_and_handles_early_duplicate_usage() {
        let name = format!("eva-asr-test-{}-{}", std::process::id(), wall_ms());
        let dir = [PathBuf::from("/dev/shm"), std::env::temp_dir()]
            .into_iter()
            .find_map(|base| {
                let candidate = base.join(&name);
                private_dir(&candidate).ok().map(|()| candidate)
            })
            .expect("private test directory");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("store.lock"))
            .unwrap();
        let mut store = Store {
            dir: dir.clone(),
            ledger: Ledger::default(),
            usage: UsageMeter::default(),
            _lock: lock,
            pending_transcription: Default::default(),
        };
        let mut config = Config::default();
        config.interaction.max_utterance_ms = 20_000;
        store.reserve_transcription("disabled", &config).unwrap();
        assert!(store.usage.records.is_empty());
        config.openai.input_transcription = true;
        store.reserve_transcription("request", &config).unwrap();
        assert_eq!(store.usage.records["request"].estimate_microusd, 1500);
        let event = serde_json::json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"item-one","content_index":0,"transcript":"secret words must never persist","usage":{"type":"duration","seconds":10.0}});
        store.receipt(&event).unwrap();
        store
            .associate_transcription("request", "item-one")
            .unwrap();
        store.receipt(&event).unwrap();
        assert_eq!(
            store.usage.records["asr:item-one:0"].settled_microusd,
            Some(750)
        );
        let fields = store.settlement_fields("asr:item-one:0").unwrap();
        assert_eq!(fields["model"], "gpt-transcribe");
        assert_eq!(fields["phase"], "transcription");
        assert_eq!(fields["usage"]["type"], "duration");
        assert_eq!(fields["cost_microusd"], 750);
        assert!(!fields.to_string().contains("secret words"));
        let day = store.usage.records["asr:item-one:0"].day.clone();
        store.usage.compact(u64::MAX, true).unwrap();
        store.receipt(&event).unwrap();
        assert!(store.pending_transcription.is_empty());
        assert_eq!(store.usage.consumed_and_reserved(&day), 750);
        let persisted = std::fs::read_to_string(dir.join("usage.json")).unwrap();
        assert!(!persisted.contains("secret words"));
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn foreign_asr_is_billed_once_even_after_budget_exhaustion_and_early_receipt() {
        let name = format!("eva-foreign-asr-{}-{}", std::process::id(), wall_ms());
        let dir = [PathBuf::from("/dev/shm"), std::env::temp_dir()]
            .into_iter()
            .find_map(|base| {
                let candidate = base.join(&name);
                private_dir(&candidate).ok().map(|()| candidate)
            })
            .unwrap();
        let mut store = Store::open_at(dir.clone()).unwrap();
        let mut config = Config::default();
        config.interaction.max_utterance_ms = 20_000;
        config.openai.input_transcription = true;
        config.budget.daily_limit_usd = 0.000001;
        let event = serde_json::json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"foreign-B","content_index":0,"transcript":"foreign transcript must never persist","usage":{"type":"duration","seconds":2.0}});
        // Exercise an already received charge before the item was classified.
        store.receipt(&event).unwrap();
        store
            .register_foreign_transcription("foreign-B", &config)
            .unwrap();
        let record = &store.usage.records["asr:foreign-B:0"];
        assert_eq!(record.settled_microusd, Some(150));
        assert_eq!(record.model, "gpt-transcribe");
        let day = record.day.clone();
        store
            .register_foreign_transcription("foreign-B", &config)
            .unwrap();
        store.receipt(&event).unwrap();
        assert_eq!(store.usage.records.len(), 1);
        assert_eq!(store.usage.consumed_and_reserved(&day), 150);
        assert!(store.pending_transcription.is_empty());
        assert!(
            store.reserve("new-model-work", "plan", &config).is_err(),
            "recording a past charge must not disable future admission"
        );
        store.usage.compact(u64::MAX, true).unwrap();
        store
            .register_foreign_transcription("foreign-B", &config)
            .unwrap();
        store.receipt(&event).unwrap();
        assert_eq!(store.usage.consumed_and_reserved(&day), 150);
        assert!(store.usage.records.is_empty());
        assert!(
            !std::fs::read_to_string(dir.join("usage.json"))
                .unwrap()
                .contains("foreign transcript")
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn foreign_asr_resumes_an_existing_intent_and_disabled_asr_reserves_nothing() {
        let name = format!("eva-foreign-resume-{}-{}", std::process::id(), wall_ms());
        let dir = [PathBuf::from("/dev/shm"), std::env::temp_dir()]
            .into_iter()
            .find_map(|base| {
                let candidate = base.join(&name);
                private_dir(&candidate).ok().map(|()| candidate)
            })
            .unwrap();
        let mut store = Store::open_at(dir.clone()).unwrap();
        let mut config = Config::default();
        config.interaction.max_utterance_ms = 20_000;
        store
            .register_foreign_transcription("disabled", &config)
            .unwrap();
        assert!(store.usage.records.is_empty());
        config.openai.input_transcription = true;
        store
            .reserve_transcription("foreign-asr:item:0", &config)
            .unwrap();
        store
            .register_foreign_transcription("item", &config)
            .unwrap();
        assert_eq!(store.usage.records.len(), 1);
        assert!(!store.usage.records.contains_key("foreign-asr:item:0"));
        assert_eq!(store.usage.records["asr:item:0"].estimate_microusd, 1500);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
