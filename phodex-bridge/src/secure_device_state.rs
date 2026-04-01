use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use color_eyre::eyre::{eyre, Result};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const KEYCHAIN_SERVICE: &str = "com.remodex.bridge.device-state";
const KEYCHAIN_ACCOUNT: &str = "default";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeDeviceState {
    pub version: u32,
    #[serde(rename = "macDeviceId")]
    pub mac_device_id: String,
    #[serde(rename = "macIdentityPublicKey")]
    pub mac_identity_public_key: String,
    #[serde(rename = "macIdentityPrivateKey")]
    pub mac_identity_private_key: String,
    #[serde(rename = "trustedPhones")]
    pub trusted_phones: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct RelaySession {
    pub device_state: BridgeDeviceState,
    pub session_id: String,
}

pub fn load_or_create_bridge_device_state() -> Result<BridgeDeviceState> {
    if let Some(state) = read_canonical_file_state() {
        let _ = write_keychain_state_string(&serde_json::to_string_pretty(&state)?);
        return Ok(state);
    }

    if let Some(state) = read_keychain_state() {
        write_bridge_device_state(&state)?;
        return Ok(state);
    }

    let state = create_bridge_device_state();
    write_bridge_device_state(&state)?;
    Ok(state)
}

pub fn reset_bridge_device_state() -> Result<()> {
    let _ = fs::remove_file(resolve_store_file());
    let _ = delete_keychain_state_string();
    Ok(())
}

pub fn resolve_bridge_relay_session(state: BridgeDeviceState) -> RelaySession {
    RelaySession {
        device_state: state,
        session_id: Uuid::new_v4().to_string(),
    }
}

pub fn remember_trusted_phone(
    state: &BridgeDeviceState,
    phone_device_id: &str,
    phone_identity_public_key: &str,
) -> Result<BridgeDeviceState> {
    let mut next = state.clone();
    next.trusted_phones.clear();
    next.trusted_phones.insert(
        phone_device_id.trim().to_owned(),
        phone_identity_public_key.trim().to_owned(),
    );
    write_bridge_device_state(&next)?;
    Ok(next)
}

pub fn get_trusted_phone_public_key(
    state: &BridgeDeviceState,
    phone_device_id: &str,
) -> Option<String> {
    state.trusted_phones.get(phone_device_id).cloned()
}

fn create_bridge_device_state() -> BridgeDeviceState {
    let mut private_key = [0_u8; 32];
    let _ = getrandom::fill(&mut private_key);
    let signing_key = SigningKey::from_bytes(&private_key);
    BridgeDeviceState {
        version: 1,
        mac_device_id: Uuid::new_v4().to_string(),
        mac_identity_public_key: BASE64.encode(signing_key.verifying_key().to_bytes()),
        mac_identity_private_key: BASE64.encode(signing_key.to_bytes()),
        trusted_phones: BTreeMap::new(),
    }
}

fn write_bridge_device_state(state: &BridgeDeviceState) -> Result<()> {
    let serialized = serde_json::to_string_pretty(state)?;
    fs::create_dir_all(resolve_store_dir())?;
    fs::write(resolve_store_file(), &serialized)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(resolve_store_file(), fs::Permissions::from_mode(0o600))?;
    }
    let _ = write_keychain_state_string(&serialized);
    Ok(())
}

fn read_canonical_file_state() -> Option<BridgeDeviceState> {
    let raw = fs::read_to_string(resolve_store_file()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn read_keychain_state() -> Option<BridgeDeviceState> {
    let raw = read_keychain_state_string()?;
    serde_json::from_str(&raw).ok()
}

fn resolve_store_dir() -> PathBuf {
    std::env::var("REMODEX_DEVICE_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::daemon_state::resolve_remodex_state_dir())
}

fn resolve_store_file() -> PathBuf {
    std::env::var("REMODEX_DEVICE_STATE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| resolve_store_dir().join("device-state.json"))
}

fn resolve_keychain_mock_file() -> Option<PathBuf> {
    std::env::var("REMODEX_DEVICE_STATE_KEYCHAIN_MOCK_FILE")
        .ok()
        .map(PathBuf::from)
}

fn read_keychain_state_string() -> Option<String> {
    if let Some(path) = resolve_keychain_mock_file() {
        return fs::read_to_string(path).ok();
    }

    if !cfg!(target_os = "macos") {
        return None;
    }

    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn write_keychain_state_string(value: &str) -> Result<()> {
    if let Some(path) = resolve_keychain_mock_file() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, value)?;
        return Ok(());
    }

    if !cfg!(target_os = "macos") {
        return Ok(());
    }

    let status = Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
            value,
        ])
        .status()?;
    if !status.success() {
        return Err(eyre!("Failed to update the bridge Keychain mirror"));
    }
    Ok(())
}

fn delete_keychain_state_string() -> Result<()> {
    if let Some(path) = resolve_keychain_mock_file() {
        let _ = fs::remove_file(path);
        return Ok(());
    }

    if !cfg!(target_os = "macos") {
        return Ok(());
    }

    let _ = Command::new("security")
        .args([
            "delete-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
        ])
        .status()?;
    Ok(())
}
