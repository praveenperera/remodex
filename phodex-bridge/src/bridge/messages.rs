use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::session_state::remember_active_thread;

use super::support::{
    extract_bridge_message_context, sanitize_thread_history_images_for_relay, stringify_request_id,
};
use super::{BridgeRuntime, BridgeTask, PendingAuthLogin, TrackedRequest};

impl BridgeRuntime {
    pub(super) fn handle_application_message(&mut self, raw_message: String) {
        if self.handle_bridge_managed_handshake_message(&raw_message) {
            return;
        }

        let parsed = match serde_json::from_str::<Value>(&raw_message) {
            Ok(parsed) => parsed,
            Err(_) => return,
        };
        let method = super::support::read_string(parsed.get("method")).unwrap_or_default();
        if method.is_empty() {
            return;
        }

        let request_id = parsed.get("id").cloned();
        let params = parsed.get("params").cloned().unwrap_or_else(|| json!({}));

        match method.as_str() {
            "account/status/read" | "getAuthStatus" | "account/login/openOnMac" => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::Account);
            }
            "voice/resolveAuth" | "voice/transcribe" => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::Voice);
            }
            "thread/contextWindow/read" => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::ThreadContext);
            }
            "desktop/continueOnMac" => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::Desktop);
            }
            "notifications/push/register" => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::Notifications);
            }
            other if other.starts_with("workspace/") => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::Workspace);
            }
            other if other.starts_with("git/") => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::Git);
            }
            _ => {
                self.prune_tracked_requests();
                self.thread_runtime_registry.prune_stale();
                self.thread_runtime_registry
                    .remember_pending_thread_start_context(&parsed);
                let outbound_message = self
                    .thread_runtime_registry
                    .prepare_outbound_application_message(parsed, raw_message);
                if let Some(rollout_live_mirror) = self.rollout_live_mirror.as_mut() {
                    rollout_live_mirror.observe_inbound(&outbound_message);
                }
                if let Ok(outbound_parsed) = serde_json::from_str::<Value>(&outbound_message) {
                    self.remember_forwarded_request_method(&outbound_parsed);
                }
                self.remember_thread_from_message("phone", &outbound_message);
                self.codex.send(outbound_message);
            }
        }
    }

    pub(super) fn handle_codex_message(&mut self, raw_message: String) {
        if self.handle_bridge_managed_codex_response(&raw_message) {
            return;
        }

        self.update_pending_auth_login_from_codex_message(&raw_message);
        self.track_codex_handshake_state(&raw_message);
        self.thread_runtime_registry
            .update_from_codex_message(&raw_message);
        self.remember_thread_from_message("codex", &raw_message);
        let sanitized = self.sanitize_relay_bound_codex_message(raw_message);
        self.secure_transport
            .queue_outbound_application_message(sanitized);
    }

    pub(super) fn handle_bridge_managed_handshake_message(&mut self, raw_message: &str) -> bool {
        let Ok(parsed) = serde_json::from_str::<Value>(raw_message) else {
            return false;
        };
        let method = super::support::read_string(parsed.get("method")).unwrap_or_default();
        if method.is_empty() {
            return false;
        }

        if method == "initialize" && parsed.get("id").is_some() {
            let id = parsed.get("id").cloned().unwrap_or(Value::Null);
            if !self.codex_handshake_warm {
                if let Some(request_id) = stringify_request_id(&id) {
                    self.forwarded_initialize_request_ids.insert(request_id);
                }
                return false;
            }

            self.secure_transport.queue_outbound_application_message(
                crate::json_rpc::success_response(id, json!({ "bridgeManaged": true })),
            );
            return true;
        }

        method == "initialized" && self.codex_handshake_warm
    }

    pub(super) fn track_codex_handshake_state(&mut self, raw_message: &str) {
        let Ok(parsed) = serde_json::from_str::<Value>(raw_message) else {
            return;
        };
        let Some(request_id) = parsed.get("id").and_then(stringify_request_id) else {
            return;
        };
        if !self.forwarded_initialize_request_ids.remove(&request_id) {
            return;
        }

        if parsed.get("result").is_some() {
            self.codex_handshake_warm = true;
            return;
        }

        let error_message =
            super::support::read_string(parsed.get("error").and_then(|value| value.get("message")))
                .unwrap_or_default();
        if error_message
            .to_ascii_lowercase()
            .contains("already initialized")
        {
            self.codex_handshake_warm = true;
        }
    }

    pub(super) fn handle_bridge_managed_codex_response(&self, raw_message: &str) -> bool {
        let Ok(parsed) = serde_json::from_str::<Value>(raw_message) else {
            return false;
        };
        let Some(request_id) = parsed.get("id").and_then(stringify_request_id) else {
            return false;
        };

        let waiter = self
            .codex_request_client
            .waiters
            .lock()
            .ok()
            .and_then(|mut waiters| waiters.remove(&request_id));
        let Some(waiter) = waiter else {
            return false;
        };

        if let Some(error) = parsed.get("error") {
            let message = super::support::read_string(error.get("message")).unwrap_or_default();
            let _ = waiter.send(Err(if message.is_empty() {
                "Codex request failed".to_owned()
            } else {
                message
            }));
            return true;
        }

        let _ = waiter.send(Ok(parsed.get("result").cloned().unwrap_or(Value::Null)));
        true
    }

    pub(super) fn update_pending_auth_login_from_codex_message(&mut self, raw_message: &str) {
        self.prune_tracked_requests();
        let Ok(parsed) = serde_json::from_str::<Value>(raw_message) else {
            return;
        };

        if let Some(response_id) = parsed.get("id").and_then(stringify_request_id) {
            if let Some(tracked) = self.forwarded_request_methods_by_id.remove(&response_id) {
                match tracked.method.as_str() {
                    "account/login/start" => {
                        let login_id = super::support::read_string(
                            parsed.get("result").and_then(|value| value.get("loginId")),
                        );
                        let auth_url = super::support::read_string(
                            parsed.get("result").and_then(|value| value.get("authUrl")),
                        );
                        if login_id.is_none() || auth_url.is_none() {
                            self.clear_pending_auth_login();
                            return;
                        }
                        if let Ok(mut pending) = self.pending_auth_login.lock() {
                            pending.login_id = login_id;
                            pending.auth_url = auth_url;
                            pending.request_id = Some(response_id);
                            pending.started_at = Some(Instant::now());
                        }
                        return;
                    }
                    "account/login/cancel" | "account/logout" => {
                        self.clear_pending_auth_login();
                        return;
                    }
                    _ => {}
                }
            }
        }

        let method = super::support::read_string(parsed.get("method")).unwrap_or_default();
        if method == "account/login/completed" || method == "account/updated" {
            self.clear_pending_auth_login();
        }
    }

    pub(super) fn clear_pending_auth_login(&self) {
        if let Ok(mut pending) = self.pending_auth_login.lock() {
            *pending = PendingAuthLogin::default();
        }
    }

    pub(super) fn remember_forwarded_request_method(&mut self, parsed: &Value) {
        self.prune_tracked_requests();
        let method = super::support::read_string(parsed.get("method")).unwrap_or_default();
        let Some(request_id) = parsed.get("id").and_then(stringify_request_id) else {
            return;
        };

        if matches!(
            method.as_str(),
            "account/login/start" | "account/login/cancel" | "account/logout"
        ) {
            self.forwarded_request_methods_by_id.insert(
                request_id.clone(),
                TrackedRequest {
                    method: method.clone(),
                    created_at: Instant::now(),
                },
            );
        }

        if matches!(method.as_str(), "thread/read" | "thread/resume") {
            self.relay_sanitized_response_methods_by_id.insert(
                request_id,
                TrackedRequest {
                    method,
                    created_at: Instant::now(),
                },
            );
        }
    }

    pub(super) fn prune_tracked_requests(&mut self) {
        self.forwarded_request_methods_by_id.retain(|_, tracked| {
            tracked.created_at.elapsed()
                < Duration::from_millis(super::FORWARDED_REQUEST_METHOD_TTL_MS)
        });
        self.relay_sanitized_response_methods_by_id
            .retain(|_, tracked| {
                tracked.created_at.elapsed()
                    < Duration::from_millis(super::FORWARDED_REQUEST_METHOD_TTL_MS)
            });
    }

    pub(super) fn sanitize_relay_bound_codex_message(&mut self, raw_message: String) -> String {
        self.prune_tracked_requests();
        let Ok(parsed) = serde_json::from_str::<Value>(&raw_message) else {
            return raw_message;
        };
        let Some(response_id) = parsed.get("id").and_then(stringify_request_id) else {
            return raw_message;
        };
        let Some(tracked) = self
            .relay_sanitized_response_methods_by_id
            .remove(&response_id)
        else {
            return raw_message;
        };

        sanitize_thread_history_images_for_relay(raw_message, &tracked.method)
    }

    pub(super) fn remember_thread_from_message(&mut self, source: &str, raw_message: &str) {
        let context = extract_bridge_message_context(raw_message);
        if context.thread_id.is_empty() {
            return;
        }

        let _ = remember_active_thread(&context.thread_id, source);
        if matches!(context.method.as_str(), "turn/start" | "turn/started") {
            let key = format!(
                "{}|{}",
                context.thread_id,
                context
                    .turn_id
                    .clone()
                    .unwrap_or_else(|| "pending-turn".to_owned())
            );
            if self
                .context_usage_watch
                .as_ref()
                .map(|watch| watch.key.as_str())
                == Some(key.as_str())
            {
                return;
            }

            self.context_usage_watch = Some(super::ContextUsageWatch {
                key,
                thread_id: context.thread_id,
                turn_id: context.turn_id,
                started_at: Instant::now(),
                last_usage_json: None,
            });
        }
    }

    pub(super) fn poll_context_usage(&mut self) -> Option<String> {
        let watch = self.context_usage_watch.as_mut()?;
        if watch.started_at.elapsed() > Duration::from_secs(90) {
            self.context_usage_watch = None;
            return None;
        }

        let result =
            crate::rollout::thread_context_read(&watch.thread_id, watch.turn_id.as_deref());
        let usage = result.get("usage")?.clone();
        let usage_json = serde_json::to_string(&usage).ok()?;
        if watch.last_usage_json.as_deref() == Some(usage_json.as_str()) {
            return None;
        }

        watch.last_usage_json = Some(usage_json);
        Some(
            json!({
                "method": "thread/tokenUsage/updated",
                "params": {
                    "threadId": watch.thread_id,
                    "usage": usage,
                }
            })
            .to_string(),
        )
    }
}
