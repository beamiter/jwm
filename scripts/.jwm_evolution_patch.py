#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one match, found {count}")
    return text.replace(old, new, 1)


support_path = Path("tools/jwm_support.rs")
support = support_path.read_text()
support = replace_once(
    support,
    "use jwm::doctor::{self, DoctorReport, DoctorStatus};",
    "use jwm::doctor::{self, DoctorReport, DoctorStatus, DoctorSummary};",
    "doctor imports",
)
support = replace_once(
    support,
    "use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};",
    "use std::os::unix::fs::OpenOptionsExt;",
    "unix fs imports",
)
support = replace_once(
    support,
    "    doctor: DoctorReport,",
    "    doctor: SupportDoctorReport,",
    "bundle doctor field",
)
support = replace_once(
    support,
    """#[derive(Debug, Serialize)]
struct LiveSnapshot {
""",
    """#[derive(Debug, Serialize)]
struct SupportDoctorReport {
    schema_version: u32,
    backend: String,
    status: DoctorStatus,
    summary: DoctorSummary,
    checks: Vec<SupportDoctorCheck>,
    config_diagnostics_included: bool,
}

#[derive(Debug, Serialize)]
struct SupportDoctorCheck {
    status: DoctorStatus,
    id: String,
    summary: String,
    #[serde(skip_serializing_if = \"Option::is_none\")]
    detail: Option<String>,
    #[serde(skip_serializing_if = \"Option::is_none\")]
    hint: Option<String>,
}

impl From<DoctorReport> for SupportDoctorReport {
    fn from(report: DoctorReport) -> Self {
        let checks = report
            .checks
            .into_iter()
            .map(|check| {
                let id = check.id;
                SupportDoctorCheck {
                    status: check.status,
                    detail: sanitize_doctor_detail(&id, check.detail),
                    id,
                    summary: sanitize_reported_value(&check.summary),
                    hint: check.hint.map(|hint| sanitize_reported_value(&hint)),
                }
            })
            .collect();

        Self {
            schema_version: report.schema_version,
            backend: report.backend,
            status: report.status,
            summary: report.summary,
            checks,
            config_diagnostics_included: false,
        }
    }
}

#[derive(Debug, Serialize)]
struct LiveSnapshot {
""",
    "support doctor structs",
)
support = replace_once(
    support,
    """    let doctor = doctor::diagnose(cli.backend);
    let live = (!cli.offline).then(collect_live_snapshot);
    let strict_failure = doctor.status == DoctorStatus::Error
        || live
            .as_ref()
            .is_some_and(|snapshot| !snapshot.health.success || !snapshot.capabilities.success);
""",
    """    let doctor_report = doctor::diagnose(cli.backend);
    let doctor_failed = doctor_report.status == DoctorStatus::Error;
    let doctor = SupportDoctorReport::from(doctor_report);
    let live = (!cli.offline).then(collect_live_snapshot);
    let strict_failure = doctor_failed
        || live
            .as_ref()
            .is_some_and(|snapshot| !snapshot.health.success || !snapshot.capabilities.success);
""",
    "doctor conversion",
)
support = replace_once(
    support,
    """fn sanitize_reported_value(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len().min(MAX_REPORTED_VALUE_CHARS));
    for character in value.chars().filter(|character| !character.is_control()) {
        if sanitized.chars().count() >= MAX_REPORTED_VALUE_CHARS {
            sanitized.push('…');
            break;
        }
        sanitized.push(character);
    }
    sanitized
}

fn collect_live_snapshot() -> LiveSnapshot {
""",
    """fn sanitize_reported_value(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len().min(MAX_REPORTED_VALUE_CHARS));
    let mut reported_characters = 0;
    for character in value.chars().filter(|character| !character.is_control()) {
        if reported_characters >= MAX_REPORTED_VALUE_CHARS {
            sanitized.push('…');
            break;
        }
        sanitized.push(character);
        reported_characters += 1;
    }
    sanitized
}

fn sanitize_doctor_detail(id: &str, detail: Option<String>) -> Option<String> {
    let detail = detail?;
    match id {
        \"backend.display\" | \"backend.host_display\" | \"backend.dri\" | \"session.dbus\" => {
            Some(sanitize_reported_value(&detail))
        }
        \"config.file\" => Some(\"<configuration path redacted>\".to_string()),
        \"runtime.xdg_runtime_dir\" => Some(\"<runtime directory details redacted>\".to_string()),
        \"command.jwm_tool\" | \"command.status_bar\" => {
            Some(\"<executable path redacted>\".to_string())
        }
        _ => None,
    }
}

fn sanitize_live_data(query: &str, data: &mut Value) {
    if query != \"get_status\" {
        return;
    }
    let Some(config) = data.get_mut(\"config\").and_then(Value::as_object_mut) else {
        return;
    };

    config.insert(
        \"path\".to_string(),
        Value::String(\"<configuration path redacted>\".to_string()),
    );
    if let Some(diagnostics) = config
        .get_mut(\"diagnostics\")
        .and_then(Value::as_object_mut)
    {
        diagnostics.remove(\"issues\");
        diagnostics.insert(\"issues_included\".to_string(), Value::Bool(false));
    }
    if let Some(reload) = config.get_mut(\"reload\").and_then(Value::as_object_mut) {
        let error_present = reload
            .get(\"last_error\")
            .is_some_and(|error| !error.is_null());
        reload.insert(\"last_error\".to_string(), Value::Null);
        reload.insert(
            \"last_error_present\".to_string(),
            Value::Bool(error_present),
        );
    }
}

fn collect_live_snapshot() -> LiveSnapshot {
""",
    "sanitizers",
)
support = replace_once(
    support,
    "        socket: Some(socket.display().to_string()),",
    "        socket: Some(\"<validated runtime socket redacted>\".to_string()),",
    "socket redaction",
)
support = replace_once(
    support,
    """fn query_ipc(socket: &Path, query: &str) -> QueryProbe {
    match query_ipc_value(socket, query) {
        Ok(response) => normalize_ipc_response(response),
        Err(error) => QueryProbe::failed(error),
    }
}
""",
    """fn query_ipc(socket: &Path, query: &str) -> QueryProbe {
    match query_ipc_value(socket, query) {
        Ok(response) => {
            let mut probe = normalize_ipc_response(response);
            if let Some(data) = probe.data.as_mut() {
                sanitize_live_data(query, data);
            }
            probe
        }
        Err(error) => QueryProbe::failed(error),
    }
}
""",
    "IPC sanitization",
)
support = replace_once(
    support,
    """mod tests {
    use super::*;
""",
    """mod tests {
    use super::*;
    use jwm::doctor::DoctorCheck;
    use std::os::unix::fs::PermissionsExt;
""",
    "test imports",
)
support = replace_once(
    support,
    """    #[test]
    fn bundle_files_are_private_and_replaced_atomically() {
""",
    """    #[test]
    fn doctor_report_redacts_paths_and_drops_config_diagnostics() {
        let report = DoctorReport {
            schema_version: 1,
            backend: \"x11rb\".to_string(),
            status: DoctorStatus::Pass,
            summary: DoctorSummary {
                passed: 1,
                warnings: 0,
                errors: 0,
            },
            checks: vec![DoctorCheck {
                status: DoctorStatus::Pass,
                id: \"config.file\".to_string(),
                summary: \"Configuration exists\".to_string(),
                detail: Some(\"/home/alice/.config/jwm/config_x11.toml\".to_string()),
                hint: None,
                config_diagnostics: None,
            }],
        };

        let encoded = serde_json::to_string(&SupportDoctorReport::from(report)).unwrap();
        assert!(!encoded.contains(\"/home/alice\"));
        assert!(encoded.contains(\"configuration path redacted\"));
        assert!(encoded.contains(\"\\\"config_diagnostics_included\\\":false\"));
    }

    #[test]
    fn live_status_drops_config_paths_issue_details_and_reload_errors() {
        let mut status = json!({
            \"config\": {
                \"path\": \"/home/alice/.config/jwm/config_x11.toml\",
                \"diagnostics\": {
                    \"error_count\": 1,
                    \"issues\": [{\"detail\": \"/home/alice/private\"}]
                },
                \"reload\": {\"last_error\": \"failed under /home/alice\"}
            }
        });

        sanitize_live_data(\"get_status\", &mut status);
        let encoded = serde_json::to_string(&status).unwrap();
        assert!(!encoded.contains(\"/home/alice\"));
        assert_eq!(status[\"config\"][\"diagnostics\"][\"issues_included\"], false);
        assert!(status[\"config\"][\"diagnostics\"].get(\"issues\").is_none());
        assert_eq!(status[\"config\"][\"reload\"][\"last_error\"], Value::Null);
        assert_eq!(status[\"config\"][\"reload\"][\"last_error_present\"], true);
    }

    #[test]
    fn bundle_files_are_private_and_replaced_atomically() {
""",
    "privacy tests",
)
support_path.write_text(support)

