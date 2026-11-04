//! Finite complete standalone-audio stop intent, only corroborating the same
//! original ordinary candidate after caller-owned BoundReview and fresh RPC.
use jwm_eva_core::actions::{Disposition, FrozenPlan, Plan, validate_plan};
use serde_json::Value;
#[derive(Debug)]
pub(super) enum Verdict {
    Unsupported,
    Reject(&'static str),
    Proved(Proof),
}
#[derive(Debug)]
pub(super) struct Proof {
    candidate: String,
    frozen: String,
    hash: String,
    frame: (u64, u64),
}
fn digest(v: &impl serde::Serialize) -> String {
    jwm_eva_core::protocol::digest(&serde_json::to_value(v).expect("typed plan"))
}
impl Proof {
    pub(super) fn accepts(&self, p: &Plan, f: &FrozenPlan, frame: (u64, u64)) -> bool {
        self.frame == frame
            && self.hash == f.plan_hash
            && self.candidate == digest(p)
            && self.frozen == digest(f)
    }
}
pub(super) fn recognizes(text: Option<&str>) -> bool {
    let Some(text) = text else {
        return false;
    };
    if text.len() > 256 || text.chars().any(char::is_control) {
        return false;
    }
    let text = text
        .trim()
        .trim_end_matches(['。', '.', '!', '！'])
        .trim()
        .to_ascii_lowercase();
    matches!(
        text.strip_prefix('请').unwrap_or(&text).trim(),
        "停止录音" | "停止独立录音" | "结束独立录音" | "stop audio recording"
    )
}
pub(super) fn assess(
    text: Option<&str>,
    p: &Plan,
    f: &FrozenPlan,
    c: &Value,
    n: &Value,
    frame: (u64, u64),
) -> Verdict {
    if !recognizes(text) {
        return Verdict::Unsupported;
    }
    if frame.0 == 0
        || frame.1 == 0
        || !super::owned_status_intent::native_frame_identity(c, n)
        || c["wm_instance_id"] != f.wm_instance_id
    {
        return Verdict::Reject("unproven_native_capture_frame");
    }
    if p.disposition != Disposition::Execute
        || f.disposition != Disposition::Execute
        || p.actions.len() != 1
        || f.actions.len() != 1
        || f.requires_confirmation
    {
        return Verdict::Reject("not_single_original_capture_control");
    }
    let a = &p.actions[0];
    let b = &f.actions[0];
    if a.name != "stop_audio_recording"
        || b.name != a.name
        || a.arguments.as_object().is_none_or(|v| !v.is_empty())
        || b.arguments != a.arguments
    {
        return Verdict::Reject(
            "standalone_stop_cannot_authorize_screen_capture_mute_or_listener_change",
        );
    }
    if !c["available_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "stop_audio_recording")
        || jwm_eva_core::actions::semantic_actions()
            .iter()
            .find(|s| s.canonical_name == "stop_audio_recording")
            .is_none_or(|s| s.target_resolver != "capture" || s.risk_class != "ordinary")
    {
        return Verdict::Reject("capture_control_capability_unavailable");
    }
    let Ok(rebuilt) = validate_plan(p, c) else {
        return Verdict::Reject("invalid_original_capture_control");
    };
    if digest(&rebuilt) != digest(f) {
        return Verdict::Reject("original_capture_control_changed");
    }
    // This is intent evidence only. No idle/saved/file-finalization success is
    // inferred: the existing native executor and recorder settlement still run.
    Verdict::Proved(Proof {
        candidate: digest(p),
        frozen: digest(f),
        hash: f.plan_hash.clone(),
        frame,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn ctx() -> Value {
        json!({"wm_instance_id":"native","wm_pid":10,"wm_start_ticks":"20","wake_generation":0,"assistant_permit":{"assistant_session_id":"private","generation":1},"session_unlocked":true,"observed_monotonic_ns":100,"tag_count":9,"available_actions":["stop_audio_recording","stop_recording","set_mic_mute"],"windows":[],"monitors":[]})
    }
    fn plan(c: &Value, name: &str, args: Value) -> (Plan, FrozenPlan) {
        let p:Plan=serde_json::from_value(json!({"disposition":"execute","actions":[{"name":name,"arguments":args}],"question":null,"reason":null})).unwrap();
        let f = validate_plan(&p, c).unwrap();
        (p, f)
    }
    #[test]
    fn complete_standalone_stop_preserves_only_existing_exact_empty_payload() {
        let c = ctx();
        let mut n = c.clone();
        n["observed_monotonic_ns"] = json!(101);
        let (p, f) = plan(&c, "stop_audio_recording", json!({}));
        let Verdict::Proved(proof) = assess(Some("停止录音"), &p, &f, &c, &n, (1, 1)) else {
            panic!("exact standalone control")
        };
        assert!(proof.accepts(&p, &f, (1, 1)));
        assert!(!proof.accepts(&p, &f, (1, 2)));
        for name in ["stop_recording", "set_mic_mute"] {
            let args = if name == "set_mic_mute" {
                json!({"muted":true})
            } else {
                json!({})
            };
            let (p, f) = plan(&c, name, args);
            assert!(matches!(
                assess(Some("停止录音"), &p, &f, &c, &n, (1, 1)),
                Verdict::Reject(_)
            ));
        }
    }
    #[test]
    fn no_listener_mute_start_question_extraeffects_or_unowned_inference() {
        for text in [
            "停止录屏",
            "停止监听",
            "把麦克风静音",
            "开始录音",
            "停止录音？",
            "停止录音好吗",
            "停止录音并退出",
            "标题写着停止录音",
        ] {
            assert!(!recognizes(Some(text)), "{text}");
        }
        assert!(!recognizes(None));
        let c = ctx();
        let mut n = c.clone();
        n["observed_monotonic_ns"] = json!(101);
        let (p, f) = plan(&c, "stop_audio_recording", json!({}));
        let mut extra = p.clone();
        extra.actions.push(p.actions[0].clone());
        assert!(matches!(
            assess(Some("停止录音"), &extra, &f, &c, &n, (1, 1)),
            Verdict::Reject(_)
        ));
        let mut changed = f.clone();
        changed.plan_hash = "changed".into();
        assert!(matches!(
            assess(Some("停止录音"), &p, &changed, &c, &n, (1, 1)),
            Verdict::Reject(_)
        ));
        n["assistant_permit"] = Value::Null;
        assert!(matches!(
            assess(Some("停止录音"), &p, &f, &c, &n, (1, 1)),
            Verdict::Reject(_)
        ));
    }
}
