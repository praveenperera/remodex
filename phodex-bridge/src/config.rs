use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeConfig {
    pub relay_url: String,
    pub push_service_url: String,
    pub push_preview_max_chars: usize,
    pub refresh_enabled: bool,
    pub refresh_debounce_ms: u64,
    pub codex_endpoint: String,
    pub refresh_command: String,
    pub codex_bundle_id: String,
    pub codex_app_path: String,
}

#[derive(Default, Debug)]
struct PrivateDefaults {
    relay_url: String,
    push_service_url: String,
}

const DEFAULT_BUNDLE_ID: &str = "com.openai.codex";
const DEFAULT_APP_PATH: &str = "/Applications/Codex.app";
const DEFAULT_DEBOUNCE_MS: u64 = 700;

pub fn read_bridge_config() -> BridgeConfig {
    let runtime_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let private_defaults = read_private_package_defaults(&runtime_root);
    let default_relay_url = private_defaults.relay_url.clone();
    let explicit_relay_url = read_first_defined_env(&["REMODEX_RELAY"], "");
    let relay_url = read_first_defined_env(&["REMODEX_RELAY"], &default_relay_url);
    let default_push_service_url = if explicit_relay_url.is_empty() {
        private_defaults.push_service_url.clone()
    } else {
        String::new()
    };
    let explicit_refresh_enabled = read_optional_boolean_env(&["REMODEX_REFRESH_ENABLED"]);

    BridgeConfig {
        relay_url,
        push_service_url: read_first_defined_env(
            &["REMODEX_PUSH_SERVICE_URL"],
            &default_push_service_url,
        ),
        push_preview_max_chars: read_first_defined_env(&["REMODEX_PUSH_PREVIEW_MAX_CHARS"], "160")
            .parse()
            .ok()
            .filter(|value: &usize| *value > 0)
            .unwrap_or(160),
        refresh_enabled: explicit_refresh_enabled.unwrap_or(false),
        refresh_debounce_ms: read_first_defined_env(
            &["REMODEX_REFRESH_DEBOUNCE_MS"],
            &DEFAULT_DEBOUNCE_MS.to_string(),
        )
        .parse()
        .ok()
        .unwrap_or(DEFAULT_DEBOUNCE_MS),
        codex_endpoint: read_first_defined_env(&["REMODEX_CODEX_ENDPOINT"], ""),
        refresh_command: read_first_defined_env(&["REMODEX_REFRESH_COMMAND"], ""),
        codex_bundle_id: read_first_defined_env(&["REMODEX_CODEX_BUNDLE_ID"], DEFAULT_BUNDLE_ID),
        codex_app_path: DEFAULT_APP_PATH.to_owned(),
    }
}

fn read_private_package_defaults(runtime_root: &Path) -> PrivateDefaults {
    let defaults_path = runtime_root.join("src").join("private-defaults.json");
    let raw = match fs::read_to_string(defaults_path) {
        Ok(raw) => raw,
        Err(_) => return PrivateDefaults::default(),
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(_) => return PrivateDefaults::default(),
    };

    PrivateDefaults {
        relay_url: read_string(parsed.get("relayUrl")),
        push_service_url: read_string(parsed.get("pushServiceUrl")),
    }
}

fn read_optional_boolean_env(keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| std::env::var(key).ok())
        .map(|value| parse_boolean_env(&value))
}

fn read_first_defined_env(keys: &[&str], fallback: &str) -> String {
    keys.iter()
        .find_map(|key| std::env::var(key).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn parse_boolean_env(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "false" | "0" | "no"
    )
}

fn read_string(value: Option<&serde_json::Value>) -> String {
    value
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_default()
}
