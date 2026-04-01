mod account_status;
mod bridge;
mod codex_transport;
mod config;
mod daemon_state;
mod desktop;
mod git_handler;
mod json_rpc;
mod macos_launch_agent;
mod package_version_status;
mod push;
mod qr;
mod rollout;
mod secure_device_state;
mod secure_transport;
mod session_state;
mod voice;
mod workspace;

use clap::{Parser, Subcommand};
use color_eyre::eyre::Result;

use crate::bridge::start_bridge;
use crate::macos_launch_agent::{
    print_macos_bridge_pairing_qr, print_macos_bridge_service_status, reset_macos_bridge_pairing,
    run_macos_bridge_service, start_macos_bridge_service, stop_macos_bridge_service,
};
use crate::session_state::open_last_active_thread;

#[derive(Debug, Parser)]
#[command(name = "remodex")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Up,
    Run,
    RunService,
    Start,
    Stop,
    Status,
    ResetPairing,
    Resume,
    Watch { thread_id: Option<String> },
    Version,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Up);

    match command {
        Command::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
        }
        Command::Up => {
            if cfg!(target_os = "macos") {
                let result = start_macos_bridge_service(true).await?;
                print_macos_bridge_pairing_qr(result.pairing_session.as_ref())?;
            } else {
                start_bridge(Default::default()).await?;
            }
        }
        Command::Run => {
            start_bridge(Default::default()).await?;
        }
        Command::RunService => {
            run_macos_bridge_service().await?;
        }
        Command::Start => {
            ensure_macos("start")?;
            start_macos_bridge_service(false).await?;
            println!("[remodex] macOS bridge service is running.");
        }
        Command::Stop => {
            ensure_macos("stop")?;
            stop_macos_bridge_service()?;
            println!("[remodex] macOS bridge service stopped.");
        }
        Command::Status => {
            ensure_macos("status")?;
            print_macos_bridge_service_status()?;
        }
        Command::ResetPairing => {
            if cfg!(target_os = "macos") {
                reset_macos_bridge_pairing()?;
                println!(
                    "[remodex] Stopped the macOS bridge service and cleared the saved pairing state. Run `remodex up` to pair again."
                );
            } else {
                secure_device_state::reset_bridge_device_state()?;
                println!(
                    "[remodex] Cleared the saved pairing state. Run `remodex up` to pair again."
                );
            }
        }
        Command::Resume => {
            let state = open_last_active_thread(None)?;
            println!(
                "[remodex] Opened last active thread: {} ({})",
                state.thread_id,
                state.source.unwrap_or_else(|| "unknown".to_owned())
            );
        }
        Command::Watch { thread_id } => {
            rollout::watch_thread_rollout(thread_id.as_deref())?;
        }
    }

    Ok(())
}

fn ensure_macos(command: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        return Ok(());
    }

    Err(color_eyre::eyre::eyre!(
        "[remodex] `{command}` is only available on macOS. Use `remodex up` or `remodex run` for the foreground bridge on this OS."
    ))
}
