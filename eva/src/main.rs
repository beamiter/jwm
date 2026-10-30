use clap::{Parser, Subcommand};
use jwm_eva::{config::Config, daemon, executor};
use serde_json::{Value, json};
use std::{io, path::PathBuf};
#[derive(Parser)]
#[command(name = "jwm-eva", version, about = "JWM 原生语音助手")]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Daemon,
    Toggle,
    Cancel,
    /// Toggle cloud mini wake-word standby (separate from the active conversation).
    Wake {
        #[arg(value_enum, default_value = "toggle")]
        action: WakeAction,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Capabilities {
        #[arg(long)]
        json: bool,
    },
    Doctor {
        #[arg(long)]
        hold: bool,
        /// Explicitly verify both configured cloud sessions without audio or inference.
        #[arg(long)]
        live: bool,
        #[arg(long)]
        json: bool,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    DryRun {
        #[arg(long)]
        text: String,
    },
    Undo,
    /// Offline latency, response-count and cost metrics from Eva's own
    /// telemetry records (the user journal by default, or a saved file).
    Report {
        /// journalctl --since expression, for example "today" or "-2h".
        #[arg(long, default_value = "today")]
        since: String,
        /// Read telemetry lines from a file instead of the user journal.
        #[arg(long)]
        input: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}
#[derive(Subcommand)]
enum ConfigCommand {
    /// Preview or apply a recognized historical bundled profile migration.
    Migrate {
        #[arg(long, conflicts_with = "write")]
        check: bool,
        /// Close editors/other configuration writers first; creates a private backup.
        #[arg(long, conflicts_with = "check")]
        write: bool,
    },
}
#[derive(Clone, clap::ValueEnum)]
enum WakeAction {
    Toggle,
    Enable,
    Disable,
}

fn print(value: Value) {
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    let args = Cli::parse();
    let hold = matches!(&args.command, Command::Doctor { hold: true, .. });
    let live = matches!(&args.command, Command::Doctor { live: true, .. });
    let result = run(args).await;
    if let Err(error) = &result {
        eprintln!("jwm-eva: {error}");
    }
    if hold {
        eprintln!(
            "\n按 Enter 关闭诊断窗口。{}",
            if live {
                "云端检查仅用于会话配置，不会采音或提交推理。"
            } else {
                "诊断未打开麦克风或云端会话。"
            }
        );
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
    if result.is_err() {
        std::process::exit(1);
    }
}
async fn run(args: Cli) -> io::Result<()> {
    // Cancellation and inspection remain available even if a newly edited
    // configuration is invalid; they do not start provider or audio work.
    let c = if matches!(
        &args.command,
        Command::Cancel
            | Command::Config { .. }
            | Command::Wake {
                action: WakeAction::Disable
            }
            | Command::Status { .. }
            | Command::Capabilities { .. }
            | Command::Report { .. }
    ) {
        Config::default()
    } else {
        match Config::load(args.config.as_deref()) {
            Ok(config) => config,
            Err(error) if matches!(&args.command, Command::Doctor { .. }) => {
                print(
                    json!({"configuration":"invalid","ready":false,"microphone_opened":false,"cloud_connected":false,
                    "live_requested":matches!(&args.command,Command::Doctor{live:true,..}),
                    "checks":[{"id":"configuration","state":"error","message":"配置文件不可读取、格式无效或含不支持的设置。","recommendation":"检查 --config 路径、权限和示例配置；旧配置可先运行 config migrate --check。"}]}),
                );
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    };
    match args.command {
        Command::Config {
            command: ConfigCommand::Migrate { write, .. },
        } => {
            use jwm_eva::migration::{Mode, Status, migrate};
            let path = args.config.unwrap_or_else(Config::path);
            let report = migrate(&path, if write { Mode::Write } else { Mode::Check })?;
            let manual = report.status == Status::ManualReview;
            print(serde_json::to_value(&report).map_err(io::Error::other)?);
            if manual {
                return Err(io::Error::other(
                    "configuration requires manual review; no file was changed",
                ));
            }
        }
        Command::Daemon => daemon::run(c).await?,
        Command::Toggle => {
            daemon::ensure_running(&c).await?;
            print(executor::command("assistant_control", json!({"action":"toggle"})).await?);
        }
        Command::Wake { action } => {
            if !matches!(action, WakeAction::Disable) {
                daemon::ensure_running(&c).await?;
            }
            let action = match action {
                WakeAction::Toggle => "wake_toggle",
                WakeAction::Enable => "wake_enable",
                WakeAction::Disable => "wake_disable",
            };
            print(executor::command("assistant_control", json!({"action":action})).await?);
        }
        Command::Cancel => match daemon::control(json!({"command":"cancel"})).await {
            Ok(value) => print(value),
            Err(_) => {
                let _ =
                    executor::command("assistant_control", json!({"action":"wake_disable"})).await;
                print(executor::command("assistant_control", json!({"action":"cancel"})).await?);
            }
        },
        Command::Status { json: _ } => {
            print(daemon::control(json!({"command":"status"})).await?);
        }
        Command::Capabilities { json: _ } => {
            let cap = executor::query("get_capabilities", json!({})).await?;
            fn strings(v: &Value) -> Vec<String> {
                v.as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default()
            }
            let report = jwm_eva_core::actions::coverage(
                &strings(&cap["commands"]),
                &strings(&cap["queries"]),
                &strings(&cap["subscription_topics"]),
            );
            let schema = executor::query("get_command_schema", json!({})).await?;
            print(
                json!({"runtime":cap,"coverage":report,"command_schema":schema,"catalog_registration_is_not_live_acceptance":true}),
            );
        }
        Command::Doctor { live, .. } => {
            // Probes are independent and read-only. No activation, capture,
            // playback, provider or inference starts without the explicit flag.
            let cost_config = c.clone();
            let input_device = c.audio.input_device.clone();
            let (costs, audio, cap, context, schema, assistant, key) = tokio::join!(
                tokio::task::spawn_blocking(move || jwm_eva::cost_report::snapshot(&cost_config)),
                tokio::task::spawn_blocking(move || jwm_eva::audio::inspect(&input_device)),
                executor::query("get_capabilities", json!({})),
                executor::query("get_execution_context", json!({})),
                executor::query("get_command_schema", json!({})),
                executor::query("get_assistant_status", json!({})),
                c.api_key(),
            );
            let audio = audio.ok().and_then(Result::ok);
            let report = jwm_eva::diagnostics::evaluate(
                &c,
                jwm_eva::diagnostics::Inputs {
                    credential_available: key.is_ok(),
                    capabilities: cap.as_ref().ok(),
                    schema: schema.as_ref().ok(),
                    context: context.as_ref().ok(),
                    assistant: assistant.as_ref().ok(),
                    audio: audio.as_ref(),
                },
            );
            let live_report = if live {
                Some(match key.as_ref() {
                    Ok(key) => jwm_eva::doctor::check_live(&c, key).await,
                    Err(_) => jwm_eva::doctor::LiveReport::credential_unavailable(&c),
                })
            } else {
                None
            };
            let ready = report.ready && live_report.as_ref().is_none_or(|report| report.ready);
            let cloud_connected = live_report.as_ref().is_some_and(|report| {
                report.checks.iter().any(|check| {
                    check.attempted && !matches!(check.phase, jwm_eva::doctor::Phase::Connect)
                })
            });
            print(
                json!({"configuration":"valid","config_path":args.config.unwrap_or_else(Config::path),
                "ready":ready,"model":c.openai.model,"wake_model":c.wake_word.model,
                "turn_detection":c.turn_detection.kind,"endpoint_eagerness":c.turn_detection.eagerness,"noise_reduction":c.audio.noise_reduction,
                "credential_source":c.openai.api_key_source,"credential_available":key.is_ok(),
                "audio_backend":audio.as_ref().map(|audio|audio.backend),
                "audio_input_available":audio.as_ref().is_some_and(|audio|audio.input_available),
                "audio_output_available":audio.as_ref().is_some_and(|audio|audio.output_available),
                "native_audio_connected":audio.is_some(),"native_pipewire_connected":audio.as_ref().filter(|audio|audio.backend=="pipewire").map(|_|true),
                "ipc_connected":cap.is_ok(),"execution_protocol":context.as_ref().ok().map(|value|value["protocol_version"].clone()),
                "protocol_ready":report.protocol_ready,"wake_protocol_ready":report.wake_protocol_ready,"checks":report.checks,
                "microphone_opened":false,"cloud_connected":cloud_connected,"live_requested":live,"live":live_report,
                "local_costs":costs.ok()}),
            );
            if !ready {
                return Err(io::Error::other(
                    "doctor found unavailable or incompatible components; see checks and recommendations",
                ));
            }
        }
        Command::DryRun { text } => {
            daemon::ensure_running(&c).await?;
            eprintln!(
                "Using {} for one bounded turn (up to {} discovery/planning responses); desktop execution is disabled.",
                c.openai.model, c.execution.max_planning_rounds
            );
            print(daemon::control(json!({"command":"dry_run","text":text})).await?);
        }
        Command::Undo => {
            daemon::ensure_running(&c).await?;
            print(daemon::control(json!({"command":"undo"})).await?);
        }
        Command::Report { since, input, json } => {
            let lines: Vec<String> = match input {
                Some(path) => std::fs::read_to_string(path)?
                    .lines()
                    .map(str::to_owned)
                    .collect(),
                None => {
                    // `since` is passed as a separate argv item; refuse values
                    // that journalctl would read as another option.
                    if since.is_empty()
                        || since.starts_with('-')
                            && since.len() > 1
                            && !since[1..]
                                .chars()
                                .next()
                                .is_some_and(|c| c.is_ascii_digit())
                    {
                        return Err(io::Error::other("invalid --since expression"));
                    }
                    let output = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        tokio::process::Command::new("journalctl")
                            .args([
                                "--user",
                                "-u",
                                "jwm-eva.service",
                                "-o",
                                "cat",
                                "--no-pager",
                                "--since",
                                &since,
                            ])
                            .kill_on_drop(true)
                            .output(),
                    )
                    .await
                    .map_err(|_| io::Error::other("journalctl timeout"))??;
                    if !output.status.success() {
                        return Err(io::Error::other(
                            "journalctl failed; use --input FILE with saved telemetry lines",
                        ));
                    }
                    String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .map(str::to_owned)
                        .collect()
                }
            };
            let report = jwm_eva::report::analyze(lines);
            if json {
                print(report);
            } else {
                print!("{}", jwm_eva::report::render(&report, &since));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cloud_diagnostics_and_config_writes_require_explicit_flags() {
        let ordinary = Cli::try_parse_from(["jwm-eva", "doctor", "--json"]).unwrap();
        assert!(matches!(
            ordinary.command,
            Command::Doctor { live: false, .. }
        ));
        let live = Cli::try_parse_from(["jwm-eva", "doctor", "--live", "--hold"]).unwrap();
        assert!(matches!(
            live.command,
            Command::Doctor {
                live: true,
                hold: true,
                ..
            }
        ));
        let preview = Cli::try_parse_from(["jwm-eva", "config", "migrate"]).unwrap();
        assert!(matches!(
            preview.command,
            Command::Config {
                command: ConfigCommand::Migrate { write: false, .. }
            }
        ));
        assert!(
            Cli::try_parse_from(["jwm-eva", "config", "migrate", "--check", "--write"]).is_err()
        );
    }
    #[tokio::test]
    async fn invalid_selected_config_stops_live_diagnostics_before_any_probe() {
        let path = std::env::temp_dir().join(format!(
            "eva-missing-doctor-{}-{}.toml",
            std::process::id(),
            jwm_eva::store::wall_ms()
        ));
        let error = run(Cli {
            config: Some(path.clone()),
            command: Command::Doctor {
                live: true,
                hold: false,
                json: true,
            },
        })
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!path.exists());
    }
}
