use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{eyre, Result};

use crate::config::read_bridge_config;
use crate::daemon_state::{
    clear_bridge_status, clear_pairing_session, ensure_remodex_logs_dir, ensure_remodex_state_dir,
    read_bridge_status, read_pairing_session, resolve_bridge_stderr_log_path,
    resolve_bridge_stdout_log_path, resolve_remodex_state_dir, write_bridge_status,
    write_daemon_config, PairingSession,
};

const SERVICE_LABEL: &str = "com.remodex.bridge";

pub struct StartMacOsBridgeServiceResult {
    pub pairing_session: Option<PairingSession>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LaunchAgentStatus {
    pid: Option<u32>,
    program: String,
    arguments: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaunchAgentTarget {
    program: String,
    arguments: Vec<String>,
}

pub async fn run_macos_bridge_service() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(eyre!(
            "The macOS bridge service is only available on macOS."
        ));
    }
    let config = read_bridge_config();
    if config.relay_url.trim().is_empty() {
        clear_pairing_session();
        write_bridge_status(
            "error",
            "error",
            std::process::id(),
            "No relay URL configured for the macOS bridge service.",
        )?;
        eprintln!("[remodex] No relay URL configured for the macOS bridge service.");
        return Ok(());
    }
    crate::bridge::start_bridge(crate::bridge::StartBridgeOptions {
        config: None,
        print_pairing_qr: false,
    })
    .await
}

pub async fn start_macos_bridge_service(
    wait_for_pairing: bool,
) -> Result<StartMacOsBridgeServiceResult> {
    if !cfg!(target_os = "macos") {
        return Err(eyre!(
            "The macOS bridge service is only available on macOS."
        ));
    }
    let config = read_bridge_config();
    if config.relay_url.trim().is_empty() {
        return Err(eyre!(
            "No relay URL configured. Run ./run-local-remodex.sh or set REMODEX_RELAY before enabling the macOS bridge service."
        ));
    }

    write_daemon_config(&config)?;
    clear_pairing_session();
    clear_bridge_status();
    ensure_remodex_state_dir()?;
    ensure_remodex_logs_dir()?;

    let plist_path = write_launch_agent_plist()?;
    restart_launch_agent(&plist_path)?;

    let pairing_session = if wait_for_pairing {
        wait_for_fresh_pairing_session().await
    } else {
        None
    };

    Ok(StartMacOsBridgeServiceResult { pairing_session })
}

pub fn stop_macos_bridge_service() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(eyre!(
            "The macOS bridge service is only available on macOS."
        ));
    }
    let _ = bootout_launch_agent();
    clear_pairing_session();
    clear_bridge_status();
    Ok(())
}

pub fn reset_macos_bridge_pairing() -> Result<()> {
    stop_macos_bridge_service()?;
    crate::secure_device_state::reset_bridge_device_state()?;
    Ok(())
}

pub fn print_macos_bridge_service_status() -> Result<()> {
    let installed = resolve_launch_agent_plist_path().exists();
    let bridge_status = read_bridge_status();
    let pairing_session = read_pairing_session();
    let launch_agent_status = read_launch_agent_status()?;
    println!("[remodex] Service label: {SERVICE_LABEL}");
    println!(
        "[remodex] Installed: {}",
        if installed { "yes" } else { "no" }
    );
    println!(
        "[remodex] Launchd loaded: {}",
        if launch_agent_status.pid.is_some() {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "[remodex] PID: {}",
        launch_agent_status
            .pid
            .or_else(|| bridge_status.as_ref().map(|status| status.pid))
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "unknown".to_owned())
    );
    println!(
        "[remodex] Bridge state: {}",
        bridge_status
            .as_ref()
            .map(|status| status.state.as_str())
            .unwrap_or("unknown")
    );
    println!(
        "[remodex] Connection: {}",
        bridge_status
            .as_ref()
            .map(|status| status.connection_status.as_str())
            .unwrap_or("unknown")
    );
    println!(
        "[remodex] Launchd program: {}",
        non_empty_or_unknown(&launch_agent_status.program)
    );
    println!(
        "[remodex] Launchd arguments: {}",
        if launch_agent_status.arguments.is_empty() {
            "unknown".to_owned()
        } else {
            launch_agent_status.arguments.join(" ")
        }
    );
    println!(
        "[remodex] Bridge runtime: {}",
        bridge_status
            .as_ref()
            .map(|status| non_empty_or_unknown(&status.runtime.runtime_kind))
            .unwrap_or("unknown".to_owned())
    );
    println!(
        "[remodex] Bridge source: {}",
        bridge_status
            .as_ref()
            .map(|status| non_empty_or_unknown(&status.runtime.runtime_source))
            .unwrap_or("unknown".to_owned())
    );
    println!(
        "[remodex] Bridge executable: {}",
        bridge_status
            .as_ref()
            .map(|status| non_empty_or_unknown(&status.runtime.runtime_executable))
            .unwrap_or("unknown".to_owned())
    );
    println!(
        "[remodex] Pairing payload: {}",
        pairing_session
            .as_ref()
            .map(|session| session.created_at.as_str())
            .unwrap_or("none")
    );
    println!(
        "[remodex] Stdout log: {}",
        resolve_bridge_stdout_log_path().display()
    );
    println!(
        "[remodex] Stderr log: {}",
        resolve_bridge_stderr_log_path().display()
    );
    Ok(())
}

