use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const LAYOUTS: &[&str] = &[
    "tile",
    "float",
    "monocle",
    "fibonacci",
    "centered_master",
    "bstack",
    "grid",
    "deck",
    "three_col",
    "tatami",
    "fullscreen",
    "scrolling",
    "vstack",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSpec {
    pub canonical_name: String,
    pub ipc_names: Vec<String>,
    pub role: String,
    pub support_status: String,
    pub description_zh: String,
    pub examples: Vec<String>,
    pub parameters_schema: Value,
    pub target_resolver: String,
    pub context_requirements: Vec<String>,
    pub backend_support: Vec<String>,
    pub execution_adapter: String,
    pub completion_contract: String,
    pub risk_class: String,
    pub retry_policy: String,
    pub undo_strategy: String,
    pub capability_version: u32,
    pub privacy_fields: Vec<String>,
    pub test_cases: Vec<String>,
}

fn object(fields: &[(&str, Value)]) -> Value {
    json!({"type":"object","properties":fields.iter().map(|(k,v)| (k.to_string(),v.clone())).collect::<serde_json::Map<_,_>>(),"required":fields.iter().map(|(k,_)| *k).collect::<Vec<_>>(),"additionalProperties":false})
}
fn reference() -> Value {
    json!({"type":"string","minLength":1,"maxLength":32})
}
fn tag() -> Value {
    json!({"type":"integer","minimum":1,"maximum":32})
}
// Table constructor keeps each reviewed capability together at the call site.
#[allow(clippy::too_many_arguments)]
fn spec(
    name: &str,
    aliases: &[&str],
    description: &str,
    fields: &[(&str, Value)],
    target: &str,
    completion: &str,
    risk: &str,
    undo: &str,
) -> ActionSpec {
    ActionSpec {
        canonical_name: name.into(),
        ipc_names: aliases.iter().map(|s| s.to_string()).collect(),
        role: "user".into(),
        support_status: "implemented".into(),
        description_zh: description.into(),
        examples: vec![description.into()],
        parameters_schema: object(fields),
        target_resolver: target.into(),
        context_requirements: vec!["session_unlocked".into(), "current_assistant_permit".into()],
        backend_support: vec!["x11".into(), "wayland".into()],
        execution_adapter: "guarded_execute_v1".into(),
        completion_contract: completion.into(),
        risk_class: risk.into(),
        retry_policy: "lookup_operation_before_any_retry".into(),
        undo_strategy: undo.into(),
        capability_version: 1,
        privacy_fields: vec!["window_title".into()],
        test_cases: vec![
            "strict_schema".into(),
            "stale_token_rejected".into(),
            "cancelled_permit_rejected".into(),
        ],
    }
}

/// Only these bounded, validated semantic operations are offered to the model.
/// Catalog registration is deliberately distinct from an implemented adapter.
pub fn semantic_actions() -> Vec<ActionSpec> {
    let mut actions = vec![
        spec(
            "switch_tag",
            &["view"],
            "在指定显示器切换到指定编号工作区",
            &[("monitor_ref", reference()), ("tag_number", tag())],
            "monitor",
            "visible_tag_mask_equals_target",
            "reversible",
            "restore_visible_tag_mask",
        ),
        spec(
            "set_layout",
            &["setlayout", "set_layout"],
            "设置指定显示器工作区的布局",
            &[
                ("monitor_ref", reference()),
                ("tag_number", tag()),
                ("layout", json!({"type":"string","enum":LAYOUTS})),
            ],
            "workspace",
            "workspace_layout_equals_target",
            "reversible",
            "restore_layout_if_poststate_unchanged",
        ),
        spec(
            "move_window_to_tag",
            &["tag"],
            "将同一窗口实例移至工作区",
            &[("window_ref", reference()), ("tag_number", tag())],
            "window",
            "same_window_tag_mask_equals_target",
            "reversible",
            "restore_tags_if_poststate_unchanged",
        ),
        spec(
            "focus_window",
            &["focus_window", "fwin"],
            "聚焦窗口，必要时切换到窗口所在工作区",
            &[("window_ref", reference())],
            "window",
            "focus_token_equals_target",
            "reversible",
            "none",
        ),
        spec(
            "move_window_to_monitor",
            &["move_window_to_monitor"],
            "将指定窗口移到指定显示器",
            &[("window_ref", reference()), ("monitor_ref", reference())],
            "window_and_monitor",
            "same_window_monitor_equals_target",
            "reversible",
            "restore_monitor_if_poststate_unchanged",
        ),
        spec(
            "focus_monitor",
            &["focusmon", "focus_mon"],
            "聚焦指定显示器",
            &[("monitor_ref", reference())],
            "monitor",
            "selected_monitor_equals_target",
            "reversible",
            "none",
        ),
        spec(
            "set_window_minimized",
            &["minimize", "minimize_window"],
            "设置指定窗口最小化状态",
            &[
                ("window_ref", reference()),
                ("minimized", json!({"type":"boolean"})),
            ],
            "window",
            "same_window_minimized_equals_target",
            "reversible",
            "restore_minimized_if_poststate_unchanged",
        ),
    ];
    actions.extend(extended_actions());
    for action in &mut actions {
        action.undo_strategy = match action.canonical_name.as_str() {
            "switch_tags" | "toggle_tags" => "restore_visible_tag_mask",
            "set_layout_at" => "restore_layout_if_poststate_unchanged",
            "move_window_to_tags" | "toggle_window_tags" => "restore_tags_if_poststate_unchanged",
            "focus_window" => "restore_focus_if_old_window_exists_and_poststate_unchanged",
            "focus_monitor" => "restore_selected_monitor_if_poststate_unchanged",
            "set_window_maximized" | "togglemaximize" => {
                "restore_full_maximize_only_if_both_axes_known_equal_and_poststate_unchanged"
            }
            "set_window_floating"
            | "set_window_sticky"
            | "set_window_pip"
            | "togglefloating"
            | "togglesticky"
            | "togglepip" => "restore_window_flag_if_poststate_unchanged",
            _ => continue,
        }
        .into();
    }
    actions
}

fn extended_actions() -> Vec<ActionSpec> {
    let catalog = ipc_catalog();
    let mut result = Vec::new();
    let mut add = |name: &str,
                   description: &str,
                   fields: Vec<(&str, Value)>,
                   scope: &str,
                   completion: &str,
                   risk: &str| {
        let aliases: Vec<_> = catalog
            .iter()
            .filter(|e| e.kind == "command" && e.canonical_name == name)
            .map(|e| e.name.as_str())
            .collect();
        result.push(spec(
            name,
            &aliases,
            description,
            &fields,
            scope,
            completion,
            risk,
            "none",
        ));
    };
    let monitor_scope = || vec![("monitor_ref", reference()), ("tag_number", tag())];
    let window_scope = || {
        vec![
            ("window_ref", reference()),
            ("monitor_ref", reference()),
            ("tag_number", tag()),
        ]
    };
    // Legacy focus-dependent actions have an explicit source scope. JWM must
    // compare that scope without silently focusing a different object first.
    for (name, description) in [
        ("zoom", "将窗口提升为主窗口"),
        ("togglefloating", "切换窗口浮动状态"),
        ("togglesticky", "切换窗口跨工作区显示"),
        ("togglepip", "切换窗口画中画"),
        ("togglemaximize", "切换窗口最大化"),
        ("killclient", "关闭指定窗口"),
    ] {
        add(
            name,
            description,
            window_scope(),
            "selected_window",
            "query:get_windows",
            if name == "killclient" {
                "confirmation"
            } else {
                "reversible"
            },
        );
    }
    for (name, description) in [
        ("set_window_floating", "设置指定窗口浮动状态"),
        ("set_window_maximized", "设置指定窗口最大化状态"),
        ("set_window_sticky", "设置指定窗口跨工作区显示"),
        ("set_window_pip", "设置指定窗口画中画"),
    ] {
        add(
            name,
            description,
            vec![
                ("window_ref", reference()),
                ("enabled", json!({"type":"boolean"})),
            ],
            "window",
            "query:get_windows",
            "reversible",
        );
    }
    for (name, description) in [
        ("focusstack", "沿堆叠顺序聚焦窗口"),
        ("movestack", "沿堆叠顺序移动窗口"),
        ("scrolling_focus_column", "聚焦相邻滚动列"),
        ("scrolling_move_column", "移动滚动列"),
        ("scrolling_focus_window", "聚焦列内窗口"),
        ("scrolling_consume", "窗口并入相邻滚动列"),
        ("scrolling_expel", "窗口从滚动列移出"),
    ] {
        let mut fields = window_scope();
        fields.push(("value", json!({"type":"integer","enum":[-1,1]})));
        add(
            name,
            description,
            fields,
            "selected_window",
            "query:get_windows",
            "reversible",
        );
    }
    for (name, description, minimum, maximum) in [
        ("setmfact", "设置主区域绝对比例（0.05到0.95）", 0.05, 0.95),
        (
            "setcfact",
            "增减窗口布局比例，value为增量，0重置；滚动布局中改为列聚焦",
            -3.75,
            3.75,
        ),
        (
            "resize_horizontal",
            "按工作区宽度比例增量调整窗口水平尺寸，value=0.05表示工作区宽度的5%，不是像素",
            -1.0,
            1.0,
        ),
        (
            "resize_vertical",
            "按工作区高度比例增量调整窗口垂直尺寸，value=0.05表示工作区高度的5%，不是像素",
            -1.0,
            1.0,
        ),
    ] {
        let mut fields = if name == "setmfact" {
            monitor_scope()
        } else {
            window_scope()
        };
        fields.push((
            "value",
            json!({"type":"number","minimum":minimum,"maximum":maximum}),
        ));
        add(
            name,
            description,
            fields,
            "workspace",
            "query:get_workspaces",
            "reversible",
        );
    }
    for (name, description, min, max) in [
        ("setnmaster", "设置主区域窗口数量", 0, 64),
        ("incnmaster", "增减主区域窗口数量", -64, 64),
        ("setgaps", "设置绝对窗口间隙像素", 0, 256),
        ("cyclelayout", "循环布局", -1, 1),
        ("cycle_overview", "循环总览界面", -1, 1),
        ("loopview", "循环切换工作区", -1, 1),
    ] {
        let mut fields = monitor_scope();
        fields.push((
            "value",
            json!({"type":"integer","minimum":min,"maximum":max}),
        ));
        add(
            name,
            description,
            fields,
            "workspace",
            "query:get_workspaces",
            "reversible",
        );
    }
    for (name, description) in [
        ("reset_layout_size", "重置布局尺寸"),
        ("lastlayout", "恢复上一个布局"),
        ("scrolling_toggle_attach_mode", "切换滚动列附着模式"),
    ] {
        add(
            name,
            description,
            monitor_scope(),
            "workspace",
            "query:get_workspaces",
            "reversible",
        );
    }
    add(
        "snap_window",
        "将窗口吸附到指定区域",
        vec![
            ("window_ref", reference()),
            ("monitor_ref", reference()),
            ("tag_number", tag()),
            (
                "direction",
                json!({"type":"string","enum":["left","right","maximize","top-left","top-right","bottom-left","bottom-right"]}),
            ),
        ],
        "selected_window",
        "query:get_windows",
        "reversible",
    );
    for (name, description) in [
        ("switch_tags", "显示指定工作区集合"),
        ("toggle_tags", "切换指定工作区的显示"),
        ("move_window_to_tags", "将窗口放到指定工作区集合"),
        ("toggle_window_tags", "切换窗口在指定工作区的归属"),
    ] {
        let fields = vec![
            (
                if name == "move_window_to_tags" || name == "toggle_window_tags" {
                    "window_ref"
                } else {
                    "monitor_ref"
                },
                reference(),
            ),
            (
                "tag_numbers",
                json!({"type":"array","minItems":1,"maxItems":32,"uniqueItems":true,"items":tag()}),
            ),
        ];
        add(
            name,
            description,
            fields,
            "workspace",
            "query:get_workspaces",
            "reversible",
        );
    }
    for (name, description) in [
        ("focus_none", "取消窗口聚焦"),
        ("refocus", "重新聚焦可见窗口"),
        ("toggle_spawn_placement", "切换新窗口放置模式"),
    ] {
        add(
            name,
            description,
            monitor_scope(),
            "workspace",
            "query:get_windows",
            "reversible",
        );
    }
    for (name, description) in [
        ("focusmon", "聚焦相邻显示器"),
        ("tagmon", "将当前窗口移到相邻显示器"),
        ("swapmon", "交换相邻显示器的窗口集合"),
    ] {
        let mut fields = window_scope();
        fields.push(("value", json!({"type":"integer","enum":[-1,1]})));
        add(
            name,
            description,
            fields,
            "selected_window",
            "query:get_monitors",
            "reversible",
        );
    }
    add(
        "focus_tab",
        "聚焦窗口标签",
        vec![
            ("window_ref", reference()),
            ("monitor_ref", reference()),
            ("tag_number", tag()),
            (
                "cmd",
                json!({"type":"array","minItems":2,"maxItems":2,"items":{"type":"string","minLength":1,"maxLength":12}}),
            ),
        ],
        "selected_window",
        "query:get_windows",
        "reversible",
    );
    add(
        "togglescratchpad",
        "显示或隐藏已配置的暂存窗口",
        vec![(
            "cmd",
            json!({"type":"array","minItems":1,"maxItems":1,"items":{"type":"string","minLength":1,"maxLength":64}}),
        )],
        "workspace",
        "query:get_scratchpads",
        "reversible",
    );
    for (name, description, query) in [
        ("togglebar", "切换状态栏", "get_bar"),
        (
            "toggle_desktop_shell_mode",
            "切换桌面外壳显示模式",
            "get_desktop_shell",
        ),
        ("toggle_dnd", "切换勿扰模式", "get_dnd"),
        ("toggle_idle_inhibit", "切换阻止自动休眠", "get_idle"),
        ("toggle_night_light", "切换夜间色温", "get_night_light"),
        ("toggle_wifi", "切换无线网络", "get_network"),
        ("toggle_bluetooth", "切换蓝牙", "get_bluetooth"),
        ("togglecompositor", "切换合成器", "get_status"),
        ("togglepartialdamage", "切换局部重绘", "get_status"),
        ("toggle_waterlily", "切换桌面效果", "get_waterlily"),
        ("toggle_overview", "显示或关闭窗口总览", "get_system_ui"),
        (
            "toggle_tags_overview",
            "显示或关闭工作区总览",
            "get_system_ui",
        ),
        ("toggle_expose", "显示或关闭窗口平铺预览", "get_system_ui"),
        ("toggle_peek", "显示或关闭桌面速览", "get_peek"),
        ("toggle_magnifier", "显示或关闭放大镜", "get_magnifier"),
        ("toggle_annotation", "显示或关闭屏幕标注", "get_system_ui"),
        (
            "toggle_shortcut_passthrough",
            "切换快捷键透传",
            "get_input_status",
        ),
    ] {
        add(
            name,
            description,
            monitor_scope(),
            "workspace",
            &format!("query:{query}"),
            "reversible",
        );
    }
    for (name, description) in [
        ("app_launcher", "打开应用启动器"),
        ("window_switcher", "打开窗口切换器"),
        ("restore_preview", "打开会话恢复预览"),
        ("control_center", "打开控制中心"),
        ("calendar", "打开日历"),
        ("notification_center", "打开通知中心"),
        ("audio_output_picker", "打开音频输出选择器"),
        ("audio_input_picker", "打开麦克风选择器"),
        ("bluetooth_picker", "打开蓝牙选择器"),
        ("wifi_picker", "打开无线网络选择器"),
        ("wallpaper_picker", "打开壁纸选择器"),
        ("layout_picker", "打开布局选择器"),
        ("clipboard_picker", "打开剪贴板选择器"),
        ("session_menu", "打开会话菜单"),
        ("music_player", "打开音乐播放器"),
        ("video_player", "打开视频播放器"),
        ("monitor_layout", "打开显示器布局界面"),
    ] {
        add(
            name,
            description,
            Vec::new(),
            "desktop",
            "query:get_system_ui",
            "reversible",
        );
    }
    for (name, description) in [
        (
            "media_play_pause",
            "切换媒体播放/暂停；明确暂停用media_control/pause，继续播放用media_control/play",
        ),
        ("media_next", "播放下一项媒体"),
        ("media_previous", "播放上一项媒体"),
        ("media_stop", "停止媒体播放"),
    ] {
        add(
            name,
            description,
            Vec::new(),
            "media",
            "query:get_media",
            "ordinary",
        );
    }
    add(
        "media_control",
        "控制当前播放器；play明确播放，pause明确暂停且重复设置不翻转；play_pause仅用于用户明确要求切换状态",
        vec![
            (
                "action",
                json!({"type":"string","enum":["play","pause","play_pause","next","previous","stop","seek"]}),
            ),
            (
                "position_us",
                json!({"type":"integer","minimum":0,"maximum":86400000000i64}),
            ),
        ],
        "media",
        "query:get_media",
        "ordinary",
    );
    add(
        "open_media",
        "打开本地媒体资源",
        vec![
            (
                "path",
                json!({"type":"string","minLength":1,"maxLength":4096}),
            ),
            (
                "mode",
                json!({"type":"string","enum":["auto","music","video"]}),
            ),
            ("enqueue", json!({"type":"boolean"})),
        ],
        "media",
        "query:get_media",
        "ordinary",
    );
    add(
        "set_mic_mute",
        "设置麦克风静音",
        vec![("muted", json!({"type":"boolean"}))],
        "device",
        "query:get_mic_mute",
        "reversible",
    );
    add(
        "set_audio_device",
        "设置音频输入或输出设备",
        vec![
            (
                "direction",
                json!({"type":"string","enum":["input","output"]}),
            ),
            ("id", json!({"type":"string","minLength":1,"maxLength":512})),
        ],
        "device",
        "query:get_audio_devices",
        "ordinary",
    );
    add(
        "set_power_profile",
        "设置电源模式",
        vec![(
            "profile",
            json!({"type":"string","enum":["power-saver","balanced","performance"]}),
        )],
        "device",
        "query:get_power",
        "ordinary",
    );
    for (name, description) in [
        ("take_screenshot", "交互式选择截图区域"),
        ("take_screenshot_fullscreen", "全屏截图"),
        ("adjust_recording_region", "调整录屏区域"),
        ("toggle_recording", "切换屏幕录制"),
        ("toggle_audio_recording", "切换音频录制"),
        ("stop_recording", "停止屏幕录制"),
        ("stop_audio_recording", "停止音频录制"),
    ] {
        add(
            name,
            description,
            Vec::new(),
            "capture",
            if name.contains("audio") {
                "query:get_audio_recording"
            } else {
                "query:get_recording"
            },
            "ordinary",
        );
    }
    for (name, description) in [
        ("start_recording", "开始屏幕录制"),
        ("start_audio_recording", "开始音频录制"),
    ] {
        add(
            name,
            description,
            vec![(
                "path",
                json!({"type":"string","minLength":1,"maxLength":4096}),
            )],
            "capture",
            if name.contains("audio") {
                "query:get_audio_recording"
            } else {
                "query:get_recording"
            },
            "ordinary",
        );
    }
    add(
        "set_recording_region",
        "设置屏幕录制区域",
        vec![
            (
                "x",
                json!({"type":"integer","minimum":-65536,"maximum":65536}),
            ),
            (
                "y",
                json!({"type":"integer","minimum":-65536,"maximum":65536}),
            ),
            (
                "width",
                json!({"type":"integer","minimum":1,"maximum":65536}),
            ),
            (
                "height",
                json!({"type":"integer","minimum":1,"maximum":65536}),
            ),
        ],
        "capture",
        "query:get_recording",
        "ordinary",
    );
    for (name, description) in [
        ("clear_clipboard", "清空剪贴板历史"),
        ("clear_notifications", "清空通知历史"),
        ("reload_config", "重新加载配置"),
        ("save_session", "保存桌面会话"),
        ("restore_session", "恢复保存的桌面会话"),
        ("quit", "退出JWM会话"),
        ("restart", "重启JWM会话"),
        ("lock_screen", "锁定整个会话"),
    ] {
        add(
            name,
            description,
            Vec::new(),
            "desktop",
            "query:get_status",
            if matches!(
                name,
                "restore_session" | "quit" | "restart" | "reload_config" | "clear_clipboard"
            ) {
                "confirmation"
            } else {
                "ordinary"
            },
        );
    }
    add(
        "lock_monitor",
        "锁定指定显示器",
        vec![("monitor_ref", reference())],
        "monitor",
        "query:get_monitors",
        "ordinary",
    );
    add(
        "unlock_monitor",
        "请求显示器本地解锁界面（不能认证或绕过密码）",
        vec![("monitor_ref", reference())],
        "monitor",
        "query:get_system_ui",
        "ordinary",
    );
    add(
        "close_notification",
        "关闭指定通知",
        vec![(
            "id",
            json!({"type":"integer","minimum":1,"maximum":4294967295u64}),
        )],
        "notification",
        "query:get_notifications",
        "ordinary",
    );
    add(
        "clipboard_copy",
        "将剪贴板历史中的指定序号条目设为当前内容（0是最新）",
        vec![(
            "index",
            json!({"type":"integer","minimum":0,"maximum":1023}),
        )],
        "clipboard",
        "query:get_clipboard",
        "ordinary",
    );
    for (name, description) in [
        ("waterlily_case", "选择桌面效果方案"),
        ("waterlily_palette", "选择桌面效果配色"),
    ] {
        add(
            name,
            description,
            vec![(
                "cmd",
                json!({"type":"array","minItems":1,"maxItems":1,"items":{"type":"string","minLength":1,"maxLength":64}}),
            )],
            "desktop",
            "query:get_waterlily",
            "reversible",
        );
    }
    add(
        "set_config",
        "设置经过JWM验证的单项配置",
        vec![
            (
                "key",
                json!({"type":"string","minLength":1,"maxLength":128}),
            ),
            (
                "value",
                json!({"anyOf":[{"type":"boolean"},{"type":"number"},{"type":"string","maxLength":4096}]}),
            ),
        ],
        "config",
        "query:get_config",
        "confirmation",
    );
    add(
        "set_config_batch",
        "批量设置经过JWM验证的配置",
        vec![(
            "changes",
            json!({"type":"array","minItems":1,"maxItems":8,"items":object(&[("key",json!({"type":"string","minLength":1,"maxLength":128})),("value",json!({"anyOf":[{"type":"boolean"},{"type":"number"},{"type":"string","maxLength":4096}]}))])}),
        )],
        "config",
        "query:get_config",
        "confirmation",
    );
    add(
        "benchmark",
        "运行有界桌面性能测量",
        vec![
            (
                "frames",
                json!({"type":"integer","minimum":1,"maximum":10000}),
            ),
            (
                "warmup",
                json!({"type":"integer","minimum":0,"maximum":2000}),
            ),
        ],
        "diagnostic",
        "query:benchmark_report",
        "confirmation",
    );
    add(
        "query_window",
        "读取指定窗口状态",
        vec![("window_ref", reference())],
        "read_only",
        "synchronous_read",
        "ordinary",
    );
    let mut column_fields = window_scope();
    column_fields.push((
        "delta",
        json!({"type":"number","minimum":-2.25,"maximum":2.25}),
    ));
    add(
        "adjust_scrolling_column_width",
        "在scrolling布局增减指定窗口所在列的宽度系数，delta为增量，0重置",
        column_fields,
        "selected_window",
        "query:get_scrolling_status",
        "reversible",
    );
    let mut factor_fields = window_scope();
    factor_fields.push((
        "factor",
        json!({"type":"number","minimum":0.25,"maximum":4.0}),
    ));
    add(
        "set_window_factor",
        "在非scrolling布局设置指定窗口的绝对布局比例",
        factor_fields,
        "selected_window",
        "query:get_windows",
        "reversible",
    );
    add(
        "undo_last_plan",
        "撤销最近的修改计划；该计划必须可补偿，不能跨过启动、关窗或配置等不可逆操作；必须单独提交",
        Vec::new(),
        "local_undo",
        "compensation_verified_or_explicit_partial_unknown",
        "ordinary",
    );
    let queries: Vec<_> = catalog
        .iter()
        .filter(|e| {
            e.kind == "query" && e.role != "bridge_internal" && e.canonical_name != "get_window"
        })
        .map(|e| e.canonical_name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    add(
        "query_state",
        "查询桌面状态（读取，不执行窗口聚焦）",
        vec![("query", json!({"type":"string","enum":queries}))],
        "read_only",
        "synchronous_read",
        "ordinary",
    );
    add(
        "launch_application",
        "启动本地已登记的应用ID",
        vec![(
            "application_id",
            json!({"type":"string","minLength":1,"maxLength":128}),
        )],
        "application",
        "process_or_window_observation",
        "ordinary",
    );
    // Several semantic forms share a legacy handler, but remain distinct
    // capabilities: replacing a tag set and toggling membership are not aliases.
    for item in &mut result {
        let ensure_panel = matches!(
            item.canonical_name.as_str(),
            "app_launcher"
                | "window_switcher"
                | "restore_preview"
                | "control_center"
                | "calendar"
                | "notification_center"
                | "audio_output_picker"
                | "audio_input_picker"
                | "bluetooth_picker"
                | "wifi_picker"
                | "wallpaper_picker"
                | "layout_picker"
                | "clipboard_picker"
                | "session_menu"
                | "music_player"
                | "video_player"
                | "monitor_layout"
        );
        let ensure_toggle = matches!(
            item.canonical_name.as_str(),
            "togglebar"
                | "toggle_desktop_shell_mode"
                | "toggle_dnd"
                | "toggle_idle_inhibit"
                | "toggle_night_light"
                | "toggle_wifi"
                | "toggle_bluetooth"
                | "togglecompositor"
                | "togglepartialdamage"
                | "toggle_waterlily"
                | "toggle_overview"
                | "toggle_tags_overview"
                | "toggle_expose"
                | "toggle_peek"
                | "toggle_magnifier"
                | "toggle_annotation"
                | "toggle_shortcut_passthrough"
        );
        if ensure_panel || ensure_toggle {
            let mut fields = if item.canonical_name == "togglebar" {
                monitor_scope()
            } else if item.canonical_name == "toggle_shortcut_passthrough" {
                window_scope()
            } else {
                vec![]
            };
            fields.push(("enabled", json!({"type":"boolean"})));
            item.parameters_schema = object(&fields);
            item.description_zh = if item.canonical_name == "toggle_desktop_shell_mode" {
                "设置桌面侧边栏呈现模式：enabled=true固定常显，false边缘悬停展开；保留已禁用边缘"
                    .into()
            } else {
                format!(
                    "设置明确目标状态(enabled=true显示或启用，false关闭)：{}；重复设置不切换",
                    item.description_zh
                )
            };
            item.examples = vec![item.description_zh.clone()];
        }
        let aliases: &[&str] = match item.canonical_name.as_str() {
            "launch_application" => &["spawn"],
            "query_window" => &["get_window", "get_win"],
            "toggle_tags" => &["toggleview", "toggle_view"],
            "toggle_window_tags" => &["toggletag", "toggle_tag"],
            _ => &[],
        };
        if !aliases.is_empty() {
            item.ipc_names = aliases.iter().map(|s| s.to_string()).collect();
        }
        match item.canonical_name.as_str() {
            "open_media" => {
                item.completion_contract = "mpris_resource_or_queue_observed".into();
                item.description_zh.push_str("；enqueue=true只承诺在播放器实际队列中加入资源，只有指定资源的Playing读回才能确认正在播放，进程启动不等于播放完成");
            }
            "setmfact" | "setcfact" | "set_window_factor" => {
                item.context_requirements
                    .push("layout_not_scrolling".into());
                item.description_zh.push_str("；scrolling布局请使用adjust_scrolling_column_width或scrolling_focus_column，不可隐式改变焦点");
                if item.canonical_name == "setcfact" {
                    item.completion_contract = "query:get_windows".into();
                }
            }
            "adjust_scrolling_column_width" => {
                item.context_requirements.push("layout_scrolling".into())
            }
            "focus_tab" => {
                item.parameters_schema = object(&[
                    ("monitor_ref", reference()),
                    (
                        "tab_number",
                        json!({"type":"integer","minimum":1,"maximum":1024}),
                    ),
                ]);
                item.target_resolver = "monitor_tab_number".into();
            }
            "focus_none" | "refocus" | "toggle_spawn_placement" => {
                item.parameters_schema = object(&window_scope());
                item.target_resolver = "selected_window".into();
                item.completion_contract = if item.canonical_name == "toggle_spawn_placement" {
                    "query:get_closed_placement"
                } else {
                    "query:get_windows"
                }
                .into();
            }
            "resize_horizontal" | "resize_vertical" | "reset_layout_size" => {
                item.completion_contract = "query:get_windows".into()
            }
            name if name.starts_with("scrolling_") => {
                item.completion_contract = "query:get_scrolling_status".into()
            }
            "cycle_overview" | "toggle_overview" | "toggle_expose" | "toggle_annotation" => {
                item.completion_contract = "query:get_effect_status".into()
            }
            "togglepartialdamage" | "togglecompositor" => {
                item.completion_contract = "query:get_effect_status".into()
            }
            "take_screenshot" | "take_screenshot_fullscreen" => {
                item.completion_contract = "query:get_capture_status".into()
            }
            _ => {}
        }
        if matches!(
            item.canonical_name.as_str(),
            "toggle_recording"
                | "toggle_audio_recording"
                | "togglefloating"
                | "togglesticky"
                | "togglepip"
                | "togglemaximize"
                | "scrolling_toggle_attach_mode"
                | "toggle_spawn_placement"
        ) {
            item.parameters_schema["properties"]["enabled"] = json!({"type":"boolean"});
            item.parameters_schema["required"]
                .as_array_mut()
                .unwrap()
                .push(json!("enabled"));
            item.description_zh = format!(
                "设置明确目标状态enabled=true启用、false关闭：{}；重复设置不切换",
                item.description_zh
            );
        }
    }
    // Typed spec is implemented here; availability is negotiated with the
    // running server, and accepted asynchronous work is never called succeeded.
    result
}

pub fn tool_schema() -> Value {
    tool_schema_for(&semantic_actions())
}
pub fn tool_schema_for(specs: &[ActionSpec]) -> Value {
    let branches:Vec<_> = specs.iter().filter(|s|s.support_status=="implemented" && s.role=="user").map(|s| json!({"type":"object","description":s.description_zh,"properties":{"name":{"const":s.canonical_name},"arguments":s.parameters_schema},"required":["name","arguments"],"additionalProperties":false})).collect();
    json!({"type":"function","name":"eva_submit_plan","description":"提交完整桌面操作计划。只能引用本轮上下文里的对象；不明确则澄清，否定或取消则不操作。结果由本地执行核验。","parameters":{"type":"object","properties":{"disposition":{"type":"string","enum":["execute","clarify","noop"]},"actions":{"type":"array","maxItems":8,"items":{"oneOf":branches}},"question":{"type":["string","null"],"maxLength":160},"reason":{"type":["string","null"],"maxLength":160}},"required":["disposition","actions","question","reason"],"additionalProperties":false}})
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub canonical_name: String,
    pub kind: String,
    pub role: String,
}
pub fn ipc_catalog() -> Vec<CatalogEntry> {
    let mut catalog: Vec<CatalogEntry> =
        serde_json::from_str(include_str!("ipc_catalog.json")).expect("reviewed IPC catalog");
    for (name, kind) in [
        ("eva_toggle", "command"),
        ("assistant_control", "command"),
        ("set_assistant_status", "command"),
        ("guarded_execute", "command"),
        ("get_execution_context", "query"),
        ("get_execution_status", "query"),
        ("get_command_schema", "query"),
        ("get_assistant_status", "query"),
        ("assistant", "subscription"),
    ] {
        if !catalog.iter().any(|e| e.name == name && e.kind == kind) {
            catalog.push(CatalogEntry {
                name: name.into(),
                canonical_name: name.into(),
                kind: kind.into(),
                role: "bridge_internal".into(),
            });
        }
    }
    catalog
}

/// Enumerates every canonical capability, retaining the per-query denominator.
pub fn full_registry() -> Vec<ActionSpec> {
    let implemented = semantic_actions();
    let mut grouped: BTreeMap<(String, String), Vec<CatalogEntry>> = BTreeMap::new();
    for entry in ipc_catalog() {
        grouped
            .entry((entry.kind.clone(), entry.canonical_name.clone()))
            .or_default()
            .push(entry);
    }
    let mut result = implemented.clone();
    for ((kind, name), entries) in grouped {
        if implemented
            .iter()
            .any(|s| entries.iter().any(|e| s.ipc_names.contains(&e.name)))
        {
            continue;
        }
        let role = entries[0].role.clone();
        let internal = role == "bridge_internal";
        let readonly = kind == "query";
        let compiled_plan = name == "command_batch";
        let adapted = internal || readonly || compiled_plan;
        let private = name == "get_clipboard";
        let schema = if readonly {
            object(&[("query", json!({"type":"string","enum":[name]}))])
        } else if compiled_plan {
            object(&[(
                "actions",
                tool_schema()["parameters"]["properties"]["actions"].clone(),
            )])
        } else {
            object(&[])
        };
        result.push(ActionSpec {
            canonical_name: name,
            ipc_names: entries.into_iter().map(|e| e.name).collect(),
            role,
            support_status: if adapted {
                "implemented"
            } else {
                "unimplemented"
            }
            .into(),
            description_zh: if internal {
                "JWM/桥接内部工作流，不允许普通语音直接写入"
            } else if readonly {
                "只读状态查询，经固定查询适配器返回；不执行窗口聚焦"
            } else if compiled_plan {
                "编译为最多八个已冻结语义步骤，每步独立许可校验、执行和核验"
            } else {
                "已登记，语义适配与完成核验尚待实施"
            }
            .into(),
            examples: vec![],
            parameters_schema: schema,
            target_resolver: "not_exposed".into(),
            context_requirements: if private {
                vec!["privacy.include_clipboard_in_context".into()]
            } else {
                vec![]
            },
            backend_support: vec!["x11".into(), "wayland".into()],
            execution_adapter: if internal {
                "internal_only"
            } else if readonly {
                "query_state"
            } else if compiled_plan {
                "frozen_plan_serial_executor"
            } else {
                "not_adapted"
            }
            .into(),
            completion_contract: if internal {
                "internal_protocol"
            } else if readonly {
                "synchronous_read"
            } else if compiled_plan {
                "all_steps_verified_or_explicit_partial_unknown"
            } else {
                "not_adapted"
            }
            .into(),
            risk_class: if internal {
                "internal_only"
            } else if readonly {
                "read_only"
            } else if compiled_plan {
                "inherit_strictest_step"
            } else {
                "not_adapted"
            }
            .into(),
            retry_policy: "never_replay".into(),
            undo_strategy: "none".into(),
            capability_version: 1,
            privacy_fields: vec![],
            test_cases: vec![format!("catalog_{kind}")],
        });
    }
    result
}

#[derive(Debug, Serialize)]
pub struct CoverageReport {
    pub discovered_names: usize,
    pub registered_names: usize,
    pub unknown_names: Vec<String>,
    pub user_capabilities: usize,
    pub implemented_user_capabilities: usize,
    pub unimplemented_capabilities: Vec<String>,
}
pub fn coverage(commands: &[String], queries: &[String], topics: &[String]) -> CoverageReport {
    let catalog = ipc_catalog();
    let known: BTreeSet<_> = catalog
        .iter()
        .map(|e| (e.kind.as_str(), e.name.as_str()))
        .collect();
    let mut unknown = Vec::new();
    let mut discovered = 0;
    for (kind, items) in [
        ("command", commands),
        ("query", queries),
        ("subscription", topics),
    ] {
        for name in items {
            discovered += 1;
            if !known.contains(&(kind, name.as_str())) {
                unknown.push(format!("{kind}:{name}"));
            }
        }
    }
    let registry = full_registry();
    let user: Vec<_> = registry
        .iter()
        .filter(|s| s.role != "bridge_internal")
        .collect();
    let unimplemented: Vec<_> = user
        .iter()
        .filter(|s| s.support_status != "implemented")
        .map(|s| s.canonical_name.clone())
        .collect();
    CoverageReport {
        discovered_names: discovered,
        registered_names: discovered - unknown.len(),
        unknown_names: unknown,
        user_capabilities: user.len(),
        implemented_user_capabilities: user.len() - unimplemented.len(),
        unimplemented_capabilities: unimplemented,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_tools_never_expose_unfinished_or_internal_actions() {
        let tool = tool_schema();
        let variants = tool["parameters"]["properties"]["actions"]["items"]["oneOf"]
            .as_array()
            .unwrap();
        assert_eq!(variants.len(), semantic_actions().len());
        assert!(!tool.to_string().contains("clipboard_record"));
        assert!(
            !variants
                .iter()
                .any(|variant| variant["properties"]["name"]["const"] == "spawn")
        );
    }
    #[test]
    fn unknown_runtime_names_are_coverage_gaps() {
        let report = coverage(&["view".into(), "new_mutation".into()], &[], &[]);
        assert_eq!(report.registered_names, 1);
        assert_eq!(report.unknown_names, ["command:new_mutation"]);
        assert!(report.user_capabilities >= report.implemented_user_capabilities);
    }
    #[test]
    fn catalog_tracks_every_current_ipc_registration() {
        let source = include_str!("../../../../src/ipc.rs");
        let declaration = source
            .split_once("pub const IPC_REGISTRY: IpcRegistry = IpcRegistry {")
            .unwrap()
            .1
            .split_once("};")
            .unwrap()
            .0;
        let catalog = ipc_catalog();
        for (index, literal) in declaration.split('"').enumerate() {
            if index % 2 == 0
                || literal.is_empty()
                || !literal
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || matches!(c, '_' | '/' | '*'))
            {
                continue;
            }
            assert!(
                catalog.iter().any(|entry| entry.name == literal),
                "IPC name needs an explicit registry role and adapter review: {literal}"
            );
        }
    }
    #[test]
    fn every_user_capability_has_an_explicit_completion_contract() {
        for spec in full_registry() {
            assert!(!spec.ipc_names.is_empty() || !spec.canonical_name.is_empty());
            assert!(!spec.completion_contract.is_empty());
            if spec.role != "bridge_internal" {
                assert_ne!(
                    spec.support_status, "unimplemented",
                    "missing semantic adapter: {}",
                    spec.canonical_name
                );
                assert_ne!(
                    spec.completion_contract, "not_adapted",
                    "missing completion: {}",
                    spec.canonical_name
                );
            }
        }
    }
    #[test]
    fn current_registration_and_capability_inventory_report() {
        let source = include_str!("../../../../src/ipc.rs");
        let declaration = source
            .split_once("pub const IPC_REGISTRY: IpcRegistry = IpcRegistry {")
            .unwrap()
            .1
            .split_once("};")
            .unwrap()
            .0;
        let names = |field: &str| {
            declaration
                .split_once(&format!("{field}: &["))
                .unwrap()
                .1
                .split_once(']')
                .unwrap()
                .0
                .split('"')
                .enumerate()
                .filter(|(index, _)| index % 2 == 1)
                .map(|(_, name)| name.to_owned())
                .collect::<Vec<_>>()
        };
        let mut commands = names("dispatch_commands");
        commands.extend(names("special_commands"));
        commands.sort();
        commands.dedup();
        let queries = names("queries");
        let topics = names("subscription_topics");
        let report = coverage(&commands, &queries, &topics);
        assert!(
            report.unknown_names.is_empty(),
            "{:?}",
            report.unknown_names
        );
        let registry = full_registry();
        println!(
            "EVA_INVENTORY {}",
            json!({"commands":commands.len(),"queries":queries.len(),"subscription_topics":topics.len(),"semantic_actions":semantic_actions().len(),"canonical_registry_entries":registry.len(),"bridge_internal_entries":registry.iter().filter(|spec|spec.role=="bridge_internal").count(),"coverage":report,"runtime_and_live_acceptance":"not_established_by_static_registration"})
        );
    }
}
