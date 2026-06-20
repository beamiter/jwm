use chrono::{Datelike, Local, Timelike};
use gloo_console::error;
use gloo_timers::callback::Interval;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsValue;
use yew::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke, catch)]
    async fn tauri_invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], js_name = listen, catch)]
    async fn tauri_listen(event: &str, handler: &Closure<dyn FnMut(JsValue)>) -> Result<JsValue, JsValue>;
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

#[derive(Deserialize)]
struct EventPayload<T> {
    payload: T,
}

const BUTTONS: [&str; 9] = ["🐖", "🐄", "🐂", "🐃", "🦥", "🦣", "🐏", "🦆", "🐢"];

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
    let symbol = lts
        .split_whitespace()
        .next()
        .unwrap_or("[]=")
        .to_string();
    let scale = lts
        .find("s:")
        .and_then(|i| {
            let rest = &lts[i + 2..];
            let s: String = rest.chars().skip_while(|c| c.is_whitespace()).take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            s.parse::<f32>().ok()
        });
    (symbol, scale)
}

fn monitor_icon(n: i32) -> String {
    if n == 0 {
        "󰎡".to_string()
    } else if n == 1 {
        "󰎤".to_string()
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

#[function_component(App)]
fn app() -> Html {
    let monitor = use_state(|| None::<MonitorInfoSnapshot>);
    let system = use_state(|| None::<SystemSnapshot>);
    let pressed = use_state(|| None::<usize>);
    let layout_open = use_state(|| false);
    let show_seconds = use_state(|| true);
    let now = use_state(Local::now);
    let is_taking = use_state(|| false);

    // listen monitor-update
    {
        let monitor = monitor.clone();
        use_effect_with((), move |_| {
            let cb = Closure::<dyn FnMut(JsValue)>::new(move |evt: JsValue| {
                if let Ok(p) = serde_wasm_bindgen::from_value::<EventPayload<MonitorInfoSnapshot>>(evt) {
                    monitor.set(Some(p.payload));
                }
            });
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = tauri_listen("monitor-update", &cb).await {
                    error!(format!("listen failed: {:?}", e));
                }
                cb.forget();
            });
            || ()
        });
    }

    // listen system-update
    {
        let system = system.clone();
        use_effect_with((), move |_| {
            let cb = Closure::<dyn FnMut(JsValue)>::new(move |evt: JsValue| {
                if let Ok(p) = serde_wasm_bindgen::from_value::<EventPayload<SystemSnapshot>>(evt) {
                    system.set(Some(p.payload));
                }
            });
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = tauri_listen("system-update", &cb).await {
                    error!(format!("listen failed: {:?}", e));
                }
                cb.forget();
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

    html! {
        <div class="button-row">
            <div class="buttons-container">
                {
                    BUTTONS.iter().enumerate().map(|(i, emoji)| {
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
                            >
                                { *emoji }
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
                            let batt_icon = if s.is_charging { "🔌" } else { "🔋" };
                            let mem_title = format!("内存使用: {} / {}", format_bytes(s.memory_used), format_bytes(s.memory_total));
                            let batt_title = if s.is_charging {
                                format!("电池充电中: {:.1}%", s.battery_percent)
                            } else {
                                format!("电池电量: {:.1}%", s.battery_percent)
                            };
                            html! {
                                <>
                                    <div class={cpu_cls} title="CPU 平均使用率">{ format!("CPU {:.0}%", s.cpu_average) }</div>
                                    <div class={mem_cls} title={mem_title}>{ format!("MEM {:.0}%", s.memory_usage_percent) }</div>
                                    <div class={batt_cls} title={batt_title}>{ format!("{} {:.0}%", batt_icon, s.battery_percent) }</div>
                                </>
                            }
                        } else {
                            html! {
                                <>
                                    <div class="pill usage-pill usage-warn">{"CPU --%"}</div>
                                    <div class="pill usage-pill usage-warn">{"MEM --%"}</div>
                                    <div class="pill usage-pill usage-warn">{"🔋 --%"}</div>
                                </>
                            }
                        }
                    }
                </div>

                <div
                    class={if *is_taking { "pill screenshot-pill taking" } else { "pill screenshot-pill" }}
                    onclick={take_screenshot}
                    title="截图 (Flameshot)"
                >
                    { if *is_taking { "⏳" } else { "📸" } }
                </div>

                <div class="pill time-pill" onclick={toggle_seconds} title="点击切换秒显示">
                    { formatted_time }
                </div>

                <div class="pill monitor-pill" title="显示器">
                    { format!("🖥️ {}", monitor_icon(m.monitor_num)) }
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