pub fn print_macos_bridge_pairing_qr(pairing_session: Option<&PairingSession>) -> Result<()> {
    let pairing_session = pairing_session
        .cloned()
        .or_else(read_pairing_session)
        .ok_or_else(|| eyre!("The macOS bridge service did not publish a pairing payload yet."))?;
    crate::qr::print_qr(&pairing_session.pairing_payload)
}

fn resolve_launch_agent_plist_path() -> PathBuf {
    dirs::home_dir()
        .unwrap()
        .join("Library")
        .join("LaunchAgents")
        .join("com.remodex.bridge.plist")
}

fn write_launch_agent_plist() -> Result<PathBuf> {
    let path = resolve_launch_agent_plist_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = resolve_launch_agent_target(std::env::current_exe().ok(), &manifest_dir);
    let arguments = build_launch_agent_arguments_xml(&target);
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{SERVICE_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    {arguments}
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>WorkingDirectory</key>
  <string>{}</string>
  <key>StandardOutPath</key>
  <string>{}</string>
  <key>StandardErrorPath</key>
  <string>{}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{}</string>
    <key>REMODEX_DEVICE_STATE_DIR</key>
    <string>{}</string>
  </dict>
</dict>
</plist>
"#,
        dirs::home_dir().unwrap().display(),
        resolve_bridge_stdout_log_path().display(),
        resolve_bridge_stderr_log_path().display(),
        std::env::var("PATH").unwrap_or_default(),
        resolve_remodex_state_dir().display(),
    );
    fs::write(&path, plist)?;
    Ok(path)
}

fn restart_launch_agent(plist_path: &std::path::Path) -> Result<()> {
    let _ = bootout_launch_agent();
    let bootstrap = Command::new("launchctl")
        .args([
            "bootstrap",
            &format!("gui/{}", current_uid()?),
            &plist_path.display().to_string(),
        ])
        .status()?;
    if !bootstrap.success() {
        return Err(eyre!(
            "Failed to start the macOS bridge service with launchctl."
        ));
    }
    Ok(())
}

fn bootout_launch_agent() -> Result<()> {
    let uid = current_uid()?;
    let plist_path = resolve_launch_agent_plist_path();
    let targets = [
        vec![
            "bootout".to_owned(),
            format!("gui/{uid}"),
            plist_path.display().to_string(),
        ],
        vec!["bootout".to_owned(), format!("gui/{uid}/{SERVICE_LABEL}")],
    ];

    for target in targets {
        let _ = Command::new("launchctl").args(&target).status();
    }

    Ok(())
}

