//! External proof for WM shutdown and restart. Socket EOF is never evidence
//! that a process exited or that a replacement compositor became ready.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    sync::{Mutex, OnceLock},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_ticks: u64,
    pub wm_instance_id: String,
}
impl ProcessIdentity {
    pub fn from_context(context: &Value) -> Option<Self> {
        Some(Self {
            pid: context["wm_pid"].as_u64()?.try_into().ok()?,
            start_ticks: context["wm_start_ticks"]
                .as_str()
                .and_then(|value| value.parse().ok())
                .or_else(|| context["wm_start_ticks"].as_u64())?,
            wm_instance_id: context["wm_instance_id"].as_str()?.into(),
        })
    }
    pub fn verify_live(&self) -> io::Result<()> {
        if self.pid == 0 || self.pid > i32::MAX as u32 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid_wm_pid"));
        }
        let directory = format!("/proc/{}", self.pid);
        let metadata = fs::metadata(&directory)?;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "wm_process_user_mismatch",
            ));
        }
        let actual = process_start_ticks(self.pid)?;
        if actual != self.start_ticks {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "wm_process_identity_mismatch",
            ));
        }
        Ok(())
    }
}
pub fn parse_start_ticks(stat: &str) -> io::Result<u64> {
    stat.rsplit_once(')')
        .and_then(|(_, tail)| tail.split_whitespace().nth(19))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid_proc_start_ticks"))
}
pub fn process_start_ticks(pid: u32) -> io::Result<u64> {
    parse_start_ticks(&fs::read_to_string(format!("/proc/{pid}/stat"))?)
}

#[derive(Debug)]
pub struct ProcessWatch {
    identity: ProcessIdentity,
    fd: OwnedFd,
}
impl ProcessWatch {
    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            identity: self.identity.clone(),
            fd: self.fd.try_clone()?,
        })
    }
    pub fn capture(identity: ProcessIdentity) -> io::Result<Self> {
        identity.verify_live()?;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid as libc::pid_t, 0u32) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        // A second read closes PID reuse between /proc observation and open.
        identity.verify_live()?;
        Ok(Self { identity, fd })
    }
    pub fn exited(&self) -> io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, 0) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if poll.revents & (libc::POLLNVAL | libc::POLLERR) != 0 {
            return Err(io::Error::other("wm_pidfd_poll_failed"));
        }
        Ok(poll.revents & libc::POLLIN != 0)
    }
}
fn pending() -> &'static Mutex<BTreeMap<String, ProcessWatch>> {
    static PENDING: OnceLock<Mutex<BTreeMap<String, ProcessWatch>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Capture before dispatch and retain the pidfd through delayed settlement.
/// Missing identity/unsupported pidfd means unavailable evidence, never success.
pub fn prepare(operation: &str, context: &Value) -> io::Result<Value> {
    let Some(identity) = ProcessIdentity::from_context(context) else {
        return Ok(json!({"available":false,"reason":"wm_process_identity_unavailable"}));
    };
    let watch = match ProcessWatch::capture(identity.clone()) {
        Ok(watch) => watch,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => return Err(error),
        Err(_) => {
            return Ok(
                json!({"available":false,"identity":identity,"reason":"wm_pidfd_unavailable"}),
            );
        }
    };
    let mut watches = pending().lock().unwrap();
    if watches.contains_key(operation) {
        return Err(io::Error::other("lifecycle_operation_id_conflict"));
    }
    if watches.len() >= 64 {
        return Err(io::Error::other("lifecycle_capacity_exceeded"));
    }
    watches.insert(operation.into(), watch);
    Ok(json!({"available":true,"identity":identity,"old_process_exited":false}))
}
pub fn forget(operation: &str) {
    pending().lock().unwrap().remove(operation);
}

fn record_evidence(operation: &str, mut evidence: Value) -> Value {
    if let Some(watch) = pending().lock().unwrap().get(operation) {
        evidence["identity"] = json!(watch.identity);
        if watch.exited().unwrap_or(false) {
            evidence["old_process_exited"] = json!(true);
        }
    }
    evidence
}
fn replacement_ready(old: &ProcessIdentity, context: &Value, old_exited: bool) -> io::Result<bool> {
    if context["protocol_version"] != 1
        || context["wm_instance_id"]
            .as_str()
            .is_none_or(|instance| instance == old.wm_instance_id)
    {
        return Ok(false);
    }
    let Some(new) = ProcessIdentity::from_context(context) else {
        return Ok(false);
    };
    new.verify_live()?;
    Ok(
        if new.pid == old.pid && new.start_ticks == old.start_ticks {
            true
        } else {
            old_exited
        },
    )
}

