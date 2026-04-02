use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use color_eyre::eyre::{eyre, Result};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const KEYCHAIN_SERVICE: &str = "com.remodex.bridge.device-state";
const KEYCHAIN_ACCOUNT: &str = "default";
static WARNED_KEYCHAIN_UNREADABLE: AtomicBool = AtomicBool::new(false);
static WARNED_KEYCHAIN_MISMATCH: AtomicBool = AtomicBool::new(false);
static WARNED_KEYCHAIN_RECOVERY: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    let file_record = read_canonical_file_state_record();
    let keychain_record = read_keychain_state_record();

    match &file_record {
        StateRecord::State(state) => {
            reconcile_keychain_mirror(state, &keychain_record)?;
            return Ok(state.clone());
        }
        StateRecord::Corrupted(error) => {
            if let StateRecord::State(state) = &keychain_record {
                warn_once(
                    &WARNED_KEYCHAIN_RECOVERY,
                    "[remodex] Recovering the canonical device-state.json from the legacy Keychain pairing mirror.",
                );
                write_bridge_device_state(state)?;
                return Ok(state.clone());
            }

            return Err(eyre!(
                "The canonical bridge pairing state at {} is unreadable: {error}",
                resolve_store_file().display()
            ));
        }
        StateRecord::Missing => {}
    }

    match keychain_record {
        StateRecord::State(state) => {
            write_bridge_device_state(&state)?;
            return Ok(state);
        }
        StateRecord::Corrupted(error) => {
            return Err(eyre!(
                "The legacy Keychain bridge pairing state is unreadable and no canonical device-state.json is available: {error}"
            ));
        }
        StateRecord::Missing => {}
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
    let normalized = normalize_bridge_device_state(state.clone())?;
    let serialized = serde_json::to_string_pretty(&normalized)?;
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

fn read_canonical_file_state_record() -> StateRecord {
    let path = resolve_store_file();
    read_state_record(&path)
}

fn read_keychain_state_record() -> StateRecord {
    let Some(raw) = read_keychain_state_string() else {
        return StateRecord::Missing;
    };

    parse_bridge_device_state(&raw).into()
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

#[derive(Debug)]
enum StateRecord {
    Missing,
    State(BridgeDeviceState),
    Corrupted(String),
}

impl From<Result<BridgeDeviceState>> for StateRecord {
    fn from(value: Result<BridgeDeviceState>) -> Self {
        match value {
            Ok(state) => Self::State(state),
            Err(error) => Self::Corrupted(error.to_string()),
        }
    }
}

fn read_state_record(path: &Path) -> StateRecord {
    if !path.exists() {
        return StateRecord::Missing;
    }

    match fs::read_to_string(path) {
        Ok(raw) => parse_bridge_device_state(&raw).into(),
        Err(error) => StateRecord::Corrupted(error.to_string()),
    }
}

fn parse_bridge_device_state(raw: &str) -> Result<BridgeDeviceState> {
    let parsed = serde_json::from_str::<BridgeDeviceState>(raw)?;
    normalize_bridge_device_state(parsed)
}

fn normalize_bridge_device_state(state: BridgeDeviceState) -> Result<BridgeDeviceState> {
    let mac_device_id = normalize_non_empty_string(&state.mac_device_id)
        .ok_or_else(|| eyre!("Bridge device state is missing macDeviceId"))?;
    let mac_identity_public_key = normalize_non_empty_string(&state.mac_identity_public_key)
        .ok_or_else(|| eyre!("Bridge device state is missing macIdentityPublicKey"))?;
    let mac_identity_private_key = normalize_non_empty_string(&state.mac_identity_private_key)
        .ok_or_else(|| eyre!("Bridge device state is missing macIdentityPrivateKey"))?;

    let trusted_phones = state
        .trusted_phones
        .into_iter()
        .filter_map(|(device_id, public_key)| {
            Some((
                normalize_non_empty_string(&device_id)?,
                normalize_non_empty_string(&public_key)?,
            ))
        })
        .collect();

    Ok(BridgeDeviceState {
        version: 1,
        mac_device_id,
        mac_identity_public_key,
        mac_identity_private_key,
        trusted_phones,
    })
}

fn reconcile_keychain_mirror(
    canonical_state: &BridgeDeviceState,
    keychain_record: &StateRecord,
) -> Result<()> {
    match keychain_record {
        StateRecord::Missing => {
            let _ = write_keychain_state_string(&serde_json::to_string_pretty(canonical_state)?);
        }
        StateRecord::Corrupted(_) => {
            warn_once(
                &WARNED_KEYCHAIN_UNREADABLE,
                "[remodex] Ignoring unreadable legacy Keychain pairing mirror; using canonical device-state.json.",
            );
            let _ = write_keychain_state_string(&serde_json::to_string_pretty(canonical_state)?);
        }
        StateRecord::State(keychain_state) if keychain_state != canonical_state => {
            warn_once(
                &WARNED_KEYCHAIN_MISMATCH,
                "[remodex] Canonical bridge pairing state differs from the legacy Keychain mirror; using device-state.json.",
            );
            let _ = write_keychain_state_string(&serde_json::to_string_pretty(canonical_state)?);
        }
        StateRecord::State(_) => {}
    }

    Ok(())
}

fn warn_once(flag: &AtomicBool, message: &str) {
    if flag
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        eprintln!("{message}");
    }
}

fn normalize_non_empty_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn canonical_state_wins_and_repairs_a_corrupt_keychain_mirror() {
        with_device_state_env(|state_file, keychain_file| {
            let state = sample_state(
                "mac-1",
                "mac-public-1",
                "mac-private-1",
                "phone-1",
                "phone-public-1",
            );
            fs::write(&state_file, serde_json::to_string_pretty(&state).unwrap()).unwrap();
            fs::write(&keychain_file, "{not-json").unwrap();

            let loaded = load_or_create_bridge_device_state().unwrap();

            assert_eq!(loaded, state);
            let repaired = serde_json::from_str::<BridgeDeviceState>(
                &fs::read_to_string(keychain_file).unwrap(),
            )
            .unwrap();
            assert_eq!(repaired, state);
        });
    }

    #[test]
    fn missing_canonical_state_recovers_from_keychain_mirror() {
        with_device_state_env(|state_file, keychain_file| {
            let state = sample_state(
                "mac-2",
                "mac-public-2",
                "mac-private-2",
                "phone-2",
                "phone-public-2",
            );
            fs::write(
                &keychain_file,
                serde_json::to_string_pretty(&state).unwrap(),
            )
            .unwrap();

            let loaded = load_or_create_bridge_device_state().unwrap();

            assert_eq!(loaded, state);
            let restored =
                serde_json::from_str::<BridgeDeviceState>(&fs::read_to_string(state_file).unwrap())
                    .unwrap();
            assert_eq!(restored, state);
        });
    }

    #[test]
    fn canonical_state_rewrites_a_stale_keychain_mirror() {
        with_device_state_env(|state_file, keychain_file| {
            let canonical = sample_state(
                "mac-3",
                "mac-public-3",
                "mac-private-3",
                "phone-3",
                "phone-public-3",
            );
            let stale = sample_state(
                "mac-stale",
                "mac-public-stale",
                "mac-private-stale",
                "phone-stale",
                "phone-public-stale",
            );
            fs::write(
                &state_file,
                serde_json::to_string_pretty(&canonical).unwrap(),
            )
            .unwrap();
            fs::write(
                &keychain_file,
                serde_json::to_string_pretty(&stale).unwrap(),
            )
            .unwrap();

            let loaded = load_or_create_bridge_device_state().unwrap();

            assert_eq!(loaded, canonical);
            let repaired = serde_json::from_str::<BridgeDeviceState>(
                &fs::read_to_string(keychain_file).unwrap(),
            )
            .unwrap();
            assert_eq!(repaired, canonical);
        });
    }

    #[test]
    fn corrupt_canonical_state_does_not_silently_rotate_identity_without_recovery() {
        with_device_state_env(|state_file, _| {
            fs::write(&state_file, "{not-json").unwrap();

            let error = load_or_create_bridge_device_state().unwrap_err();

            assert!(error.to_string().contains("canonical bridge pairing state"));
        });
    }

    fn with_device_state_env(test: impl FnOnce(PathBuf, PathBuf)) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tempdir = tempdir().unwrap();
        let state_file = tempdir.path().join("device-state.json");
        let keychain_file = tempdir.path().join("keychain-state.json");
        let env_guard = ScopedEnv::new([
            (
                "REMODEX_DEVICE_STATE_FILE",
                Some(state_file.as_os_str().to_os_string()),
            ),
            (
                "REMODEX_DEVICE_STATE_DIR",
                Some(tempdir.path().as_os_str().to_os_string()),
            ),
            (
                "REMODEX_DEVICE_STATE_KEYCHAIN_MOCK_FILE",
                Some(keychain_file.as_os_str().to_os_string()),
            ),
        ]);

        test(state_file, keychain_file);

        drop(env_guard);
    }

    fn sample_state(
        mac_device_id: &str,
        mac_identity_public_key: &str,
        mac_identity_private_key: &str,
        phone_device_id: &str,
        phone_public_key: &str,
    ) -> BridgeDeviceState {
        BridgeDeviceState {
            version: 1,
            mac_device_id: mac_device_id.to_owned(),
            mac_identity_public_key: mac_identity_public_key.to_owned(),
            mac_identity_private_key: mac_identity_private_key.to_owned(),
            trusted_phones: BTreeMap::from([(
                phone_device_id.to_owned(),
                phone_public_key.to_owned(),
            )]),
        }
    }

    struct ScopedEnv {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl ScopedEnv {
        fn new<const N: usize>(values: [(&'static str, Option<OsString>); N]) -> Self {
            let saved = values
                .iter()
                .map(|(key, _)| (*key, std::env::var_os(key)))
                .collect::<Vec<_>>();

            for (key, value) in values {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }

            Self { saved }
        }
    }

    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}
