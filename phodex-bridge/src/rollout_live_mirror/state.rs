use std::collections::HashMap;

use serde_json::Value;

use super::read_string;

#[derive(Default)]
pub(super) struct MirrorState {
    pub(super) session_meta: Option<SessionMeta>,
    pub(super) is_desktop_origin: Option<bool>,
    pub(super) active_turn_id: Option<String>,
    pub(super) reasoning_item_id: Option<String>,
    pub(super) has_thinking: bool,
    pub(super) command_calls: HashMap<String, CommandCall>,
}

pub(super) struct SessionMeta {
    pub(super) originator: String,
    pub(super) source: String,
    pub(super) cwd: String,
}

pub(super) struct CommandCall {
    pub(super) tool_name: String,
    pub(super) command: String,
    pub(super) cwd: String,
}

impl MirrorState {
    pub(super) fn update_session_meta(&mut self, payload: Option<&Value>) {
        let Some(payload) = payload else {
            return;
        };

        self.session_meta = Some(SessionMeta {
            originator: read_string(payload.get("originator")),
            source: read_string(payload.get("source")),
            cwd: read_string(payload.get("cwd")),
        });
    }

    pub(super) fn mark_turn_started(&mut self, thread_id: &str, turn_id: &str) {
        self.active_turn_id = Some(turn_id.to_owned());
        self.reasoning_item_id = Some(build_synthetic_item_id("thinking", thread_id, turn_id));
        self.has_thinking = false;
        self.command_calls.clear();
    }

    pub(super) fn ensure_reasoning_item_id(&mut self, thread_id: &str, turn_id: &str) -> String {
        if self.reasoning_item_id.is_none() {
            self.reasoning_item_id = Some(build_synthetic_item_id("thinking", thread_id, turn_id));
        }

        self.reasoning_item_id.clone().unwrap_or_default()
    }

    pub(super) fn reset_run_state(&mut self) {
        self.active_turn_id = None;
        self.reasoning_item_id = None;
        self.has_thinking = false;
        self.command_calls.clear();
    }
}

pub(super) fn is_desktop_rollout_origin(session_meta: Option<&SessionMeta>) -> bool {
    let Some(session_meta) = session_meta else {
        return false;
    };
    let originator = session_meta.originator.to_ascii_lowercase();
    let source = session_meta.source.to_ascii_lowercase();

    if originator.is_empty() && source.is_empty() {
        return false;
    }
    if originator.contains("mobile") || originator.contains("ios") {
        return false;
    }

    originator.contains("desktop")
        || originator.contains("vscode")
        || source.contains("vscode")
        || source.contains("desktop")
}

fn build_synthetic_item_id(kind: &str, thread_id: &str, turn_id: &str) -> String {
    format!("rollout-{kind}:{thread_id}:{turn_id}")
}
