use std::sync::Arc;

use anyhow::{Result, bail};
use tulma_core::app::Tulma;
use tulma_core::store::{KeychainSecrets, MemorySecrets, Paths, SecretStore};
use tulma_core::{bridge, control};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage:
  tulma daemon [--data-dir DIR] [--memory-secrets]   run the gateway, controlled over stdin/stdout
  tulma connect [--url URL]                           stdio bridge to a running Tulma
  tulma --version";

fn init_tracing(default: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("TULMA_LOG").unwrap_or_else(|_| EnvFilter::new(default)))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

fn flag_value(args: &[String], flag: &str) -> Result<Option<String>> {
    match args.iter().position(|a| a == flag) {
        None => Ok(None),
        Some(i) => match args.get(i + 1) {
            Some(v) => Ok(Some(v.clone())),
            None => bail!("{flag} needs a value\n{USAGE}"),
        },
    }
}

async fn daemon(args: &[String]) -> Result<()> {
    init_tracing("info,rmcp=warn");
    let paths = match flag_value(args, "--data-dir")? {
        Some(dir) => Paths::new(dir),
        None => Paths::from_env(),
    };
    let secrets: Arc<dyn SecretStore> = if args.iter().any(|a| a == "--memory-secrets") {
        Arc::new(MemorySecrets::default())
    } else {
        Arc::new(KeychainSecrets)
    };
    let app = Tulma::open(paths, secrets)?;
    app.start().await;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = control::serve(app.clone(), tokio::io::stdin(), tokio::io::stdout()) => result?,
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT"),
    }
    tracing::info!("shutting down");
    app.shutdown().await;
    Ok(())
}

async fn connect(args: &[String]) -> Result<()> {
    init_tracing("warn");
    let url = match flag_value(args, "--url")? {
        Some(url) => url,
        None => bridge::default_url(&Paths::from_env()),
    };
    bridge::run(&url, rmcp_stdio()).await
}

fn rmcp_stdio() -> (tokio::io::Stdin, tokio::io::Stdout) {
    (tokio::io::stdin(), tokio::io::stdout())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = match args.first().map(String::as_str) {
        Some("daemon") => runtime.block_on(daemon(&args[1..])),
        Some("connect") => runtime.block_on(connect(&args[1..])),
        Some("--version" | "-V") => {
            println!("tulma {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    // A blocked stdin read never finishes on its own; don't let it hold the process open.
    runtime.shutdown_timeout(std::time::Duration::from_millis(500));
    result
}
