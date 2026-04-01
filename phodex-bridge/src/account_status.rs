use serde_json::{json, Value};

use crate::package_version_status::BridgeVersionInfo;

pub fn compose_sanitized_auth_status(
    account_read: Option<Value>,
    auth_status: Option<Value>,
    login_in_flight: bool,
    bridge_version_info: Option<BridgeVersionInfo>,
) -> color_eyre::eyre::Result<Value> {
    if account_read.is_none() && auth_status.is_none() {
        return Err(color_eyre::eyre::eyre!(
            "Unable to read ChatGPT account status from the bridge."
        ));
    }

    let account = account_read
        .as_ref()
        .and_then(|value| value.get("account"))
        .cloned()
        .unwrap_or(Value::Null);
    let auth_token = auth_status
        .as_ref()
        .and_then(|value| value.get("authToken"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let auth_method = first_non_empty(&[
        auth_status
            .as_ref()
            .and_then(|value| value.get("authMethod"))
            .and_then(Value::as_str),
        account.get("type").and_then(Value::as_str),
    ]);
    let has_account_login = account
        .get("loggedIn")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || account
            .get("logged_in")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || account
            .get("isLoggedIn")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || account.get("email").and_then(Value::as_str).is_some();
    let requires_openai_auth = account_read
        .as_ref()
        .and_then(|value| value.get("requiresOpenaiAuth"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || auth_status
            .as_ref()
            .and_then(|value| value.get("requiresOpenaiAuth"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let has_prior_login_context = has_account_login || auth_method.is_some();
    let needs_reauth = !login_in_flight && requires_openai_auth && has_prior_login_context;
    let token_ready = !auth_token.is_empty();
    let is_authenticated = !needs_reauth && (token_ready || has_account_login);
    let status = if is_authenticated {
        "authenticated"
    } else if login_in_flight {
        "pending_login"
    } else if needs_reauth {
        "expired"
    } else {
        "not_logged_in"
    };

    let version_info = bridge_version_info.unwrap_or_default();

    Ok(json!({
        "status": status,
        "authMethod": auth_method,
        "email": account.get("email").and_then(Value::as_str),
        "planType": account.get("planType").and_then(Value::as_str),
        "loginInFlight": login_in_flight,
        "needsReauth": needs_reauth,
        "tokenReady": token_ready,
        "expiresAt": Value::Null,
        "bridgeVersion": version_info.bridge_version.unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned()),
        "bridgeLatestVersion": version_info.bridge_latest_version,
    }))
}

fn first_non_empty(values: &[Option<&str>]) -> Option<String> {
    values
        .iter()
        .flatten()
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}
