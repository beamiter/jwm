#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one match, found {count}")
    return text.replace(old, new, 1)


cargo_path = Path("Cargo.toml")
cargo = cargo_path.read_text()
cargo = replace_once(
    cargo,
    'shared_structures = { git = "https://github.com/beamiter/shared_structures.git", features = ["use-futex"] }',
    'shared_structures = { git = "https://github.com/beamiter/shared_structures.git", rev = "8c162f3b4a4cdeac49ef1a47545a2f4427f98dff", features = ["use-futex"] }',
    "shared_structures revision",
)
cargo = replace_once(
    cargo,
    'xbar_core = { git = "https://github.com/beamiter/xbar_core.git" }',
    'xbar_core = { git = "https://github.com/beamiter/xbar_core.git", rev = "6a4b136c8158824e5857b438e3fcf1cf8e21974c", features = ["logging-flexi"] }',
    "xbar_core revision and logging feature",
)
cargo = replace_once(
    cargo,
    'smithay = { git = "https://github.com/Smithay/smithay.git", branch = "master", default-features = false, features = [',
    'smithay = { git = "https://github.com/Smithay/smithay.git", rev = "e76f1af1418e9cdf012da9020c2f1ecc0fe020fa", default-features = false, features = [',
    "smithay revision",
)
cargo_path.write_text(cargo)

main_path = Path("src/main.rs")
main = main_path.read_text()
main = replace_once(
    main,
    "use xbar_core::initialize_logging;",
    "use xbar_core::logging::init as initialize_logging;",
    "xbar_core logging API",
)
main_path.write_text(main)

support_path = Path("tools/jwm_support.rs")
support = support_path.read_text()
support = replace_once(
    support,
    '''fn sanitize_live_data(query: &str, data: &mut Value) {
    if query != "get_status" {
        return;
    }
    let Some(config) = data.get_mut("config").and_then(Value::as_object_mut) else {
        return;
    };

    config.insert(
        "path".to_string(),
        Value::String("<configuration path redacted>".to_string()),
    );
    if let Some(diagnostics) = config.get_mut("diagnostics").and_then(Value::as_object_mut) {
        diagnostics.remove("issues");
        diagnostics.insert("issues_included".to_string(), Value::Bool(false));
    }
    if let Some(reload) = config.get_mut("reload").and_then(Value::as_object_mut) {
        let error_present = reload
            .get("last_error")
            .is_some_and(|error| !error.is_null());
        reload.insert("last_error".to_string(), Value::Null);
        reload.insert(
            "last_error_present".to_string(),
            Value::Bool(error_present),
        );
    }
}
''',
    '''fn sanitize_live_data(query: &str, data: &mut Value) {
    if query != "get_status" {
        return;
    }

    if let Some(reasons) = data
        .get_mut("health")
        .and_then(Value::as_object_mut)
        .and_then(|health| health.get_mut("reasons"))
        .and_then(Value::as_array_mut)
    {
        for reason in reasons {
            let Some(text) = reason.as_str() else {
                continue;
            };
            *reason = Value::String(if text.starts_with("last configuration reload failed:") {
                "last configuration reload failed (detail redacted)".to_string()
            } else {
                sanitize_reported_value(text)
            });
        }
    }

    let Some(config) = data.get_mut("config").and_then(Value::as_object_mut) else {
        return;
    };

    config.insert(
        "path".to_string(),
        Value::String("<configuration path redacted>".to_string()),
    );
    if let Some(diagnostics) = config.get_mut("diagnostics").and_then(Value::as_object_mut) {
        diagnostics.remove("issues");
        diagnostics.insert("issues_included".to_string(), Value::Bool(false));
    }
    if let Some(reload) = config.get_mut("reload").and_then(Value::as_object_mut) {
        let error_present = reload
            .get("last_error")
            .is_some_and(|error| !error.is_null());
        reload.insert("last_error".to_string(), Value::Null);
        reload.insert(
            "last_error_present".to_string(),
            Value::Bool(error_present),
        );
    }
}
''',
    "live health reason redaction",
)
support = replace_once(
    support,
    '''        Err(error) => {
            let message = format!("cannot resolve a safe IPC socket: {error}");
            return LiveSnapshot {
''',
    '''        Err(_) => {
            let message = "cannot resolve a safe IPC socket".to_string();
            return LiveSnapshot {
''',
    "socket resolution error redaction",
)
support = replace_once(
    support,
    '''        Err(error) => QueryProbe::failed(error),
''',
    '''        Err(_) => QueryProbe::failed(format!(
            "live IPC query {query:?} failed; inspect locally with jwm-tool"
        )),
''',
    "IPC transport error redaction",
)
support = replace_once(
    support,
    '''        let mut status = json!({
            "config": {
                "path": "/home/alice/.config/jwm/config_x11.toml",
                "diagnostics": {
                    "error_count": 1,
                    "issues": [{"detail": "/home/alice/private"}]
                },
                "reload": {"last_error": "failed under /home/alice"}
            }
        });
''',
    '''        let mut status = json!({
            "health": {
                "reasons": [
                    "configuration has 1 error(s)",
                    "last configuration reload failed: /home/alice/private"
                ]
            },
            "config": {
                "path": "/home/alice/.config/jwm/config_x11.toml",
                "diagnostics": {
                    "error_count": 1,
                    "issues": [{"detail": "/home/alice/private"}]
                },
                "reload": {"last_error": "failed under /home/alice"}
            }
        });
''',
    "privacy fixture health reason",
)
support = replace_once(
    support,
    '''        assert_eq!(status["config"]["reload"]["last_error_present"], true);
''',
    '''        assert_eq!(status["config"]["reload"]["last_error_present"], true);
        assert_eq!(
            status["health"]["reasons"][1],
            "last configuration reload failed (detail redacted)"
        );
''',
    "privacy health assertion",
)
support_path.write_text(support)