/// No mutations occur here. Restart is proved by a fresh protocol-ready WM
/// instance and verified process identity; JWM normally uses same-PID exec.
pub async fn verify(operation: &str, action: &str, mut result: Value, timeout: Duration) -> Value {
    if matches!(result["status"].as_str(), Some("failed" | "cancelled")) {
        forget(operation);
        return result;
    }
    let mut evidence = result["verified"]["lifecycle"].clone();
    if !evidence.is_object() {
        evidence = json!({"available":false});
    }
    let identity: Option<ProcessIdentity> =
        serde_json::from_value(evidence["identity"].clone()).ok();
    if identity.is_none() {
        result["status"] = json!("unknown");
        result["error"] = json!("wm_lifecycle_identity_unavailable");
        result["verified"]["lifecycle"] = evidence;
        return result;
    }
    // Recover a still-live identity after a daemon restart. If it has already
    // disappeared and the original pidfd was lost, keep the outcome unknown.
    let retained = { pending().lock().unwrap().contains_key(operation) };
    if !retained
        && let Some(identity) = identity.clone()
        && let Ok(watch) = ProcessWatch::capture(identity)
    {
        let mut watches = pending().lock().unwrap();
        if watches.len() < 64 {
            watches.insert(operation.into(), watch);
        }
    }
    let local_watch = pending()
        .lock()
        .unwrap()
        .get(operation)
        .and_then(|watch| watch.try_clone().ok());
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        evidence = record_evidence(operation, evidence);
        if let Some(watch) = local_watch.as_ref()
            && watch.exited().unwrap_or(false)
        {
            evidence["old_process_exited"] = json!(true);
        }
        let exited = evidence["old_process_exited"] == true;
        if action == "quit" && exited {
            result["status"] = json!("succeeded");
            result.as_object_mut().unwrap().remove("error");
            result["verified"]["lifecycle"] = evidence;
            forget(operation);
            return result;
        }
        if action == "restart"
            && let Some(old) = identity.as_ref()
        {
            let query_deadline =
                deadline.max(tokio::time::Instant::now() + Duration::from_millis(1));
            if let Ok(Ok(context)) = tokio::time::timeout_at(
                query_deadline,
                crate::executor::query("get_execution_context", json!({})),
            )
            .await
                && replacement_ready(old, &context, exited).unwrap_or(false)
            {
                evidence["new_wm_instance_id"] = context["wm_instance_id"].clone();
                evidence["new_process_identity"] =
                    serde_json::to_value(ProcessIdentity::from_context(&context))
                        .unwrap_or(Value::Null);
                evidence["process_continued"] = json!(
                        ProcessIdentity::from_context(&context).is_some_and(|new| new.pid
                            == old.pid
                            && new.start_ticks == old.start_ticks)
                    );
                result["status"] = json!("succeeded");
                result.as_object_mut().unwrap().remove("error");
                result["verified"]["lifecycle"] = evidence;
                forget(operation);
                return result;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(
            Duration::from_millis(25)
                .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
    result["status"] = json!(if result["status"] == "accepted"
        && !evidence["old_process_exited"].as_bool().unwrap_or(false)
    {
        "accepted"
    } else {
        "unknown"
    });
    result["verified"]["lifecycle"] = evidence;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn child() -> std::process::Child {
        std::process::Command::new("/bin/cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }
    fn identity(pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            start_ticks: process_start_ticks(pid).unwrap(),
            wm_instance_id: "old-instance".into(),
        }
    }
    #[test]
    fn pidfd_proves_child_exit_and_rejects_reused_pid_identity() {
        let mut child = child();
        let id = identity(child.id());
        let watch = ProcessWatch::capture(id.clone()).unwrap();
        assert!(!watch.exited().unwrap());
        let mut wrong = id;
        wrong.start_ticks += 1;
        assert_eq!(
            ProcessWatch::capture(wrong).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        drop(child.stdin.take());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !watch.exited().unwrap() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        child.wait().unwrap();
    }
    #[tokio::test]
    async fn accepted_shutdown_cannot_succeed_while_process_is_alive() {
        let mut child = child();
        let id = identity(child.id());
        let context = json!({"wm_pid":id.pid,"wm_start_ticks":id.start_ticks.to_string(),"wm_instance_id":id.wm_instance_id});
        let operation = format!("lifecycle-test-{}", child.id());
        let evidence = prepare(&operation, &context).unwrap();
        let pending = verify(
            &operation,
            "quit",
            json!({"status":"accepted","verified":{"lifecycle":evidence}}),
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(pending["status"], "accepted");
        drop(child.stdin.take());
        child.wait().unwrap();
        let finished = verify(&operation, "quit", pending, Duration::from_millis(100)).await;
        assert_eq!(finished["status"], "succeeded");
        assert_eq!(
            finished["verified"]["lifecycle"]["old_process_exited"],
            true
        );
    }
    #[test]
    fn exec_restart_requires_new_instance_and_real_process_identity() {
        let id = identity(std::process::id());
        let mut context = json!({"protocol_version":1,"wm_pid":id.pid,"wm_start_ticks":id.start_ticks.to_string(),"wm_instance_id":"old-instance"});
        assert!(!replacement_ready(&id, &context, false).unwrap());
        context["wm_instance_id"] = json!("new-instance");
        assert!(replacement_ready(&id, &context, false).unwrap());
        context["protocol_version"] = json!(0);
        assert!(!replacement_ready(&id, &context, true).unwrap());
    }
    #[test]
    fn exec_preserves_pidfd_and_start_ticks_but_new_instance_can_prove_restart() {
        use std::io::Write;
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "read trigger; exec /bin/cat"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let old = identity(child.id());
        let watch = ProcessWatch::capture(old.clone()).unwrap();
        child.stdin.as_mut().unwrap().write_all(b"start\n").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if fs::read_link(format!("/proc/{}/exe", child.id()))
                .is_ok_and(|path| path.ends_with("cat"))
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(process_start_ticks(child.id()).unwrap(), old.start_ticks);
        assert!(!watch.exited().unwrap());
        let replacement = json!({"protocol_version":1,"wm_pid":child.id(),"wm_start_ticks":old.start_ticks.to_string(),"wm_instance_id":"instance-after-exec"});
        assert!(replacement_ready(&old, &replacement, false).unwrap());
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(watch.exited().unwrap());
    }
    #[test]
    fn stat_parser_handles_spaces_and_parentheses_in_process_names() {
        let fields = (3..=22)
            .map(|field| {
                if field == 3 {
                    "S".to_owned()
                } else if field == 22 {
                    "123456".to_owned()
                } else {
                    "0".to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            parse_start_ticks(&format!("1 (name ) with spaces) {fields}")).unwrap(),
            123456
        );
    }
}
