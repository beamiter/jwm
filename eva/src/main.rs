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
    },
    DryRun {
        #[arg(long)]
        text: String,
    },
    Undo,
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
    let hold = matches!(&args.command, Command::Doctor { hold: true });
    let result = run(args).await;
    if let Err(error) = &result {
        eprintln!("jwm-eva: {error}");
    }
    if hold {
        eprintln!("\n按 Enter 关闭诊断窗口。诊断不会打开麦克风或云端会话。");
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
            | Command::Wake {
                action: WakeAction::Disable
            }
            | Command::Status { .. }
            | Command::Capabilities { .. }
    ) {
        Config::default()
    } else {
        Config::load(args.config.as_deref())?
    };
    match args.command {
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
        Command::Doctor { .. } => {
            // Read the last atomic usage snapshot without taking the daemon's
            // Store lock. This remains useful on quota/credential failures.
            let cost_config = c.clone();
            let costs =
                tokio::task::spawn_blocking(move || jwm_eva::cost_report::snapshot(&cost_config))
                    .await
                    .map_err(|_| io::Error::other("local cost diagnostics failed"))?;
            let cap = executor::query("get_capabilities", json!({})).await;
            let key = c.api_key().await;
            let input_device = c.audio.input_device.clone();
            let audio = tokio::task::spawn_blocking(move || jwm_eva::audio::inspect(&input_device))
                .await
                .ok()
                .and_then(Result::ok);
            let audio_ready = audio
                .as_ref()
                .is_some_and(|a| a.input_available && a.output_available);
            let context = executor::query("get_execution_context", json!({})).await;
            let schema = executor::query("get_command_schema", json!({})).await;
            let assistant = executor::query("get_assistant_status", json!({})).await;
            let wake_protocol_ready = assistant.as_ref().is_ok_and(|v| {
                v["wm_instance_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
                    && v["wake_generation"].is_u64()
                    && v["assistant_generation"].is_u64()
                    && v["standby_allowed"].is_boolean()
            }) && context
                .as_ref()
                .is_ok_and(|v| v["selected_monitor_history"].is_array());
            let protocol_ready = context.as_ref().is_ok_and(|v| v["protocol_version"] == 1)
                && schema.as_ref().is_ok_and(|v| {
                    v["protocol_version"] == 1
                        && v["guarded_execution"] == true
                        && v["context_tokens"] == true
                });
            print(
                json!({"configuration":"valid","config_path":args.config.unwrap_or_else(Config::path),"model":c.openai.model,"wake_model":c.wake_word.model,"turn_detection":c.turn_detection.kind,"endpoint_eagerness":c.turn_detection.eagerness,"noise_reduction":c.audio.noise_reduction,"wake_protocol_ready":wake_protocol_ready,"credential_source":c.openai.api_key_source,"credential_available":key.is_ok(),"audio_backend":audio.as_ref().map(|a|a.backend),"audio_input_available":audio.as_ref().is_some_and(|a|a.input_available),"audio_output_available":audio.as_ref().is_some_and(|a|a.output_available),"native_audio_connected":audio.is_some(),"native_pipewire_connected":audio.as_ref().filter(|a|a.backend=="pipewire").map(|_|true),"ipc_connected":cap.is_ok(),"execution_protocol":context.as_ref().ok().map(|v|v["protocol_version"].clone()),"protocol_ready":protocol_ready,"microphone_opened":false,"cloud_connected":false,"local_costs":costs}),
            );
            if key.is_err()
                || !audio_ready
                || !protocol_ready
                || (c.wake_word.enabled && !wake_protocol_ready)
            {
                return Err(io::Error::other(
                    "doctor found missing dependencies, credentials or JWM protocol; no audio or cloud session was opened",
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
    }
    Ok(())
}
