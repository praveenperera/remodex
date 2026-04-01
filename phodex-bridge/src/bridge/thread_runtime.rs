use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use super::support::{read_string, stringify_request_id};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ThreadRuntimeContext {
    model: Option<String>,
    model_provider: Option<String>,
}

impl ThreadRuntimeContext {
    fn is_empty(&self) -> bool {
        self.model.is_none() && self.model_provider.is_none()
    }

    fn merge(&mut self, incoming: &Self) {
        if self.model.is_none() {
            self.model = incoming.model.clone();
        }
        if self.model_provider.is_none() {
            self.model_provider = incoming.model_provider.clone();
        }
    }
}

#[derive(Clone)]
struct PendingThreadStartContext {
    context: ThreadRuntimeContext,
    created_at: Instant,
}

#[derive(Default)]
pub(super) struct ThreadRuntimeRegistry {
    pending_thread_start_contexts_by_request_id: HashMap<String, PendingThreadStartContext>,
    thread_runtime_context_by_thread_id: HashMap<String, ThreadRuntimeContext>,
}

impl ThreadRuntimeRegistry {
    pub(super) fn prune_stale(&mut self) {
        self.pending_thread_start_contexts_by_request_id
            .retain(|_, tracked| {
                tracked.created_at.elapsed()
                    < Duration::from_millis(super::FORWARDED_REQUEST_METHOD_TTL_MS)
            });
    }

    pub(super) fn remember_pending_thread_start_context(&mut self, parsed: &Value) {
        let method = read_string(parsed.get("method")).unwrap_or_default();
        if method != "thread/start" {
            return;
        }

        let Some(request_id) = parsed.get("id").and_then(stringify_request_id) else {
            return;
        };
        let context = parsed
            .get("params")
            .map(thread_runtime_context_from_value)
            .unwrap_or_default();
        if context.is_empty() {
            return;
        }

        self.pending_thread_start_contexts_by_request_id.insert(
            request_id,
            PendingThreadStartContext {
                context,
                created_at: Instant::now(),
            },
        );
    }

    pub(super) fn prepare_outbound_application_message(
        &mut self,
        mut parsed: Value,
        raw_message: String,
    ) -> String {
        let Some(thread_id) = existing_thread_id_from_request(&parsed) else {
            return raw_message;
        };

        let context = self.resolve_existing_thread_runtime_context(&thread_id);
        if rewrite_existing_thread_runtime_request(&mut parsed, context.as_ref()) {
            parsed.to_string()
        } else {
            raw_message
        }
    }

    pub(super) fn update_from_codex_message(&mut self, raw_message: &str) {
        let Ok(parsed) = serde_json::from_str::<Value>(raw_message) else {
            return;
        };

        if let Some(thread_value) = parsed.get("result").and_then(|value| value.get("thread")) {
            self.merge_thread_runtime_context_from_thread_value(thread_value);
        }
        if let Some(thread_value) = parsed.get("params").and_then(|value| value.get("thread")) {
            self.merge_thread_runtime_context_from_thread_value(thread_value);
        }

        if let Some(response_id) = parsed.get("id").and_then(stringify_request_id) {
            let pending_thread_start = self
                .pending_thread_start_contexts_by_request_id
                .remove(&response_id);
            let thread_value = parsed.get("result").and_then(|value| value.get("thread"));
            if let (Some(pending_thread_start), Some(thread_value)) =
                (pending_thread_start, thread_value)
            {
                if let Some(thread_id) = read_string(thread_value.get("id")) {
                    self.merge_thread_runtime_context(&thread_id, pending_thread_start.context);
                }
            }
        }
    }

    fn merge_thread_runtime_context_from_thread_value(&mut self, thread_value: &Value) {
        let Some(thread_id) = read_string(thread_value.get("id")) else {
            return;
        };

        let context = thread_runtime_context_from_value(thread_value);
        self.merge_thread_runtime_context(&thread_id, context);
    }

    fn merge_thread_runtime_context(&mut self, thread_id: &str, incoming: ThreadRuntimeContext) {
        if incoming.is_empty() {
            return;
        }

        self.thread_runtime_context_by_thread_id
            .entry(thread_id.to_owned())
            .and_modify(|existing| existing.merge(&incoming))
            .or_insert(incoming);
    }

    fn resolve_existing_thread_runtime_context(
        &mut self,
        thread_id: &str,
    ) -> Option<ThreadRuntimeContext> {
        let mut resolved = self
            .thread_runtime_context_by_thread_id
            .get(thread_id)
            .cloned()
            .unwrap_or_default();
        if let Some(session_meta) = crate::rollout::read_thread_rollout_session_meta(
            &crate::rollout::resolve_sessions_root(),
            thread_id,
        ) {
            resolved.merge(&thread_runtime_context_from_value(&session_meta));
        }

        if resolved.is_empty() {
            return None;
        }

        self.thread_runtime_context_by_thread_id
            .insert(thread_id.to_owned(), resolved.clone());
        Some(resolved)
    }
}

fn existing_thread_id_from_request(parsed: &Value) -> Option<String> {
    parsed
        .get("params")
        .and_then(Value::as_object)
        .and_then(|params| {
            read_string_value(params.get("threadId"))
                .or_else(|| read_string_value(params.get("thread_id")))
        })
}

fn thread_runtime_context_from_value(value: &Value) -> ThreadRuntimeContext {
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

fn rewrite_existing_thread_runtime_request(
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
                rewrite_turn_start_settings_model(params, context.model.as_ref());
            }
        }
        None => {
            params.remove("model");
            params.remove("modelProvider");
            params.remove("model_provider");
            if method == "turn/start" {
                rewrite_turn_start_settings_model(params, None);
            }
        }
    }

    true
}

fn rewrite_turn_start_settings_model(params: &mut Map<String, Value>, model: Option<&String>) {
    let Some(settings) = params
        .get_mut("collaborationMode")
        .and_then(Value::as_object_mut)
        .and_then(|value| value.get_mut("settings"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };

    rewrite_runtime_identity_field(settings, "model", model);
}

fn rewrite_runtime_identity_field(
    object: &mut Map<String, Value>,
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        rewrite_existing_thread_runtime_request, thread_runtime_context_from_value,
        ThreadRuntimeContext,
    };

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
