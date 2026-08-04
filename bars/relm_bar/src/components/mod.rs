mod layout_selector;
mod status_strip;
mod workspaces;

pub use layout_selector::{
    LayoutSelectorInput, LayoutSelectorModel, LayoutSelectorOutput, LayoutSelectorState,
};
pub use status_strip::{StatusStripInput, StatusStripModel, StatusStripOutput, StatusStripState};
pub use workspaces::{WorkspacesInput, WorkspacesModel, WorkspacesOutput, WorkspacesState};

pub fn load_css() {
    use relm4::gtk;

    let provider = gtk::CssProvider::new();
    provider.load_from_data(include_str!("../styles.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