async fn wait_for_fresh_pairing_session() -> Option<PairingSession> {
    let started_at = std::time::Instant::now();
    while started_at.elapsed() < std::time::Duration::from_secs(10) {
        if let Some(pairing_session) = read_pairing_session() {
            return Some(pairing_session);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    None
}

fn read_launch_agent_status() -> Result<LaunchAgentStatus> {
    let output = Command::new("launchctl")
        .args([
            "print",
            &format!("gui/{}/{}", current_uid()?, SERVICE_LABEL),
        ])
        .output()?;
    if !output.status.success() {
        return Ok(LaunchAgentStatus::default());
    }
    Ok(parse_launch_agent_status(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn current_uid() -> Result<String> {
    let output = Command::new("id").arg("-u").output()?;
    if !output.status.success() {
        return Err(eyre!(
            "Failed to resolve the current user id for launchctl."
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn resolve_launch_agent_target(
    current_exe: Option<PathBuf>,
    manifest_dir: &Path,
) -> LaunchAgentTarget {
    if let Some(current_exe) = current_exe {
        return LaunchAgentTarget {
            program: current_exe.display().to_string(),
            arguments: vec!["run-service".to_owned()],
        };
    }

    let debug_binary = manifest_dir.join("target").join("debug").join("remodex");
    if debug_binary.exists() {
        return LaunchAgentTarget {
            program: debug_binary.display().to_string(),
            arguments: vec!["run-service".to_owned()],
        };
    }

    LaunchAgentTarget {
        program: "cargo".to_owned(),
        arguments: vec![
            "run".to_owned(),
            "--manifest-path".to_owned(),
            manifest_dir.join("Cargo.toml").display().to_string(),
            "--bin".to_owned(),
            "remodex".to_owned(),
            "--".to_owned(),
            "run-service".to_owned(),
        ],
    }
}

fn build_launch_agent_arguments_xml(target: &LaunchAgentTarget) -> String {
    std::iter::once(&target.program)
        .chain(target.arguments.iter())
        .map(|argument| format!("<string>{argument}</string>"))
        .collect::<Vec<_>>()
        .join("\n    ")
}

fn parse_launch_agent_status(stdout: &str) -> LaunchAgentStatus {
    let mut status = LaunchAgentStatus::default();
    let mut inside_arguments = false;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("pid = ") {
            status.pid = value.trim_end_matches(';').parse().ok();
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("program = ") {
            status.program = value.to_owned();
            continue;
        }
        if trimmed == "arguments = {" {
            inside_arguments = true;
            continue;
        }
        if inside_arguments {
            if trimmed == "}" {
                inside_arguments = false;
                continue;
            }
            if !trimmed.is_empty() {
                status.arguments.push(trimmed.to_owned());
            }
        }
    }

    status
}

fn non_empty_or_unknown(value: &str) -> String {
    if value.trim().is_empty() {
        "unknown".to_owned()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_launch_agent_arguments_xml, parse_launch_agent_status, resolve_launch_agent_target,
    };
    use std::path::Path;

    #[test]
    fn launch_agent_target_prefers_the_current_rust_executable() {
        let target = resolve_launch_agent_target(
            Some("/Users/praveen/remodex/phodex-bridge/target/debug/remodex".into()),
            Path::new("/Users/praveen/remodex/phodex-bridge"),
        );

        assert_eq!(
            target.program,
            "/Users/praveen/remodex/phodex-bridge/target/debug/remodex"
        );
        assert_eq!(target.arguments, vec!["run-service".to_owned()]);
    }

    #[test]
    fn launch_agent_arguments_xml_includes_program_and_runtime_arguments() {
        let target = resolve_launch_agent_target(
            Some("/Users/praveen/remodex/phodex-bridge/target/debug/remodex".into()),
            Path::new("/Users/praveen/remodex/phodex-bridge"),
        );

        let xml = build_launch_agent_arguments_xml(&target);

        assert!(xml.contains(
            "<string>/Users/praveen/remodex/phodex-bridge/target/debug/remodex</string>"
        ));
        assert!(xml.contains("<string>run-service</string>"));
    }

    #[test]
    fn launch_agent_status_parser_extracts_program_and_arguments() {
        let parsed = parse_launch_agent_status(
            r#"
gui/502/com.remodex.bridge = {
    pid = 77110;
    program = /Users/praveen/.local/share/fnm/node-versions/v22.11.0/installation/bin/node
    arguments = {
        /Users/praveen/.local/share/fnm/node-versions/v22.11.0/installation/bin/node
        /Users/praveen/code/remodex/phodex-bridge/bin/remodex.js
        run-service
    }
}
"#,
        );

        assert_eq!(parsed.pid, Some(77110));
        assert_eq!(
            parsed.program,
            "/Users/praveen/.local/share/fnm/node-versions/v22.11.0/installation/bin/node"
        );
        assert_eq!(
            parsed.arguments,
            vec![
                "/Users/praveen/.local/share/fnm/node-versions/v22.11.0/installation/bin/node"
                    .to_owned(),
                "/Users/praveen/code/remodex/phodex-bridge/bin/remodex.js".to_owned(),
                "run-service".to_owned(),
            ]
        );
    }
}
