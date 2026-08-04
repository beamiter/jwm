use relm4::{
    ComponentParts, ComponentSender, SimpleComponent,
    gtk::{self, glib::Propagation, prelude::*},
};

const LEVEL_WARN: f64 = 0.50;
const LEVEL_HIGH: f64 = 0.75;
const LEVEL_CRIT: f64 = 0.90;

#[derive(Debug, Clone)]
pub struct StatusStripState {
    pub current_time: String,
    pub current_volume: Option<i32>,
    pub current_muted: bool,
    pub cpu_usage: f64,
    pub memory_usage: f64,
    pub theme_dark: bool,
    pub monitor_num: u8,
    /// The shell lives in the window manager, so its entry has to gray out
    /// when the projection is gone rather than silently drop clicks.
    pub wm_available: bool,
}

impl Default for StatusStripState {
    fn default() -> Self {
        Self {
            current_time: String::new(),
            current_volume: None,
            current_muted: false,
            cpu_usage: 0.0,
            memory_usage: 0.0,
            theme_dark: true,
            monitor_num: 0,
            wm_available: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum StatusStripInput {
    Sync(StatusStripState),
}

#[derive(Debug, Clone)]
pub enum StatusStripOutput {
    ToggleSeconds,
    ToggleTheme,
    ToggleMute,
    VolumeStep(i32),
    Screenshot,
    /// Ask the window manager to open its own shell surface.
    OpenShell,
}

pub struct StatusStripModel {
    state: StatusStripState,
}

#[relm4::component(pub)]
impl SimpleComponent for StatusStripModel {
    type Init = StatusStripState;
    type Input = StatusStripInput;
    type Output = StatusStripOutput;

    view! {
        #[root]
        gtk::Box {
            set_orientation: gtk::Orientation::Horizontal,
            set_spacing: 4,
            set_halign: gtk::Align::End,
            add_css_class: "status-strip",

            #[name(cpu_label)]
            gtk::Label {
                set_width_request: 58,
                set_height_request: 24,
                set_justify: gtk::Justification::Center,
                #[watch]
                set_label: &metric_text("CPU", model.state.cpu_usage),
                #[watch]
                set_css_classes: &metric_classes(model.state.cpu_usage),
            },
            #[name(memory_label)]
            gtk::Label {
                set_width_request: 58,
                set_height_request: 24,
                #[watch]
                set_label: &metric_text("MEM", model.state.memory_usage),
                #[watch]
                set_css_classes: &metric_classes(model.state.memory_usage),
            },
            #[name(volume_button)]
            gtk::Button {
                set_width_request: 92,
                set_height_request: 28,
                add_css_class: "volume-button",
                #[watch]
                set_label: &volume_label(model.state.current_volume, model.state.current_muted),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(StatusStripOutput::ToggleMute);
                },
            },
            // Entry point into JWM's own shell surface. It opens the hub,
            // which is itself the page that routes to applications,
            // notifications, clipboard, calendar and wallpaper.
            #[name(shell_button)]
            gtk::Button {
                set_width_request: 40,
                set_height_request: 28,
                set_label: " \u{25c8} ",
                set_tooltip_text: Some("Open the JWM shell"),
                add_css_class: "shell-button",
                #[watch]
                set_sensitive: model.state.wm_available,
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(StatusStripOutput::OpenShell);
                },
            },
            #[name(theme_button)]
            gtk::Button {
                set_width_request: 40,
                set_height_request: 28,
                add_css_class: "theme-button",
                #[watch]
                set_label: if model.state.theme_dark { " 🌙 " } else { " ☀️ " },
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(StatusStripOutput::ToggleTheme);
                },
            },
            gtk::Button {
                set_width_request: 56,
                set_height_request: 28,
                set_label: " 📸 ",
                add_css_class: "screenshot-button",
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(StatusStripOutput::Screenshot);
                },
            },
            #[name(time_button)]
            gtk::Button {
                set_width_request: 132,
                set_height_request: 28,
                add_css_class: "time-button",
                #[watch]
                set_label: &model.state.current_time,
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(StatusStripOutput::ToggleSeconds);
                },
            },
            #[name(monitor_label)]
            gtk::Label {
                set_width_request: 36,
                set_height_request: 28,
                set_halign: gtk::Align::Center,
                add_css_class: "monitor-badge",
                #[watch]
                set_label: monitor_num_to_icon(model.state.monitor_num),
            },
        }
    }

    fn init(
        state: Self::Init,
        _root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = StatusStripModel { state };
        let widgets = view_output!();

        let sender = sender.clone();
        let controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        controller.connect_scroll(move |_, _dx, dy| {
            if dy == 0.0 {
                return Propagation::Proceed;
            }

            let step = if dy < 0.0 { 3 } else { -3 };
            let _ = sender.output(StatusStripOutput::VolumeStep(step));
            Propagation::Stop
        });
        widgets.volume_button.add_controller(controller);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            StatusStripInput::Sync(state) => self.state = state,
        }
    }
}

fn monitor_num_to_icon(monitor_num: u8) -> &'static str {
    match monitor_num {
        0 => "🥇",
        1 => "🥈",
        2 => "🥉",
        _ => "🖥",
    }
}

fn usage_to_level_class(ratio: f64) -> &'static str {
    if ratio >= LEVEL_CRIT {
        "level-crit"
    } else if ratio >= LEVEL_HIGH {
        "level-high"
    } else if ratio >= LEVEL_WARN {
        "level-warn"
    } else {
        "level-ok"
    }
}

fn metric_text(title: &str, ratio: f64) -> String {
    let percent = (ratio * 100.0).round().clamp(0.0, 100.0) as i32;
    format!("{} {}%", title, percent)
}

fn metric_classes(ratio: f64) -> Vec<&'static str> {
    vec!["metric-label", usage_to_level_class(ratio)]
}

fn volume_label(current_volume: Option<i32>, current_muted: bool) -> String {
    match current_volume {
        Some(volume) => {
            if current_muted {
                format!(" 🔇 {}% ", volume)
            } else {
                format!(" 🔊 {}% ", volume)
            }
        }
        None => " 🔊 --% ".to_string(),
    }
}
