#!/usr/bin/env python3
from pathlib import Path

path = Path("tools/jwm_support.rs")
text = path.read_text()

old_mapping = '''                let id = check.id;
                SupportDoctorCheck {
                    status: check.status,
                    detail: sanitize_doctor_detail(&id, check.detail),
                    id,
                    summary: sanitize_reported_value(&check.summary),
                    hint: check.hint.map(|hint| sanitize_reported_value(&hint)),
                }
'''
new_mapping = '''                let id = check.id;
                let summary = sanitize_doctor_summary(&id, &check.summary);
                SupportDoctorCheck {
                    status: check.status,
                    detail: sanitize_doctor_detail(&id, check.detail),
                    id,
                    summary,
                    hint: check.hint.map(|hint| sanitize_reported_value(&hint)),
                }
'''
if old_mapping not in text:
    raise SystemExit("support doctor mapping changed unexpectedly")
text = text.replace(old_mapping, new_mapping, 1)

marker = '''fn sanitize_doctor_detail(id: &str, detail: Option<String>) -> Option<String> {
'''
helper = '''fn sanitize_doctor_summary(id: &str, summary: &str) -> String {
    if id == "command.status_bar" && summary.starts_with("Configured status bar ") {
        if summary.ends_with(" is executable") {
            return "Configured status bar is executable".to_string();
        }
        if summary.ends_with(" is not executable from PATH") {
            return "Configured status bar is not executable from PATH".to_string();
        }
        return "Configured status bar check completed".to_string();
    }
    sanitize_reported_value(summary)
}

'''
if marker not in text:
    raise SystemExit("doctor detail helper changed unexpectedly")
text = text.replace(marker, helper + marker, 1)

test_marker = '''    #[test]
    fn live_status_drops_config_paths_issue_details_and_reload_errors() {
'''
test = '''    #[test]
    fn doctor_report_redacts_status_bar_commands_from_summaries() {
        let report = DoctorReport {
            schema_version: 1,
            backend: "x11rb".to_string(),
            status: DoctorStatus::Pass,
            summary: DoctorSummary {
                passed: 1,
                warnings: 0,
                errors: 0,
            },
            checks: vec![DoctorCheck {
                status: DoctorStatus::Pass,
                id: "command.status_bar".to_string(),
                summary: "Configured status bar \\"/home/alice/private-bar\\" is executable"
                    .to_string(),
                detail: Some("/home/alice/private-bar".to_string()),
                hint: None,
                config_diagnostics: None,
            }],
        };

        let encoded = serde_json::to_string(&SupportDoctorReport::from(report)).unwrap();
        assert!(!encoded.contains("/home/alice"));
        assert!(encoded.contains("Configured status bar is executable"));
    }

'''
if test_marker not in text:
    raise SystemExit("support tests changed unexpectedly")
text = text.replace(test_marker, test + test_marker, 1)

path.write_text(text)
