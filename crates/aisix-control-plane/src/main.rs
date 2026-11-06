//! `aisix-control-plane [serve]`: configuration from the environment (see
//! `aisix_control_plane::config`). A refused boot prints one line naming the
//! variable to fix and exits 1.

use std::process::ExitCode;

use aisix_control_plane::config::CpConfig;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: aisix-control-plane [serve]";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let subcommand = match args.as_slice() {
        [] => "serve",
        [one] => one.to_str().unwrap_or(""),
        _ => "",
    };
    if subcommand != "serve" {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }

    let cfg = match CpConfig::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("aisix-control-plane: {e}");
            return ExitCode::FAILURE;
        }
    };
    match aisix_control_plane::run(cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("aisix-control-plane: {e}");
            ExitCode::FAILURE
        }
    }
}
