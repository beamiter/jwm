use relm4::{
    ComponentParts, ComponentSender, SimpleComponent,
    gtk::{self, prelude::*},
};

use xbar_core::TagState;

const CLS_SELECTED: u8 = 1 << 0;
const CLS_OCCUPIED: u8 = 1 << 1;
const CLS_FILLED: u8 = 1 << 2;
const CLS_URGENT: u8 = 1 << 3;
const CLS_EMPTY: u8 = 1 << 4;

#[derive(Debug, Clone, Default)]
pub struct WorkspacesState {
    pub active_tab: usize,
    pub tag_status_vec: Vec<TagState>,
}

#[derive(Debug, Clone)]
pub enum WorkspacesInput {
    Sync(WorkspacesState),
}

#[derive(Debug, Clone)]
pub enum WorkspacesOutput {
    Selected(usize),
}

pub struct WorkspacesModel {
    state: WorkspacesState,
}

#[relm4::component(pub)]
impl SimpleComponent for WorkspacesModel {
    type Init = WorkspacesState;
    type Input = WorkspacesInput;
    type Output = WorkspacesOutput;

    view! {
        #[root]
        gtk::Box {
            set_orientation: gtk::Orientation::Horizontal,
            set_spacing: 3,
            add_css_class: "workspace-strip",

            #[name(tab_button_0)]
            gtk::Button {
                set_height_request: 28,
                set_width_request: 36,
                #[watch]
                set_label: pick_emoji(0),
                #[watch]
                set_css_classes: &button_classes(model.state.tag_status_vec.first(), 0 == model.state.active_tab),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(WorkspacesOutput::Selected(0));
                },
            },
            #[name(tab_button_1)]
            gtk::Button {
                set_height_request: 28,
                set_width_request: 36,
                #[watch]
                set_label: pick_emoji(1),
                #[watch]
                set_css_classes: &button_classes(model.state.tag_status_vec.get(1), 1 == model.state.active_tab),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(WorkspacesOutput::Selected(1));
                },
            },
            #[name(tab_button_2)]
            gtk::Button {
                set_height_request: 28,
                set_width_request: 36,
                #[watch]
                set_label: pick_emoji(2),
                #[watch]
                set_css_classes: &button_classes(model.state.tag_status_vec.get(2), 2 == model.state.active_tab),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(WorkspacesOutput::Selected(2));
                },
            },
            #[name(tab_button_3)]
            gtk::Button {
                set_height_request: 28,
                set_width_request: 36,
                #[watch]
                set_label: pick_emoji(3),
                #[watch]
                set_css_classes: &button_classes(model.state.tag_status_vec.get(3), 3 == model.state.active_tab),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(WorkspacesOutput::Selected(3));
                },
            },
            #[name(tab_button_4)]
            gtk::Button {
                set_height_request: 28,
                set_width_request: 36,
                #[watch]
                set_label: pick_emoji(4),
                #[watch]
                set_css_classes: &button_classes(model.state.tag_status_vec.get(4), 4 == model.state.active_tab),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(WorkspacesOutput::Selected(4));
                },
            },
            #[name(tab_button_5)]
            gtk::Button {
                set_height_request: 28,
                set_width_request: 36,
                #[watch]
                set_label: pick_emoji(5),
                #[watch]
                set_css_classes: &button_classes(model.state.tag_status_vec.get(5), 5 == model.state.active_tab),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(WorkspacesOutput::Selected(5));
                },
            },
            #[name(tab_button_6)]
            gtk::Button {
                set_height_request: 28,
                set_width_request: 36,
                #[watch]
                set_label: pick_emoji(6),
                #[watch]
                set_css_classes: &button_classes(model.state.tag_status_vec.get(6), 6 == model.state.active_tab),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(WorkspacesOutput::Selected(6));
                },
            },
            #[name(tab_button_7)]
            gtk::Button {
                set_height_request: 28,
                set_width_request: 36,
                #[watch]
                set_label: pick_emoji(7),
                #[watch]
                set_css_classes: &button_classes(model.state.tag_status_vec.get(7), 7 == model.state.active_tab),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(WorkspacesOutput::Selected(7));
                },
            },
            #[name(tab_button_8)]
            gtk::Button {
                set_height_request: 28,
                set_width_request: 36,
                #[watch]
                set_label: pick_emoji(8),
                #[watch]
                set_css_classes: &button_classes(model.state.tag_status_vec.get(8), 8 == model.state.active_tab),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(WorkspacesOutput::Selected(8));
                },
            },
        }
    }

    fn init(
        state: Self::Init,
        _root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = WorkspacesModel { state };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            WorkspacesInput::Sync(state) => self.state = state,
        }
    }
}

fn pick_emoji(index: usize) -> &'static str {
    match index {
        0 => "🖥",
        1 => "🌐",
        2 => "📁",
        3 => "💬",
        4 => "📝",
        5 => "🎵",
        6 => "⚙",
        7 => "📊",
        8 => "🏠",
        _ => "❔",
    }
}

fn classes_mask_for(tag: Option<&TagState>, is_active_index: bool) -> u8 {
    if let Some(tag) = tag {
        if tag.urgent {
            CLS_URGENT
        } else if tag.filled {
            CLS_FILLED
        } else if tag.selected && tag.occupied {
            CLS_SELECTED | CLS_OCCUPIED
        } else if tag.selected || is_active_index {
            CLS_SELECTED
        } else if tag.occupied {
            CLS_OCCUPIED
        } else {
            CLS_EMPTY
        }
    } else if is_active_index {
        CLS_SELECTED
    } else {
        CLS_EMPTY
    }
}

fn button_classes(tag: Option<&TagState>, is_active_index: bool) -> Vec<&'static str> {
    let mut classes = vec!["tab-button"];
    let class_mask = classes_mask_for(tag, is_active_index);

    if class_mask & CLS_URGENT != 0 {
        classes.push("urgent");
    }
    if class_mask & CLS_FILLED != 0 {
        classes.push("filled");
    }
    if class_mask & CLS_SELECTED != 0 {
        classes.push("selected");
    }
    if class_mask & CLS_OCCUPIED != 0 {
        classes.push("occupied");
    }
    if class_mask & CLS_EMPTY != 0 {
        classes.push("empty");
    }

    classes
}
