use color_eyre::eyre::{eyre, Result};
use serde_json::{json, Value};

use crate::secure_device_state::BridgeDeviceState;

use super::{RelayCommand, ThreadRuntimeContext};

pub(super) fn send_relay_registration_update(
    command_tx: &tokio::sync::mpsc::UnboundedSender<RelayCommand>,
    device_state: &BridgeDeviceState,
) {
    let _ = command_tx.send(RelayCommand::Text(
        json!({
            "kind": "relayMacRegistration",
            "registration": build_mac_registration(device_state),
        })
        .to_string(),
    ));
}

pub(super) fn build_mac_registration_headers(
    device_state: &BridgeDeviceState,
) -> Vec<(&'static str, String)> {
    let registration = build_mac_registration(device_state);
    let mut headers = vec![
        (
            "x-mac-device-id",
            read_string(registration.get("macDeviceId")).unwrap_or_default(),
        ),
        (
            "x-mac-identity-public-key",
            read_string(registration.get("macIdentityPublicKey")).unwrap_or_default(),
        ),
        (
            "x-machine-name",
            read_string(registration.get("displayName")).unwrap_or_default(),
        ),
    ];
    if let Some(trusted_phone_device_id) = read_string(registration.get("trustedPhoneDeviceId")) {
        headers.push(("x-trusted-phone-device-id", trusted_phone_device_id));
    }
    if let Some(trusted_phone_public_key) = read_string(registration.get("trustedPhonePublicKey")) {
        headers.push(("x-trusted-phone-public-key", trusted_phone_public_key));
    }
    headers
}

fn build_mac_registration(device_state: &BridgeDeviceState) -> Value {
    let trusted_phone_entry = device_state
        .trusted_phones
        .iter()
        .next()
        .map(|(device_id, public_key)| (device_id.clone(), public_key.clone()));
    json!({
        "macDeviceId": device_state.mac_device_id,
        "macIdentityPublicKey": device_state.mac_identity_public_key,
        "displayName": hostname::get()
            .ok()
            .and_then(|value| value.into_string().ok())
            .unwrap_or_default(),
        "trustedPhoneDeviceId": trusted_phone_entry.as_ref().map(|entry| entry.0.clone()),
        "trustedPhonePublicKey": trusted_phone_entry.as_ref().map(|entry| entry.1.clone()),
    })
}

pub(super) fn relay_ws_url(base_url: &str, session_id: &str) -> Result<String> {
    let mut url = url::Url::parse(base_url)?;
    match url.scheme() {
        "http" => {
            let _ = url.set_scheme("ws");
        }
        "https" => {
            let _ = url.set_scheme("wss");
        }
        "ws" | "wss" => {}
        other => return Err(eyre!("Unsupported relay URL scheme: {other}")),
    }
    let next_path = format!("{}/{}", url.path().trim_end_matches('/'), session_id);
    url.set_path(&next_path);
    Ok(url.to_string())
}

