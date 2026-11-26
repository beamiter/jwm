//! Opt-in FileManager1 folder requests; no bus activation or replacing an owner.
use gtk4 as gtk;
use gtk4::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    path::{Component, PathBuf},
    rc::Rc,
};

const NAME: &str = "org.freedesktop.FileManager1";
const PATH: &str = "/org/freedesktop/FileManager1";
const MAX_URI_BYTES: usize = 4096;
const MAX_STARTUP_BYTES: usize = 1024;
const MAX_REQUEST_BYTES: usize =
    (MAX_URI_BYTES + 16) * crate::WINDOW_LIMIT + MAX_STARTUP_BYTES + 32;
const XML: &str = "<node><interface name='org.freedesktop.FileManager1'> <method name='ShowFolders'><arg type='as' name='URIs' direction='in'/> <arg type='s' name='StartupId' direction='in'/></method></interface></node>";

fn native_folders(values: &[String]) -> Result<Vec<PathBuf>, &'static str> {
    if values.is_empty() || values.len() > crate::WINDOW_LIMIT {
        return Err("expected between one and eight local folder URIs");
    }
    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();
    for value in values {
        if value.len() > MAX_URI_BYTES
            || !value.starts_with("file://")
            || value.contains(['?', '#', '\0'])
        {
            return Err("expected a bounded native file URI without query or fragment");
        }
        let (path, host) = glib::filename_from_uri(value).map_err(|_| "invalid native file URI")?;
        if host
            .as_deref()
            .is_some_and(|host| !host.eq_ignore_ascii_case("localhost"))
            || !path.is_absolute()
            || path.components().any(|part| part == Component::ParentDir)
        {
            return Err("remote or nonlocal folder URI is not supported");
        }
        if seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn request_paths(parameters: &glib::Variant) -> Result<Vec<PathBuf>, &'static str> {
    // Bound the serialized size before copying any untrusted strings into Rust.
    if parameters.size() > MAX_REQUEST_BYTES {
        return Err("folder request exceeds the maximum size");
    }
    let (values, startup) = parameters
        .get::<(Vec<String>, String)>()
        .ok_or("expected folder URI list and startup ID")?;
    if startup.len() > MAX_STARTUP_BYTES {
        return Err("startup ID is too large");
    }
    native_folders(&values)
}

#[derive(Default)]
struct Binding {
    owns: Cell<bool>,
    object: RefCell<Option<(gio::DBusConnection, gio::RegistrationId)>>,
    owner: RefCell<Option<gio::OwnerId>>,
}

impl Binding {
    fn stop(&self) {
        self.owns.set(false);
        if let Some(owner) = self.owner.take() {
            gio::bus_unown_name(owner);
        }
        self.remove_object();
    }

    fn remove_object(&self) {
        if let Some((connection, object)) = self.object.take() {
            let _ = connection.unregister_object(object);
        }
    }
}

