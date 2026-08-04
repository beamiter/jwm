use relm4::{
    ComponentParts, ComponentSender, SimpleComponent,
    gtk::{self, prelude::*},
};

#[derive(Debug, Clone, Default)]
pub struct LayoutSelectorState {
    pub layout_symbol: String,
    pub layout_open: bool,
}

#[derive(Debug, Clone)]
pub enum LayoutSelectorInput {
    Sync(LayoutSelectorState),
}

#[derive(Debug, Clone)]
pub enum LayoutSelectorOutput {
    TogglePanel,
    SelectLayout(u32),
}

pub struct LayoutSelectorModel {
    state: LayoutSelectorState,
}

#[relm4::component(pub)]
impl SimpleComponent for LayoutSelectorModel {
    type Init = LayoutSelectorState;
    type Input = LayoutSelectorInput;
    type Output = LayoutSelectorOutput;

    view! {
        #[root]
        gtk::Box {
            set_orientation: gtk::Orientation::Horizontal,
            set_spacing: 4,

            #[name(layout_toggle)]
            gtk::Button {
                set_height_request: 28,
                set_width_request: 46,
                #[watch]
                set_label: &model.state.layout_symbol,
                #[watch]
                set_css_classes: &layout_toggle_classes(model.state.layout_open),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(LayoutSelectorOutput::TogglePanel);
                },
            },

            #[name(layout_revealer)]
            gtk::Revealer {
                #[watch]
                set_reveal_child: model.state.layout_open,
                set_transition_type: gtk::RevealerTransitionType::SlideRight,
                set_transition_duration: 120,

                gtk::Box {
                    set_orientation: gtk::Orientation::Horizontal,
                    set_spacing: 4,
                    add_css_class: "layout-selector",

                    #[name(layout_option_tiled)]
                    gtk::Button {
                        set_height_request: 24,
                        set_width_request: 36,
                        set_label: "[]=",
                        #[watch]
                        set_css_classes: &layout_option_classes(model.state.layout_symbol.contains("[]=")),
                        connect_clicked[sender] => move |_| {
                            let _ = sender.output(LayoutSelectorOutput::SelectLayout(0));
                        },
                    },
                    #[name(layout_option_floating)]
                    gtk::Button {
                        set_height_request: 24,
                        set_width_request: 36,
                        set_label: "<><>",
                        #[watch]
                        set_css_classes: &layout_option_classes(model.state.layout_symbol.contains("><>")),
                        connect_clicked[sender] => move |_| {
                            let _ = sender.output(LayoutSelectorOutput::SelectLayout(1));
                        },
                    },
                    #[name(layout_option_monocle)]
                    gtk::Button {
                        set_height_request: 24,
                        set_width_request: 36,
                        set_label: "[M]",
                        #[watch]
                        set_css_classes: &layout_option_classes(model.state.layout_symbol.contains("[M]")),
                        connect_clicked[sender] => move |_| {
                            let _ = sender.output(LayoutSelectorOutput::SelectLayout(2));
                        },
                    },
                }
            }
        }
    }

    fn init(
        state: Self::Init,
        _root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = LayoutSelectorModel { state };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            LayoutSelectorInput::Sync(state) => self.state = state,
        }
    }
}

fn layout_toggle_classes(layout_open: bool) -> Vec<&'static str> {
    vec!["layout-toggle", if layout_open { "open" } else { "closed" }]
}

fn layout_option_classes(is_current: bool) -> Vec<&'static str> {
    let mut classes = vec!["layout-option"];
    if is_current {
        classes.push("current");
    }
    classes
}