installer_path = Path("scripts/install_jwm_scripts.sh")
installer = installer_path.read_text()
installer = replace_once(
    installer,
    "for binary in jwm jwm-tool; do",
    "for binary in jwm jwm-tool jwm-support; do",
    "legacy binary cleanup",
)
installer = replace_once(
    installer,
    "# JWM 不使用 cargo install，避免把 jwm/jwm-tool 写入 cargo bin。",
    "# JWM 不使用 cargo install，避免把 jwm/jwm-tool/jwm-support 写入 cargo bin。",
    "installer comment",
)
installer = replace_once(
    installer,
    'info "同步 jwm, jwm-tool 到 /usr/local/bin ..."',
    'info "同步 jwm, jwm-tool, jwm-support 到 /usr/local/bin ..."',
    "installer status",
)
installer = replace_once(
    installer,
    '    install_system_binary "$target_dir/jwm-tool" /usr/local/bin\n',
    '    install_system_binary "$target_dir/jwm-tool" /usr/local/bin\n    install_system_binary "$target_dir/jwm-support" /usr/local/bin\n',
    "support binary install",
)
installer = replace_once(
    installer,
    'ok "jwm, jwm-tool 安装完成: /usr/local/bin（未安装到 cargo bin）"',
    'ok "jwm, jwm-tool, jwm-support 安装完成: /usr/local/bin（未安装到 cargo bin）"',
    "installer completion",
)
installer = replace_once(
    installer,
    "  - jwm / jwm-tool 只通过 cargo build 构建，并安装到 /usr/local/bin，不会安装到 cargo bin。",
    "  - jwm / jwm-tool / jwm-support 只通过 cargo build 构建，并安装到 /usr/local/bin，不会安装到 cargo bin。",
    "installer usage",
)
installer_path.write_text(installer)