pub(super) fn install(
    app: &gtk::Application,
    copy_buffer: crate::copy_ui::CopyBuffer,
    home: PathBuf,
    special: [(glib::UserDirectory, Option<PathBuf>); 2],
) {
    let binding = Rc::new(Binding::default());
    let app_weak = app.downgrade();
    let bus_acquired = {
        let binding = Rc::downgrade(&binding);
        move |connection: gio::DBusConnection, _: &str| {
            let Some(activation) = binding.upgrade() else {
                return;
            };
            let Ok(node) = gio::DBusNodeInfo::for_xml(XML) else {
                return;
            };
            let Some(interface) = node.lookup_interface(NAME) else {
                return;
            };
            let (binding, app_weak, home, special, copy_buffer) = (
                binding.clone(),
                app_weak.clone(),
                home.clone(),
                special.clone(),
                copy_buffer.clone(),
            );
            let registration = connection
                .register_object(PATH, &interface)
                .method_call(move |_, _, _, _, method, parameters, invocation| {
                    let Some(app) = app_weak.upgrade() else {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.Failed",
                            "JWM Files is closing",
                        );
                        return;
                    };
                    if !binding.upgrade().is_some_and(|binding| binding.owns.get())
                        || method != "ShowFolders"
                    {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.Failed",
                            "JWM Files does not own this service",
                        );
                        return;
                    }
                    let paths = request_paths(&parameters);
                    let paths = match paths {
                        Ok(paths) => paths,
                        Err(reason) => {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                reason,
                            );
                            return;
                        }
                    };
                    if crate::browser_window_count(&app) + paths.len() > crate::WINDOW_LIMIT {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.LimitsExceeded",
                            "Too many file windows",
                        );
                        return;
                    }
                    // The request only opens folder views. Each view independently
                    // loads and reports filesystem errors on its normal scan worker.
                    for path in paths {
                        crate::open_window(&app, &path, &home, &special, &copy_buffer);
                    }
                    invocation.return_value(Some(&().to_variant()));
                })
                .build();
            match registration {
                Ok(object) => {
                    activation.object.replace(Some((connection, object)));
                }
                Err(error) => eprintln!("FileManager1 object unavailable: {error}"),
            }
        }
    };
    let name_acquired = {
        let binding = Rc::downgrade(&binding);
        move |_: gio::DBusConnection, _: &str| {
            if let Some(binding) = binding.upgrade() {
                binding.owns.set(binding.object.borrow().is_some());
                if !binding.owns.get() {
                    binding.stop();
                }
            }
        }
    };
    let name_lost = {
        let binding = Rc::downgrade(&binding);
        move |_: Option<gio::DBusConnection>, _: &str| {
            if let Some(binding) = binding.upgrade() {
                binding.stop();
                eprintln!(
                    "FileManager1 not started: session name unavailable; existing owner unchanged"
                );
            }
        }
    };
    let owner = gio::bus_own_name(
        gio::BusType::Session,
        NAME,
        gio::BusNameOwnerFlags::DO_NOT_QUEUE,
        bus_acquired,
        name_acquired,
        name_lost,
    );
    binding.owner.replace(Some(owner));
    app.connect_shutdown(move |_| binding.stop());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn serialised_folder_arguments_are_bounded_before_path_decoding() {
        assert!(request_paths(&(vec!["file:///a"], "").to_variant()).is_ok());
        assert!(
            request_paths(&(vec!["file:///a"], "x".repeat(MAX_STARTUP_BYTES + 1)).to_variant())
                .is_err()
        );
        assert!(
            request_paths(&(vec!["file:///a"], "x".repeat(MAX_REQUEST_BYTES + 1)).to_variant())
                .is_err()
        );
        assert!(request_paths(&(vec!["file:///a"],).to_variant()).is_err());
    }

    #[test]
    fn folder_bus_accepts_exact_native_paths_and_rejects_partial_batches() {
        let paths = native_folders(&[
            "file:///tmp/space%20name%25%80".into(),
            "file://localhost/tmp/space%20name%25%80".into(),
            "file:///tmp/second%23name".into(),
        ])
        .unwrap();
        assert_eq!(paths[0].as_os_str().as_bytes(), b"/tmp/space name%\x80");
        assert_eq!(paths[1].as_os_str().as_bytes(), b"/tmp/second#name");
        assert_eq!(paths.len(), 2);
        for uri in [
            "file://remote/tmp/a",
            "smb://example/tmp/a",
            "file:///a?b",
            "file:///a#b",
            "file:///a/%00",
            "file:///a/../b",
        ] {
            assert!(
                native_folders(&["file:///good".into(), uri.into()]).is_err(),
                "{uri}"
            );
        }
        assert!(native_folders(&[]).is_err());
        assert!(native_folders(&vec!["file:///".into(); crate::WINDOW_LIMIT + 1]).is_err());
    }
}
