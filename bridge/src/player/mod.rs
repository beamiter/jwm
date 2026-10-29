mod backend;
mod cli;
mod eva;
mod model;
mod service;
mod ui;

pub async fn run(eva_request: Option<String>) -> Result<(), String> {
    let eva_request = eva_request.map(|text| eva::parse(&text)).transpose()?;
    match cli::parse(std::env::args_os().skip(1))? {
        cli::Cli::Help => {
            println!("{}", cli::HELP);
            Ok(())
        }
        cli::Cli::Version => {
            println!("jwm-player {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        cli::Cli::Open(open) => service::run(open, eva_request).await,
    }
}

pub async fn notify_error(error: &str) {
    let body: String = error
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(1024)
        .collect();
    // The shared client has bounded connect/read/write timeouts. Keep it on
    // the blocking pool: neither media commands nor bus discovery wait here.
    let _ = tokio::task::spawn_blocking(move || {
        let ipc = crate::jwm_ipc::JwmIpc::new();
        ipc.command(
            "notify",
            serde_json::json!({
                "app": "JWM Player", "title": "JWM Player error",
                "body": body, "urgency": 2, "timeout_ms": 8000,
            }),
        )
    })
    .await;
}
