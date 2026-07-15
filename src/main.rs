use chrono::{Datelike, Local, Timelike};
use gloo_console::error;
use gloo_timers::callback::Interval;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::*;
use yew::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke, catch)]
    async fn tauri_invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], js_name = listen, catch)]
    async fn tauri_listen(
        event: &str,
        handler: &Closure<dyn FnMut(JsValue)>,
    ) -> Result<JsValue, JsValue>;
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, Default)]
struct TagStatus {
    is_selected: bool,
    is_urg: bool,
    is_filled: bool,
    is_occ: bool,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
struct MonitorInfoSnapshot {
    monitor_num: i32,
    monitor_width: i32,
    monitor_height: i32,
    monitor_x: i32,
    monitor_y: i32,
    tag_status_vec: Vec<TagStatus>,
    client_name: String,
    ltsymbol: String,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
struct SystemSnapshot {
    cpu_average: f32,
    memory_used: u64,
    memory_total: u64,
    memory_usage_percent: f32,
    battery_percent: f32,
    is_charging: bool,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
struct AudioSnapshot {
    volume: i32,
    is_muted: bool,
    device_name: String,
    has_device: bool,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
struct BrightnessSnapshot {
    percent: Option<u8>,
}

#[derive(Deserialize)]
struct EventPayload<T> {
    payload: T,
}

const TAG_ICONS: [&str; 9] = [
    "\u{F0A1E}",
    "\u{F0239}",
    "\u{F0A1B}",
    "\u{F0B79}",
    "\u{F024B}",
    "\u{F0388}",
    "\u{F0567}",
    "\u{F01F0}",
    "\u{F0297}",
];

const ICON_CPU: &str = "\u{F4BC}";
const ICON_MEM: &str = "\u{F035B}";
const ICON_BAT_FULL: &str = "\u{F0079}";
const ICON_BAT_CHG: &str = "\u{F0084}";
const ICON_VOL_HIGH: &str = "\u{F057E}";
const ICON_VOL_MID: &str = "\u{F0580}";
const ICON_VOL_LOW: &str = "\u{F057F}";
const ICON_VOL_MUTE: &str = "\u{F075F}";
const ICON_BRIGHT: &str = "\u{F00DE}";
const ICON_SHOT: &str = "\u{F0104}";
const ICON_TIME: &str = "\u{F0954}";
const ICON_MON: &str = "\u{F0379}";

fn button_class(t: &TagStatus) -> &'static str {
    if t.is_filled {
        "emoji-button state-filtered"
    } else if t.is_selected {
        "emoji-button state-selected"
    } else if t.is_urg {
        "emoji-button state-urgent"
    } else if t.is_occ {
        "emoji-button state-occupied"
    } else {
        "emoji-button state-default"
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "0B".to_string();
    }
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let i = ((bytes as f64).ln() / 1024f64.ln()).floor() as usize;
    let i = i.min(U.len() - 1);
    let s = bytes as f64 / 1024f64.powi(i as i32);
    if i == 0 {
        format!("{:.0}{}", s, U[i])
    } else {
        format!("{:.1}{}", s, U[i])
    }
}

fn parse_lt_symbol(lts: &str) -> (String, Option<f32>) {
    if lts.is_empty() {
        return ("[]=".to_string(), None);
    }
    let symbol = lts.split_whitespace().next().unwrap_or("[]=").to_string();
    let scale = lts.find("s:").and_then(|i| {
        let rest = &lts[i + 2..];
        let s: String = rest
            .chars()
            .skip_while(|c| c.is_whitespace())
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        s.parse::<f32>().ok()
    });
    (symbol, scale)
}

fn monitor_icon(n: i32) -> String {
    if n == 0 {
        "\u{F02DA}".to_string()
    } else if n == 1 {
        "\u{F02DB}".to_string()
    } else {
        format!("M{}", n)
    }
}

fn sev(p: f32) -> &'static str {
    if p <= 30.0 {
        "usage-good"
    } else if p <= 60.0 {
        "usage-warn"
    } else if p <= 80.0 {
        "usage-caution"
    } else {
        "usage-danger"
    }
}

fn volume_icon(a: Option<&AudioSnapshot>) -> &'static str {
    match a {
        None => ICON_VOL_MUTE,
        Some(s) => {
            if !s.has_device || s.is_muted || s.volume <= 0 {
                ICON_VOL_MUTE
            } else if s.volume < 34 {
                ICON_VOL_LOW
            } else if s.volume < 67 {
                ICON_VOL_MID
            } else {
                ICON_VOL_HIGH
            }
        }
    }
}

fn invoke_async(cmd: &'static str, args: JsValue) {
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = tauri_invoke(cmd, args).await {
            error!(format!("invoke {} failed: {:?}", cmd, e));
        }
    });
}

