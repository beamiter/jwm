// Synthetic OFFLINE TEST DATA only. These scripts are derived from a static
// rubric proposal and original owned text/context, never runtime Plan/Frozen,
// native execution output or a real independent provider verdict. Mount only
// below session::tests; do not import into the product planning path.
use serde_json::{Value, json};

/// Explicit synthetic replay alternatives; first is preferred source analysis,
/// later bounded variants vary only source basis/owned quote, never operations,
/// targets, parameter values or ordering. An empty result invents no proposal.
pub(super) fn review_fixtures(case: &Value, test_proposal: &Value, context: &Value) -> Vec<Value> {
    let Some(text) = case["text"].as_str().filter(|s| !s.trim().is_empty()) else {
        return vec![];
    };
    let actions = test_proposal
        .as_array()
        .or_else(|| test_proposal["actions"].as_array());
    let Some(actions) = actions.filter(|a| !a.is_empty() && a.len() <= 32) else {
        return vec![];
    };
    let registry = jwm_eva_core::actions::semantic_actions();
    let mut steps = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        let Some(name) = action["name"].as_str() else {
            return vec![];
        };
        let Some(spec) = registry.iter().find(|s| s.canonical_name == name) else {
            return vec![];
        };
        let args = &action["arguments"];
        if !args.is_object() {
            return vec![];
        }
        let properties = &spec.parameters_schema["properties"];
        // Do not silently erase a cross-domain target from a malformed static
        // proposal to manufacture a well-formed review declaration.
        for key in [
            "window_ref",
            "monitor_ref",
            "tag_number",
            "tag_numbers",
            "session_ref",
            "application_id",
        ] {
            if args.get(key).is_some() && properties.get(key).is_none() {
                return vec![];
            }
        }
        if args.get("id").is_some()
            && (name != "set_audio_device" || properties.get("id").is_none())
        {
            return vec![];
        }
        let singleton = |key: &str| args[key].as_str().map(|s| json!([s])).unwrap_or(json!([]));
        let windows = singleton("window_ref");
        let monitors = singleton("monitor_ref");
        let tags = args["tag_numbers"].as_array().cloned().unwrap_or_else(|| {
            args["tag_number"]
                .as_u64()
                .map(|t| vec![json!(t)])
                .unwrap_or_default()
        });
        let (wb, wq) = window_source(text, context, args["window_ref"].as_str());
        let (mb, mq) = monitor_source(text, context, args["monitor_ref"].as_str(), index, actions);
        let mut row = json!({"name":name,"target_scope":spec.target_resolver,"window_refs":windows,"window_basis":wb,"window_quote":wq,"monitor_refs":monitors,"monitor_basis":mb,"monitor_quote":mq,"tag_numbers":tags,"session_refs":singleton("session_ref"),"session_quote":if args["session_ref"].is_string(){text}else{""},"device_ids":if name=="set_audio_device"{singleton("id")}else{json!([])},"device_quote":if name=="set_audio_device"{device_source(text,args["id"].as_str())}else{""},"application_ids":singleton("application_id"),"application_quote":if args["application_id"].is_string(){text}else{""},"effect_quote":effect_source(text,name,actions.len()),"media_action":null});
        if name == "media_control" {
            let Some(media) = args["action"].as_str().filter(|a| {
                [
                    "play",
                    "pause",
                    "play_pause",
                    "next",
                    "previous",
                    "stop",
                    "seek",
                ]
                .contains(a)
            }) else {
                return vec![];
            };
            row["media_action"] = json!(media);
        }
        steps.push(row);
    }
    let mut base = json!({"session_intent":"continue","steps":steps});
    if actions.iter().all(|a| a["name"] == "killclient")
        && let Some(scope) = batch_scope(text)
    {
        base["batch_close_scope"] = json!(scope);
        base["batch_close_quote"] = json!(text);
    }
    let mut alternatives = vec![base.clone()];
    // Whole-owned quotes are explicit source-test inputs, not manufactured
    // object words. Their rejection remains a meaningful pure-guard diagnosis.
    let mut whole = base.clone();
    for row in whole["steps"].as_array_mut().unwrap() {
        if !row["window_refs"].as_array().unwrap().is_empty() {
            row["window_quote"] = json!(text);
        }
        if !row["monitor_refs"].as_array().unwrap().is_empty() {
            row["monitor_quote"] = json!(text);
        }
    }
    push_unique(&mut alternatives, whole);
    for (window_basis, monitor_basis) in [
        ("current", "selected_workspace"),
        ("conversation", "conversation"),
        ("previous", "named"),
        ("named", "spatial"),
        ("prior_step", "prior_step"),
        ("none", "none"),
    ] {
        let mut variant = base.clone();
        for row in variant["steps"].as_array_mut().unwrap() {
            if !row["window_refs"].as_array().unwrap().is_empty() {
                row["window_basis"] = json!(window_basis);
                row["window_quote"] = json!(text);
            }
            if !row["monitor_refs"].as_array().unwrap().is_empty() {
                row["monitor_basis"] = json!(monitor_basis);
                row["monitor_quote"] = json!(text);
            }
        }
        push_unique(&mut alternatives, variant);
    }
    alternatives.into_iter().enumerate().map(|(n,arguments)|json!({"name":"eva_review_allow","call_id":format!("offline-review-{n}"),"arguments":arguments,"semantic_review":true})).collect()
}
fn push_unique(values: &mut Vec<Value>, value: Value) {
    if !values.contains(&value) {
        values.push(value);
    }
}
fn phrase<'a>(text: &'a str, words: &[&str]) -> Option<&'a str> {
    words
        .iter()
        .filter_map(|word| text.find(word).map(|at| &text[at..at + word.len()]))
        .max_by_key(|s| s.len())
}
fn window_source<'a>(
    text: &'a str,
    context: &Value,
    reference: Option<&str>,
) -> (&'static str, &'a str) {
    let Some(reference) = reference else {
        return ("none", "");
    };
    if let Some(window) = context["windows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|w| w["window_ref"] == reference))
    {
        // Named qualifiers in the owned text take precedence over an adjacent
        // generic demonstrative. Window titles are matching data, not commands.
        for key in ["title", "class"] {
            if let Some(name) = window[key].as_str().filter(|s| !s.is_empty())
                && let Some(at) = text.to_ascii_lowercase().find(&name.to_ascii_lowercase())
            {
                return ("named", &text[at..at + name.len()]);
            }
        }
        let aliases: &[&str] = match window["class"]
            .as_str()
            .and_then(crate::language::application_family)
        {
            Some("editor") => &["编辑器"],
            Some("browser") => &["浏览器"],
            Some("terminal") => &["终端"],
            Some("file_manager") => &["文件管理器"],
            Some("media") => &["播放器"],
            _ => &[],
        };
        if let Some(q) = phrase(text, aliases) {
            return ("named", q);
        }
    }
    if let Some(q) = phrase(
        text,
        &[
            "刚才那个窗口",
            "刚才那个",
            "刚才的窗口",
            "the window I was just using",
        ],
    ) {
        return ("previous", q);
    }
    if let Some(q) = phrase(
        text,
        &[
            "当前这个窗口",
            "这个窗口",
            "当前窗口",
            "this window",
            "this one",
            "这个",
            "当前这个",
        ],
    ) {
        return ("current", q);
    }
    if let Some(q) = phrase(text, &["把它", "它", "该窗口"]) {
        return ("conversation", q.strip_prefix('把').unwrap_or(q));
    }
    ("none", "")
}
fn device_source<'a>(text: &'a str, id: Option<&str>) -> &'a str {
    if let Some(id) = id.filter(|s| !s.is_empty())
        && let Some(at) = text.find(id)
    {
        return &text[at..at + id.len()];
    }
    phrase(
        text,
        &[
            "USB 耳机",
            "USB耳机",
            "USB 麦克风",
            "USB麦克风",
            "usb 耳机",
            "usb耳机",
            "usb 麦克风",
            "usb麦克风",
            "耳机",
            "麦克风",
            "扬声器",
        ],
    )
    .unwrap_or("")
}
fn monitor_source<'a>(
    text: &'a str,
    context: &Value,
    reference: Option<&str>,
    index: usize,
    actions: &[Value],
) -> (&'static str, &'a str) {
    let Some(reference) = reference else {
        return ("none", "");
    };
    if let Some(q) = phrase(text, &["移回来", "搬回来", "原来的屏", "回来"]) {
        return ("conversation", q);
    }
    if let Some(q) = phrase(
        text,
        &[
            "左屏",
            "左边屏幕",
            "左手那块屏",
            "右屏",
            "右边屏幕",
            "右边显示器",
            "右手那块屏幕",
            "上屏",
            "下屏",
        ],
    ) {
        let selected = if text.contains("不，是左屏") {
            phrase(text, &["左屏"]).unwrap()
        } else {
            q
        };
        return ("spatial", selected);
    }
    if let Some(m) = context["monitors"]
        .as_array()
        .and_then(|rows| rows.iter().find(|m| m["monitor_ref"] == reference))
    {
        for key in ["connector", "name"] {
            if let Some(name) = m[key].as_str().filter(|s| !s.is_empty())
                && let Some(at) = text.to_ascii_lowercase().find(&name.to_ascii_lowercase())
            {
                return ("named", &text[at..at + name.len()]);
            }
        }
    }
    if let Some(q) = phrase(text, &["主显示器", "主屏幕", "主屏", "HDMI screen", "HDMI"]) {
        return ("named", q);
    }
    if index > 0
        && actions[..index].iter().any(|a| {
            a["arguments"]["monitor_ref"] == reference || a["arguments"].get("tag_number").is_some()
        })
    {
        return ("prior_step", text);
    }
    if let Some(q) = phrase(text, &["当前屏幕", "当前屏", "这个屏幕"]) {
        return ("current", q);
    }
    if let Some(q) = phrase(
        text,
        &[
            "当前工作区",
            "当前tag",
            "当前 tag",
            "工作区",
            "workspace",
            "tag",
            "桌面",
        ],
    ) {
        return ("selected_workspace", q);
    }
    if text.contains("再") || text.contains("刚才") {
        return ("conversation", text);
    }
    ("selected_workspace", text)
}
fn effect_source<'a>(text: &'a str, name: &str, steps: usize) -> &'a str {
    let words: &[&str] = match name {
        "set_volume" | "adjust_volume" => &["音量", "声音", "volume"],
        "set_brightness" | "adjust_brightness" => &["亮度", "屏幕", "brightness", "backlight"],
        "set_mic_mute" => &["麦克风", "mic"],
        "toggle_dnd" => &["勿扰", "disturb"],
        _ => return "",
    };
    if steps > 1 {
        let matching = text
            .split(['，', ','])
            .filter(|clause| {
                let lower = clause.to_ascii_lowercase();
                words.iter().any(|word| lower.contains(word))
            })
            .collect::<Vec<_>>();
        if matching.len() == 1 {
            return matching[0];
        }
    }
    text
}
fn batch_scope(text: &str) -> Option<&'static str> {
    let t = text.trim().trim_end_matches(['。', '!', '！', '.']).trim();
    let t = t.strip_prefix('请').unwrap_or(t).trim();
    let t = t.strip_prefix("我已经确认了，直接").unwrap_or(t);
    match t {
        "关闭所有窗口" | "关掉所有窗口" | "关闭全部窗口" | "关掉全部窗口" | "close all windows" => {
            Some("all")
        }
        "关闭当前 tag 的所有窗口"
        | "关闭当前tag的所有窗口"
        | "关闭当前工作区所有窗口"
        | "关闭当前工作区的所有窗口"
        | "关闭当前工作区所有可见窗口"
        | "关闭当前工作区的所有可见窗口"
        | "关闭当前桌面的所有窗口"
        | "关闭当前桌面的所有可见窗口"
        | "close all windows on the current workspace"
        | "close all visible windows on the current workspace" => Some("current_tag_visible"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn case(text: &str) -> Value {
        json!({"text":text})
    }
    fn context() -> Value {
        json!({"windows":[{"window_ref":"w1","class":"editor","title":"Project"}],"monitors":[{"monitor_ref":"m1","connector":"DP-1","name":"Left"}]})
    }
    fn declarations(script: &Value) -> Value {
        let mut args = script["arguments"].clone();
        for row in args["steps"].as_array_mut().unwrap() {
            for key in [
                "window_basis",
                "window_quote",
                "monitor_basis",
                "monitor_quote",
            ] {
                row.as_object_mut().unwrap().remove(key);
            }
        }
        args
    }
    #[test]
    fn variants_are_explicit_synthetic_and_never_swap_names_targets_order_or_effects() {
        let input = case("把这个窗口移到第八个tag，然后切过去");
        let proposal = json!([{"name":"move_window_to_tag","arguments":{"window_ref":"w1","tag_number":8}},{"name":"switch_tag","arguments":{"monitor_ref":"m1","tag_number":8}}]);
        let before = proposal.clone();
        let scripts = review_fixtures(&input, &proposal, &context());
        assert!(!scripts.is_empty());
        assert!(scripts.len() <= 8);
        let baseline = declarations(&scripts[0]);
        for script in &scripts {
            assert_eq!(script["name"], "eva_review_allow");
            assert_eq!(script["semantic_review"], true);
            assert_eq!(script["arguments"]["session_intent"], "continue");
            assert_eq!(declarations(script), baseline);
            for row in script["arguments"]["steps"].as_array().unwrap() {
                for key in [
                    "window_quote",
                    "monitor_quote",
                    "session_quote",
                    "device_quote",
                    "application_quote",
                    "effect_quote",
                ] {
                    let quote = row[key].as_str().unwrap();
                    assert!(quote.is_empty() || input["text"].as_str().unwrap().contains(quote));
                }
            }
        }
        assert_eq!(
            scripts[0]["arguments"]["steps"][0]["window_refs"],
            json!(["w1"])
        );
        assert_eq!(
            scripts[0]["arguments"]["steps"][0]["monitor_refs"],
            json!([])
        );
        assert_eq!(
            scripts[0]["arguments"]["steps"][1]["monitor_refs"],
            json!(["m1"])
        );
        assert_eq!(proposal, before);
    }
    #[test]
    fn media_is_an_explicit_global_declaration_and_cross_domain_extra_targets_are_rejected() {
        let action =
            json!([{"name":"media_control","arguments":{"action":"pause","position_us":0}}]);
        let scripts = review_fixtures(&case("暂停音乐"), &action, &context());
        let row = &scripts[0]["arguments"]["steps"][0];
        assert_eq!(row["media_action"], "pause");
        assert_eq!(row["window_refs"], json!([]));
        assert_eq!(row["monitor_refs"], json!([]));
        let invalid = json!([{"name":"media_control","arguments":{"action":"pause","position_us":0,"window_ref":"w1"}}]);
        assert!(review_fixtures(&case("暂停音乐"), &invalid, &context()).is_empty());
        let invalid = json!([{"name":"media_control","arguments":{"action":"pause","position_us":0,"id":"usb-headset"}}]);
        assert!(review_fixtures(&case("暂停音乐"), &invalid, &context()).is_empty());
        let invalid = json!([{"name":"media_control","arguments":{"action":"pause","position_us":0,"id":null}}]);
        assert!(review_fixtures(&case("暂停音乐"), &invalid, &context()).is_empty());
        let missing = json!([{"name":"media_control","arguments":{"position_us":0}}]);
        assert!(review_fixtures(&case("暂停音乐"), &missing, &context()).is_empty());
    }
    #[test]
    fn no_raw_action_parameters_hash_or_extra_fields_leak_into_review_steps() {
        let scripts = review_fixtures(
            &case("声音输出切到USB耳机"),
            &json!([{"name":"set_audio_device","arguments":{"direction":"output","id":"usb-headset","untrusted_extra":"never copy"}}]),
            &context(),
        );
        let row = &scripts[0]["arguments"]["steps"][0];
        assert_eq!(row["device_ids"], json!(["usb-headset"]));
        assert_eq!(row["target_scope"], "device");
        let allowed = [
            "name",
            "target_scope",
            "window_refs",
            "window_basis",
            "window_quote",
            "monitor_refs",
            "monitor_basis",
            "monitor_quote",
            "tag_numbers",
            "session_refs",
            "session_quote",
            "device_ids",
            "device_quote",
            "application_ids",
            "application_quote",
            "effect_quote",
            "media_action",
        ];
        assert!(
            row.as_object()
                .unwrap()
                .keys()
                .all(|key| allowed.contains(&key.as_str()))
        );
        for key in [
            "arguments",
            "untrusted_extra",
            "direction",
            "plan_hash",
            "permit",
            "frozen_plan",
        ] {
            assert!(row.get(key).is_none());
        }
        assert!(review_fixtures(&case("闲聊"), &json!([]), &context()).is_empty());
        assert!(
            review_fixtures(
                &case(""),
                &json!([{"name":"quit","arguments":{}}]),
                &context()
            )
            .is_empty()
        );
    }
    #[test]
    fn read_only_manual_proposal_and_explicit_batch_scope_need_no_model_or_runtime_candidate() {
        let scripts = review_fixtures(
            &case("电池还有多少"),
            &json!([{"name":"query_state","arguments":{"query":"get_power_status"}}]),
            &context(),
        );
        assert_eq!(scripts.len(), 1);
        let row = &scripts[0]["arguments"]["steps"][0];
        assert_eq!(row["name"], "query_state");
        assert_eq!(row["window_refs"], json!([]));
        assert_eq!(row["device_ids"], json!([]));
        let proposal = json!([{"name":"killclient","arguments":{"window_ref":"w1"}}]);
        let scripts = review_fixtures(&case("关闭所有窗口"), &proposal, &context());
        assert_eq!(scripts[0]["arguments"]["batch_close_scope"], "all");
        assert_eq!(scripts[0]["arguments"]["batch_close_quote"], "关闭所有窗口");
        let scripts = review_fixtures(&case("他说关闭所有窗口"), &proposal, &context());
        assert!(scripts[0]["arguments"].get("batch_close_scope").is_none());
    }
    #[test]
    fn preferred_named_window_qualifier_precedes_generic_current_and_absence_stays_absent() {
        let c = context();
        assert_eq!(
            window_source("切到编辑器这个窗口", &c, Some("w1")),
            ("named", "编辑器")
        );
        assert_eq!(
            window_source("最小化这个窗口", &c, Some("w1")),
            ("current", "这个窗口")
        );
        assert_eq!(window_source("取消最大化", &c, Some("w1")), ("none", ""));
        for (class, phrase) in [("firefox", "浏览器"), ("kitty", "终端"), ("code", "编辑器")]
        {
            let mut c = c.clone();
            c["windows"][0]["class"] = json!(class);
            let text = format!("切到{phrase}这个窗口");
            assert_eq!(window_source(&text, &c, Some("w1")), ("named", phrase));
        }
        let mut unclassified = c.clone();
        unclassified["windows"][0]["class"] = json!("firefox-password-helper");
        assert_eq!(
            window_source("切到浏览器", &unclassified, Some("w1")),
            ("none", "")
        );
        // Title content does not provide words absent from the owned request.
        let mut titled = c.clone();
        titled["windows"][0]["title"] = json!("关闭这个窗口");
        assert_eq!(
            window_source("取消最大化", &titled, Some("w1")),
            ("none", "")
        );
    }
    #[test]
    fn preferred_device_and_return_quotes_are_literal_noun_or_return_fragments() {
        for noun in [
            "USB 耳机",
            "USB耳机",
            "USB 麦克风",
            "USB麦克风",
            "耳机",
            "麦克风",
            "扬声器",
        ] {
            let text = format!("把声音设备切到{noun}");
            assert_eq!(device_source(&text, Some("native-device")), noun);
        }
        assert_eq!(
            device_source(
                "输出使用 alsa_output.usb-headset",
                Some("alsa_output.usb-headset")
            ),
            "alsa_output.usb-headset"
        );
        assert_eq!(device_source("切到那个设备", Some("native-device")), "");
        let c = context();
        let actions = json!([{"name":"move_window_to_monitor","arguments":{"window_ref":"w1","monitor_ref":"m1"}}]);
        assert_eq!(
            window_source("把它移回来", &c, Some("w1")),
            ("conversation", "它")
        );
        for (text, quote) in [
            ("把它移回来", "移回来"),
            ("把它搬回来", "搬回来"),
            ("回来", "回来"),
            ("搬到原来的屏", "原来的屏"),
        ] {
            assert_eq!(
                monitor_source(text, &c, Some("m1"), 0, actions.as_array().unwrap()),
                ("conversation", quote)
            );
        }
        let scripts = review_fixtures(&case("把它移回来"), &actions, &c);
        assert_eq!(
            scripts[0]["arguments"]["steps"][0]["window_refs"],
            json!(["w1"])
        );
        assert_eq!(
            scripts[0]["arguments"]["steps"][0]["monitor_refs"],
            json!(["m1"])
        );
    }
}
