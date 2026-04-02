use std::fs;
use std::path::PathBuf;

use color_eyre::eyre::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const DEFAULT_STATE_DIR_NAME: &str = ".remodex";
const DAEMON_CONFIG_FILE: &str = "daemon-config.json";
const PAIRING_SESSION_FILE: &str = "pairing-session.json";
const BRIDGE_STATUS_FILE: &str = "bridge-status.json";
const LOGS_DIR: &str = "logs";
const BRIDGE_STDOUT_LOG_FILE: &str = "bridge.stdout.log";
const BRIDGE_STDERR_LOG_FILE: &str = "bridge.stderr.log";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BridgeRuntimeMetadata {
    #[serde(rename = "runtimeKind", default)]
    pub runtime_kind: String,
    #[serde(rename = "runtimeSource", default)]
    pub runtime_source: String,
    #[serde(rename = "runtimeExecutable", default)]
    pub runtime_executable: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairingSession {
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "pairingPayload")]
    pub pairing_payload: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeStatus {
    pub state: String,
    #[serde(rename = "connectionStatus")]
    pub connection_status: String,
    pub pid: u32,
    #[serde(rename = "lastError")]
    pub last_error: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(flatten, default)]
    pub runtime: BridgeRuntimeMetadata,
}

pub fn resolve_remodex_state_dir() -> PathBuf {
    match std::env::var("REMODEX_DEVICE_STATE_DIR") {
        Ok(path) if !path.trim().is_empty() => PathBuf::from(path),
        _ => dirs::home_dir().unwrap().join(DEFAULT_STATE_DIR_NAME),
    }
}

pub fn resolve_daemon_config_path() -> PathBuf {
    resolve_remodex_state_dir().join(DAEMON_CONFIG_FILE)
}

pub fn resolve_pairing_session_path() -> PathBuf {
    resolve_remodex_state_dir().join(PAIRING_SESSION_FILE)
}

pub fn resolve_bridge_status_path() -> PathBuf {
    resolve_remodex_state_dir().join(BRIDGE_STATUS_FILE)
}

pub fn resolve_bridge_logs_dir() -> PathBuf {
    resolve_remodex_state_dir().join(LOGS_DIR)
}

pub fn resolve_bridge_stdout_log_path() -> PathBuf {
    resolve_bridge_logs_dir().join(BRIDGE_STDOUT_LOG_FILE)
}

pub fn resolve_bridge_stderr_log_path() -> PathBuf {
    resolve_bridge_logs_dir().join(BRIDGE_STDERR_LOG_FILE)
}

pub fn write_daemon_config(config: &crate::config::BridgeConfig) -> Result<()> {
    write_json_file(resolve_daemon_config_path(), config)
}

pub fn write_pairing_session(pairing_payload: Value) -> Result<()> {
    write_json_file(
        resolve_pairing_session_path(),
        &PairingSession {
            created_at: now_iso(),
            pairing_payload,
        },
    )
}

pub fn read_pairing_session() -> Option<PairingSession> {
    read_json_file(resolve_pairing_session_path())
}

pub fn clear_pairing_session() {
    remove_file(resolve_pairing_session_path());
}

pub fn write_bridge_status(
    state: &str,
    connection_status: &str,
    pid: u32,
    last_error: &str,
) -> Result<()> {
    write_json_file(
        resolve_bridge_status_path(),
        &BridgeStatus {
            state: state.to_owned(),
            connection_status: connection_status.to_owned(),
            pid,
            last_error: last_error.to_owned(),
            updated_at: now_iso(),
            runtime: current_bridge_runtime_metadata(),
        },
    )
}

pub fn read_bridge_status() -> Option<BridgeStatus> {
    read_json_file(resolve_bridge_status_path())
}

pub fn clear_bridge_status() {
    remove_file(resolve_bridge_status_path());
}

pub fn ensure_remodex_state_dir() -> Result<()> {
    fs::create_dir_all(resolve_remodex_state_dir())?;
    Ok(())
}

pub fn ensure_remodex_logs_dir() -> Result<()> {
    fs::create_dir_all(resolve_bridge_logs_dir())?;
    Ok(())
}

fn write_json_file<T: Serialize>(path: PathBuf, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_string_pretty(value)?;
    fs::write(&path, serialized)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: PathBuf) -> Option<T> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn remove_file(path: PathBuf) {
    let _ = fs::remove_file(path);
}

fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| String::new())
}

pub fn current_bridge_runtime_metadata() -> BridgeRuntimeMetadata {
    BridgeRuntimeMetadata {
        runtime_kind: "rust".to_owned(),
        runtime_source: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .display()
            .to_string(),
        runtime_executable: std::env::current_exe()
            .ok()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    }
}