#[derive(Serialize)]
struct TagCmdArgs {
    #[serde(rename = "tagIndex")]
    tag_index: usize,
    #[serde(rename = "isView")]
    is_view: bool,
    #[serde(rename = "monitorId")]
    monitor_id: i32,
}

#[derive(Serialize)]
struct LayoutCmdArgs {
    #[serde(rename = "layoutIndex")]
    layout_index: u32,
    #[serde(rename = "monitorId")]
    monitor_id: i32,
}

#[derive(Serialize)]
struct DeltaArgs {
    delta: i32,
}

#[function_component(App)]
fn app() -> Html {
    let monitor = use_state(|| None::<MonitorInfoSnapshot>);
    let system = use_state(|| None::<SystemSnapshot>);
    let audio = use_state(|| None::<AudioSnapshot>);
    let brightness = use_state(|| None::<BrightnessSnapshot>);
    let pressed = use_state(|| None::<usize>);
    let layout_open = use_state(|| false);
    let show_seconds = use_state(|| true);
    let now = use_state(Local::now);
    let is_taking = use_state(|| false);

    // Register every state listener before asking the backend for a full replay.
    {
        let monitor = monitor.clone();
        let system = system.clone();
        let audio = audio.clone();
        let brightness = brightness.clone();
        use_effect_with((), move |_| {
            let monitor_cb = Closure::<dyn FnMut(JsValue)>::new(move |evt: JsValue| {
                if let Ok(p) =
                    serde_wasm_bindgen::from_value::<EventPayload<Option<MonitorInfoSnapshot>>>(evt)
                {
                    monitor.set(p.payload);
                }
            });
            let system_cb = Closure::<dyn FnMut(JsValue)>::new(move |evt: JsValue| {
                if let Ok(p) = serde_wasm_bindgen::from_value::<EventPayload<SystemSnapshot>>(evt) {
                    system.set(Some(p.payload));
                }
            });
            let audio_cb = Closure::<dyn FnMut(JsValue)>::new(move |evt: JsValue| {
                if let Ok(p) = serde_wasm_bindgen::from_value::<EventPayload<AudioSnapshot>>(evt) {
                    audio.set(Some(p.payload));
                }
            });
            let brightness_cb = Closure::<dyn FnMut(JsValue)>::new(move |evt: JsValue| {
                if let Ok(p) =
                    serde_wasm_bindgen::from_value::<EventPayload<BrightnessSnapshot>>(evt)
                {
                    brightness.set(Some(p.payload));
                }
            });

            wasm_bindgen_futures::spawn_local(async move {
                let registration = async {
                    tauri_listen("monitor-update", &monitor_cb).await?;
                    tauri_listen("system-update", &system_cb).await?;
                    tauri_listen("audio-update", &audio_cb).await?;
                    tauri_listen("brightness-update", &brightness_cb).await?;
                    tauri_invoke("frontend_ready", JsValue::NULL).await?;
                    Ok::<(), JsValue>(())
                }
                .await;
                if let Err(e) = registration {
                    error!(format!("failed to initialize Tauri event bridge: {:?}", e));
                }
                monitor_cb.forget();
                system_cb.forget();
                audio_cb.forget();
                brightness_cb.forget();
            });
            || ()
        });
    }

    // tick clock
    {
        let now = now.clone();
        let show_seconds = show_seconds.clone();
        use_effect_with(*show_seconds, move |&secs| {
            let n = now.clone();
            let interval_ms = if secs { 1000 } else { 60000 };
            let handle = Interval::new(interval_ms, move || n.set(Local::now()));
            move || drop(handle)
        });
    }

    let monitor_val = (*monitor).clone();
    let system_val = (*system).clone();
    let audio_val = (*audio).clone();
    let brightness_val = (*brightness).clone();

    if monitor_val.is_none() {
        return html! { <div class="button-row">{"Loading..."}</div> };
    }
    let m = monitor_val.unwrap();
    let (lt_symbol, lt_scale) = parse_lt_symbol(&m.ltsymbol);

    let on_press = {
        let pressed = pressed.clone();
        move |i: usize| {
            let p = pressed.clone();
            Callback::from(move |_| p.set(Some(i)))
        }
    };
    let on_release = {
        let pressed = pressed.clone();
        let mn = m.monitor_num;
        move |i: usize| {
            let p = pressed.clone();
            Callback::from(move |_: MouseEvent| {
                p.set(None);
                let args = serde_wasm_bindgen::to_value(&TagCmdArgs {
                    tag_index: i,
                    is_view: true,
                    monitor_id: mn,
                })
                .unwrap_or(JsValue::NULL);
                invoke_async("send_tag_command", args);
            })
        }
    };
    let on_leave = {
        let pressed = pressed.clone();
        Callback::from(move |_: MouseEvent| pressed.set(None))
    };

    let select_layout = {
        let layout_open = layout_open.clone();
        let mn = m.monitor_num;
        move |idx: u32| {
            let lo = layout_open.clone();
            Callback::from(move |_: MouseEvent| {
                lo.set(false);
                let args = serde_wasm_bindgen::to_value(&LayoutCmdArgs {
                    layout_index: idx,
                    monitor_id: mn,
                })
                .unwrap_or(JsValue::NULL);
                invoke_async("send_layout_command", args);
            })
        }
    };

    let toggle_layout = {
        let layout_open = layout_open.clone();
        Callback::from(move |_: MouseEvent| layout_open.set(!*layout_open))
    };

    let take_screenshot = {
        let is_taking = is_taking.clone();
        Callback::from(move |_: MouseEvent| {
            if *is_taking {
                return;
            }
            is_taking.set(true);
            let it = is_taking.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = tauri_invoke("take_screenshot", JsValue::NULL).await {
                    error!(format!("screenshot failed: {:?}", e));
                }
                gloo_timers::future::TimeoutFuture::new(500).await;
                it.set(false);
            });
        })
    };

    let toggle_seconds = {
        let show_seconds = show_seconds.clone();
        Callback::from(move |_: MouseEvent| show_seconds.set(!*show_seconds))
    };

    let toggle_mute = Callback::from(move |_: MouseEvent| {
        invoke_async("toggle_mute", JsValue::NULL);
    });
    let volume_wheel = Callback::from(move |e: WheelEvent| {
        e.prevent_default();
        let delta = if e.delta_y() < 0.0 { 5 } else { -5 };
        let args = serde_wasm_bindgen::to_value(&DeltaArgs { delta }).unwrap_or(JsValue::NULL);
        invoke_async("adjust_volume", args);
    });

    let brightness_click = Callback::from(move |_: MouseEvent| {
        let args = serde_wasm_bindgen::to_value(&DeltaArgs { delta: 5 }).unwrap_or(JsValue::NULL);
        invoke_async("adjust_brightness", args);
    });
    let brightness_wheel = Callback::from(move |e: WheelEvent| {
        e.prevent_default();
        let delta = if e.delta_y() < 0.0 { 5 } else { -5 };
        let args = serde_wasm_bindgen::to_value(&DeltaArgs { delta }).unwrap_or(JsValue::NULL);
        invoke_async("adjust_brightness", args);
    });
    let brightness_right = Callback::from(move |e: MouseEvent| {
        e.prevent_default();
        let args = serde_wasm_bindgen::to_value(&DeltaArgs { delta: -5 }).unwrap_or(JsValue::NULL);
        invoke_async("adjust_brightness", args);
    });

    let formatted_time = {
        let d = *now;
        let pad = |n: u32| format!("{:02}", n);
        let ts = if *show_seconds {
            format!("{}:{}:{}", pad(d.hour()), pad(d.minute()), pad(d.second()))
        } else {
            format!("{}:{}", pad(d.hour()), pad(d.minute()))
        };
        format!("{}-{}-{} {}", d.year(), pad(d.month()), pad(d.day()), ts)
    };

    let layout_toggle_class = if *layout_open {
        "pill layout-toggle open"
    } else {
        "pill layout-toggle closed"
    };

    let opt_class = |s: &str| -> String {
        if lt_symbol == s {
            "pill layout-option current".to_string()
        } else {
            "pill layout-option".to_string()
        }
    };

    let tags = m.tag_status_vec.clone();

    let volume_pill_class = {
        let muted = match audio_val.as_ref() {
            None => true,
            Some(s) => s.is_muted || !s.has_device,
        };
        if muted {
            "pill volume-pill muted"
        } else {
            "pill volume-pill"
        }
    };
    let volume_label = match audio_val.as_ref() {
        Some(s) if s.has_device => format!("{}%", s.volume),
        _ => "--".to_string(),
    };
    let volume_ico = volume_icon(audio_val.as_ref());

    let brightness_label = match brightness_val.as_ref().and_then(|b| b.percent) {
        Some(p) => format!("{}%", p),
        None => "--".to_string(),
    };

    html! {
        <div class="button-row">
            <div class="buttons-container">
                {
                    TAG_ICONS.iter().enumerate().map(|(i, icon)| {
                        let tag = tags.get(i).cloned().unwrap_or_default();
                        let base = button_class(&tag);
                        let cls = if *pressed == Some(i) {
                            format!("{} pressed", base)
                        } else {
                            base.to_string()
                        };
                        html! {
                            <button
                                key={i}
                                class={cls}
                                onmousedown={on_press(i)}
                                onmouseup={on_release(i)}
                                onmouseleave={on_leave.clone()}
                                title={format!("Tag {}", i + 1)}
                            >
                                <span class="nf-icon">{ *icon }</span>
                            </button>
                        }
                    }).collect::<Html>()
                }
                <div class="layout-controls">
                    <div class={layout_toggle_class} onclick={toggle_layout} title="切换布局">
                        { lt_symbol.clone() }
                    </div>
                    if *layout_open {
                        <div class="layout-selector">
                            <div class={opt_class("[]=")} onclick={select_layout(0)}>{"[]="}</div>
                            <div class={opt_class("><>")} onclick={select_layout(1)}>{"><>"}</div>
                            <div class={opt_class("[M]")} onclick={select_layout(2)}>{"[M]"}</div>
                        </div>
                    }
                </div>
            </div>

            <div class="spacer"></div>

            <div class="right-info-container">
                <div class="system-info-container">
                    {
                        if let Some(s) = system_val {
                            let cpu_cls = format!("pill usage-pill {}", sev(s.cpu_average));
                            let mem_cls = format!("pill usage-pill {}", sev(s.memory_usage_percent));
                            let batt_cls = format!("pill usage-pill {}",
                                if s.battery_percent > 50.0 { "usage-good" }
                                else if s.battery_percent > 20.0 { "usage-warn" }
                                else { "usage-danger" });
                            let batt_icon = if s.is_charging { ICON_BAT_CHG } else { ICON_BAT_FULL };
                            let mem_title = format!("内存使用: {} / {}", format_bytes(s.memory_used), format_bytes(s.memory_total));
                            let batt_title = if s.is_charging {
                                format!("电池充电中: {:.1}%", s.battery_percent)
                            } else {
                                format!("电池电量: {:.1}%", s.battery_percent)
                            };
                            html! {
                                <>
                                    <div class={cpu_cls} title="CPU 平均使用率">
                                        <span class="nf-icon">{ ICON_CPU }</span>{ format!(" {:.0}%", s.cpu_average) }
                                    </div>
                                    <div class={mem_cls} title={mem_title}>
                                        <span class="nf-icon">{ ICON_MEM }</span>{ format!(" {:.0}%", s.memory_usage_percent) }
                                    </div>
                                    <div class={batt_cls} title={batt_title}>
                                        <span class="nf-icon">{ batt_icon }</span>{ format!(" {:.0}%", s.battery_percent) }
                                    </div>
                                </>
                            }
                        } else {
                            html! {
                                <>
                                    <div class="pill usage-pill usage-warn"><span class="nf-icon">{ ICON_CPU }</span>{" --%"}</div>
                                    <div class="pill usage-pill usage-warn"><span class="nf-icon">{ ICON_MEM }</span>{" --%"}</div>
                                    <div class="pill usage-pill usage-warn"><span class="nf-icon">{ ICON_BAT_FULL }</span>{" --%"}</div>
                                </>
                            }
                        }
                    }
                </div>

                <div
                    class="pill brightness-pill"
                    onclick={brightness_click}
                    onwheel={brightness_wheel}
                    oncontextmenu={brightness_right}
                    title="左键加亮 / 右键减暗 / 滚轮调节"
                >
                    <span class="nf-icon">{ ICON_BRIGHT }</span>{ format!(" {}", brightness_label) }
                </div>

                <div
                    class={volume_pill_class}
                    onclick={toggle_mute}
                    onwheel={volume_wheel}
                    title="左键静音 / 滚轮调节"
                >
                    <span class="nf-icon">{ volume_ico }</span>{ format!(" {}", volume_label) }
                </div>

                <div
                    class={if *is_taking { "pill screenshot-pill taking" } else { "pill screenshot-pill" }}
                    onclick={take_screenshot}
                    title="截图 (Flameshot)"
                >
                    <span class="nf-icon">{ ICON_SHOT }</span>
                </div>

                <div class="pill time-pill" onclick={toggle_seconds} title="点击切换秒显示">
                    <span class="nf-icon">{ ICON_TIME }</span>{ format!(" {}", formatted_time) }
                </div>

                <div class="pill monitor-pill" title="显示器">
                    <span class="nf-icon">{ ICON_MON }</span>{ format!(" {}", monitor_icon(m.monitor_num)) }
                </div>

                <div class="pill scale-pill" title="Scale Factor">
                    {
                        match lt_scale {
                            Some(s) => format!("s: {:.2}", s),
                            None => "s: --".to_string(),
                        }
                    }
                </div>
            </div>
        </div>
    }
}

fn main() {
    let document = web_sys::window().unwrap().document().unwrap();
    let root = document.get_element_by_id("root").unwrap();
    yew::Renderer::<App>::with_root(root).render();
}