roadmap_path = Path("docs/roadmap.md")
roadmap = roadmap_path.read_text()
roadmap = replace_once(
    roadmap,
    '- [ ] Commit and enforce `Cargo.lock` for reproducible application builds.',
    '- [x] Commit and enforce `Cargo.lock` for reproducible application builds.',
    "lockfile roadmap item",
)
roadmap = replace_once(
    roadmap,
    '- [ ] Pin git dependencies to reviewed revisions or release tags.',
    '- [x] Pin git dependencies to reviewed revisions or release tags.',
    "git revision roadmap item",
)
roadmap_path.write_text(roadmap)

support_docs_path = Path("docs/support-bundles.md")
support_docs = support_docs_path.read_text()
support_docs = replace_once(
    support_docs,
    '- the versioned `DoctorReport` used by `jwm --doctor --json`;\n- optional `get_status` and `get_capabilities` IPC response data.',
    '- a support-safe projection of the versioned startup doctor report;\n- optional, redacted `get_status` data and the `get_capabilities` catalog.',
    "support schema description",
)
support_docs = replace_once(
    support_docs,
    '''- `HOME`, `PATH`, and other user paths;
- D-Bus addresses and authentication material;
- process command lines;
- window titles and application content;
- all environment variables outside the documented allowlist.
''',
    '''- `HOME`, `PATH`, and other user paths;
- configuration, executable, runtime-socket, and runtime-directory paths;
- raw configuration issue bodies, reload errors, and IPC transport errors;
- D-Bus addresses and authentication material;
- process command lines;
- window titles and application content;
- all environment variables outside the documented allowlist.
''',
    "support privacy details",
)
support_docs_path.write_text(support_docs)

readme_path = Path("README.md")
readme = readme_path.read_text()
readme = replace_once(
    readme,
    '''small environment allowlist and deliberately excludes HOME, PATH, D-Bus
addresses, process command lines, window titles, and arbitrary environment
variables. Review [support bundles](docs/support-bundles.md) before attaching a
''',
    '''small environment allowlist and redacts configuration, executable, runtime,
and IPC error details; it excludes HOME, PATH, D-Bus addresses, process command
lines, window titles, and arbitrary environment variables. Review
[support bundles](docs/support-bundles.md) before attaching a
''',
    "README support privacy",
)
readme_path.write_text(readme)