pub(super) fn random_hex(len: usize) -> String {
    let mut bytes = vec![0_u8; len];
    let _ = getrandom::fill(&mut bytes);
    bytes
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn sanitize_thread_history_images_for_relay(
    raw_message: String,
    request_method: &str,
) -> String {
    if request_method != "thread/read" && request_method != "thread/resume" {
        return raw_message;
    }

    let Ok(mut parsed) = serde_json::from_str::<Value>(&raw_message) else {
        return raw_message;
    };
    let Some(turns) = parsed
        .get_mut("result")
        .and_then(|value| value.get_mut("thread"))
        .and_then(|value| value.get_mut("turns"))
        .and_then(Value::as_array_mut)
    else {
        return raw_message;
    };

    let mut did_change = false;
    for turn in turns {
        let Some(items) = turn.get_mut("items").and_then(Value::as_array_mut) else {
            continue;
        };
        for item in items {
            let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for content_item in content {
                if sanitize_inline_history_image_content_item(content_item) {
                    did_change = true;
                }
            }
        }
    }

    if did_change {
        parsed.to_string()
    } else {
        raw_message
    }
}

fn sanitize_inline_history_image_content_item(content_item: &mut Value) -> bool {
    let normalized_type = read_string(content_item.get("type"))
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace([' ', '_', '-'], "");
    if normalized_type != "image" && normalized_type != "localimage" {
        return false;
    }

    let has_inline_url = is_inline_history_image_data_url(content_item.get("url"))
        || is_inline_history_image_data_url(content_item.get("image_url"))
        || is_inline_history_image_data_url(content_item.get("path"));
    if !has_inline_url {
        return false;
    }

    if let Some(object) = content_item.as_object_mut() {
        object.remove("path");
        object.remove("image_url");
        object.insert(
            "url".to_owned(),
            Value::String(super::RELAY_HISTORY_IMAGE_REFERENCE_URL.to_owned()),
        );
        return true;
    }

    false
}

fn is_inline_history_image_data_url(value: Option<&Value>) -> bool {
    read_string(value)
        .map(|value| value.to_ascii_lowercase().starts_with("data:image"))
        .unwrap_or(false)
}

pub(super) struct MessageContext {
    pub(super) method: String,
    pub(super) thread_id: String,
    pub(super) turn_id: Option<String>,
}

pub(super) fn extract_bridge_message_context(raw_message: &str) -> MessageContext {
    let Ok(parsed) = serde_json::from_str::<Value>(raw_message) else {
        return MessageContext {
            method: String::new(),
            thread_id: String::new(),
            turn_id: None,
        };
    };
    let method = read_string(parsed.get("method")).unwrap_or_default();
    let params = parsed.get("params").cloned().unwrap_or_else(|| json!({}));
    let thread_id = extract_thread_id(&method, &params).unwrap_or_default();
    let turn_id = extract_turn_id(&method, &params);

    MessageContext {
        method,
        thread_id,
        turn_id,
    }
}

fn extract_thread_id(method: &str, params: &Value) -> Option<String> {
    match method {
        "turn/start" | "turn/started" | "turn/completed" => read_string(params.get("threadId"))
            .or_else(|| read_string(params.get("thread_id")))
            .or_else(|| read_string(params.get("turn").and_then(|value| value.get("threadId"))))
            .or_else(|| read_string(params.get("turn").and_then(|value| value.get("thread_id")))),
        "thread/start" | "thread/started" => read_string(params.get("threadId"))
            .or_else(|| read_string(params.get("thread_id")))
            .or_else(|| read_string(params.get("thread").and_then(|value| value.get("id"))))
            .or_else(|| read_string(params.get("thread").and_then(|value| value.get("threadId"))))
            .or_else(|| {
                read_string(
                    params
                        .get("thread")
                        .and_then(|value| value.get("thread_id")),
                )
            }),
        _ => None,
    }
}

fn extract_turn_id(method: &str, params: &Value) -> Option<String> {
    match method {
        "turn/started" | "turn/completed" => read_string(params.get("turnId"))
            .or_else(|| read_string(params.get("turn_id")))
            .or_else(|| read_string(params.get("id")))
            .or_else(|| read_string(params.get("turn").and_then(|value| value.get("id"))))
            .or_else(|| read_string(params.get("turn").and_then(|value| value.get("turnId"))))
            .or_else(|| read_string(params.get("turn").and_then(|value| value.get("turn_id")))),
        _ => None,
    }
}

pub(super) fn read_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn stringify_request_id(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if !value.trim().is_empty() => Some(value.trim().to_owned()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

pub(super) fn thread_runtime_context_from_value(value: &Value) -> ThreadRuntimeContext {
    let metadata = value.get("metadata");
    ThreadRuntimeContext {
        model: read_string(value.get("model"))
            .or_else(|| read_string(metadata.and_then(|value| value.get("model"))))
            .or_else(|| read_string(metadata.and_then(|value| value.get("modelName"))))
            .or_else(|| read_string(metadata.and_then(|value| value.get("model_name")))),
        model_provider: read_string(value.get("modelProvider"))
            .or_else(|| read_string(value.get("model_provider")))
            .or_else(|| read_string(metadata.and_then(|value| value.get("modelProvider"))))
            .or_else(|| read_string(metadata.and_then(|value| value.get("model_provider"))))
            .or_else(|| read_string(metadata.and_then(|value| value.get("modelProviderId"))))
            .or_else(|| read_string(metadata.and_then(|value| value.get("model_provider_id")))),
    }
}

pub(super) fn rewrite_existing_thread_runtime_request(
    parsed: &mut Value,
    context: Option<&ThreadRuntimeContext>,
) -> bool {
    let method = read_string(parsed.get("method")).unwrap_or_default();
    if method != "thread/resume" && method != "turn/start" {
        return false;
    }

    let Some(params) = parsed.get_mut("params").and_then(Value::as_object_mut) else {
        return false;
    };
    let has_thread_id = read_string_value(params.get("threadId"))
        .or_else(|| read_string_value(params.get("thread_id")))
        .is_some();
    if !has_thread_id {
        return false;
    }

    match context {
        Some(context) => {
            rewrite_runtime_identity_field(params, "model", context.model.as_ref());
            rewrite_runtime_identity_field(
                params,
                "modelProvider",
                context.model_provider.as_ref(),
            );
            rewrite_runtime_identity_field(
                params,
                "model_provider",
                context.model_provider.as_ref(),
            );
            if method == "turn/start" {
                if let Some(settings) = params
                    .get_mut("collaborationMode")
                    .and_then(Value::as_object_mut)
                    .and_then(|value| value.get_mut("settings"))
                    .and_then(Value::as_object_mut)
                {
                    rewrite_runtime_identity_field(settings, "model", context.model.as_ref());
                }
            }
        }
        None => {
            params.remove("model");
            params.remove("modelProvider");
            params.remove("model_provider");
            if method == "turn/start" {
                if let Some(settings) = params
                    .get_mut("collaborationMode")
                    .and_then(Value::as_object_mut)
                    .and_then(|value| value.get_mut("settings"))
                    .and_then(Value::as_object_mut)
                {
                    settings.remove("model");
                }
            }
        }
    }

    true
}

fn rewrite_runtime_identity_field(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<&String>,
) {
    if let Some(value) = value {
        object.insert(key.to_owned(), Value::String(value.clone()));
    } else {
        object.remove(key);
    }
}

fn read_string_value(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn error_code_for_method(method: &str) -> &str {
    match method {
        "account/status/read" | "getAuthStatus" | "account/login/openOnMac" => "auth_status_failed",
        "voice/resolveAuth" | "voice/transcribe" => "voice_error",
        "thread/contextWindow/read" => "thread_context_error",
        "desktop/continueOnMac" => "desktop_error",
        "notifications/push/register" => "push_registration_failed",
        _ if method.starts_with("workspace/") => "workspace_error",
        _ if method.starts_with("git/") => "git_error",
        _ => "bridge_error",
    }
}

#[cfg(unix)]
pub(super) async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut terminate = signal(SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            if let Some(signal) = terminate.as_mut() {
                let _ = signal.recv().await;
            }
        } => {}
    }
}

#[cfg(not(unix))]
pub(super) async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        relay_ws_url, rewrite_existing_thread_runtime_request,
        sanitize_thread_history_images_for_relay, thread_runtime_context_from_value,
    };
    use crate::bridge::ThreadRuntimeContext;

    #[test]
    fn relay_ws_url_converts_http_scheme_and_appends_session() {
        let resolved = relay_ws_url("http://127.0.0.1:9000/relay", "session-123").unwrap();
        assert_eq!(resolved, "ws://127.0.0.1:9000/relay/session-123");
    }

    #[test]
    fn sanitize_thread_history_images_for_relay_elides_inline_data_urls() {
        let raw_message = json!({
            "id": "req-thread-read",
            "result": {
                "thread": {
                    "id": "thread-images",
                    "turns": [
                        {
                            "id": "turn-1",
                            "items": [
                                {
                                    "id": "item-user",
                                    "type": "user_message",
                                    "content": [
                                        {
                                            "type": "input_text",
                                            "text": "Look at this screenshot",
                                        },
                                        {
                                            "type": "image",
                                            "image_url": "data:image/png;base64,AAAA",
                                        }
                                    ],
                                }
                            ],
                        }
                    ],
                }
            }
        })
        .to_string();

        let sanitized = sanitize_thread_history_images_for_relay(raw_message, "thread/read");
        let sanitized: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        let content = &sanitized["result"]["thread"]["turns"][0]["items"][0]["content"];

        assert_eq!(
            content[0],
            json!({
                "type": "input_text",
                "text": "Look at this screenshot",
            })
        );
        assert_eq!(
            content[1],
            json!({
                "type": "image",
                "url": "remodex://history-image-elided",
            })
        );
    }

    #[test]
    fn sanitize_thread_history_images_for_relay_leaves_unrelated_payloads_unchanged() {
        let raw_message = json!({
            "id": "req-other",
            "result": {
                "ok": true,
            },
        })
        .to_string();

        assert_eq!(
            sanitize_thread_history_images_for_relay(raw_message.clone(), "turn/start"),
            raw_message
        );
    }

    #[test]
    fn rewrite_existing_thread_runtime_request_uses_cached_model_and_provider() {
        let mut raw_message = json!({
            "id": "req-resume",
            "method": "thread/resume",
            "params": {
                "threadId": "thread-1",
                "model": "gpt-5.4",
            },
        });

        let context = ThreadRuntimeContext {
            model: Some("gpt-5.4-mini".to_owned()),
            model_provider: Some("openai".to_owned()),
        };
        assert!(rewrite_existing_thread_runtime_request(
            &mut raw_message,
            Some(&context)
        ));
        assert_eq!(raw_message["params"]["model"], json!("gpt-5.4-mini"));
        assert_eq!(raw_message["params"]["modelProvider"], json!("openai"));
    }

    #[test]
    fn rewrite_existing_thread_runtime_request_strips_unknown_existing_thread_model() {
        let mut raw_message = json!({
            "id": "req-turn",
            "method": "turn/start",
            "params": {
                "threadId": "thread-1",
                "model": "gpt-5.4",
                "collaborationMode": {
                    "settings": {
                        "model": "gpt-5.4"
                    }
                }
            },
        });

        assert!(rewrite_existing_thread_runtime_request(
            &mut raw_message,
            None
        ));
        assert!(raw_message["params"].get("model").is_none());
        assert!(raw_message["params"]["collaborationMode"]["settings"]
            .get("model")
            .is_none());
    }

    #[test]
    fn thread_runtime_context_from_value_reads_metadata_fallbacks() {
        let context = thread_runtime_context_from_value(&json!({
            "id": "thread-1",
            "metadata": {
                "model_name": "gpt-5.4-mini",
                "model_provider": "openai",
            }
        }));

        assert_eq!(
            context,
            ThreadRuntimeContext {
                model: Some("gpt-5.4-mini".to_owned()),
                model_provider: Some("openai".to_owned()),
            }
        );
    }

    #[test]
    fn rewrite_existing_thread_runtime_request_leaves_thread_start_untouched() {
        let mut raw_message = json!({
            "id": "req-start",
            "method": "thread/start",
            "params": {
                "model": "gpt-5.4",
            },
        });

        let original = raw_message.clone();
        assert!(!rewrite_existing_thread_runtime_request(
            &mut raw_message,
            None
        ));
        assert_eq!(raw_message, original);
    }
}
