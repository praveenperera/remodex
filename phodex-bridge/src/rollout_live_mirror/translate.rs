use serde_json::{json, Value};

use super::read_string_value;
use super::state::{is_desktop_rollout_origin, CommandCall, MirrorState};

pub(super) fn synthesize_notifications_from_rollout_entry(
    entry: &Value,
    state: &mut MirrorState,
    thread_id: &str,
) -> Vec<Value> {
    match read_string_value(entry.get("type")).as_deref() {
        Some("session_meta") => session_meta_notifications(entry, state),
        Some("event_msg") => event_message_notifications(entry, state, thread_id),
        Some("response_item") => response_item_notifications(entry, state, thread_id),
        _ => Vec::new(),
    }
}

fn session_meta_notifications(entry: &Value, state: &mut MirrorState) -> Vec<Value> {
    state.update_session_meta(entry.get("payload"));
    if !is_desktop_rollout_origin(state.session_meta.as_ref()) {
        state.is_desktop_origin = Some(false);
    } else if state.is_desktop_origin.is_none() {
        state.is_desktop_origin = Some(true);
    }
    Vec::new()
}

fn event_message_notifications(
    entry: &Value,
    state: &mut MirrorState,
    thread_id: &str,
) -> Vec<Value> {
    if state.is_desktop_origin == Some(false) {
        return Vec::new();
    }

    let payload = entry.get("payload").unwrap_or(&Value::Null);
    match read_string_value(payload.get("type")).as_deref() {
        Some("task_started") => task_started_notifications(payload, state, thread_id),
        Some("user_message") => user_message_notifications(payload, state, thread_id),
        Some("task_complete") => task_completed_notifications(payload, state, thread_id),
        Some("agent_reasoning") => {
            let text = read_string_value(payload.get("message"))
                .or_else(|| read_string_value(payload.get("text")))
                .or_else(|| read_string_value(payload.get("summary")));
            reasoning_notifications(state, thread_id, text)
        }
        Some("agent_message") => agent_message_notifications(payload, state, thread_id),
        _ => Vec::new(),
    }
}

fn response_item_notifications(
    entry: &Value,
    state: &mut MirrorState,
    thread_id: &str,
) -> Vec<Value> {
    if state.is_desktop_origin == Some(false) {
        return Vec::new();
    }

    let payload = entry.get("payload").unwrap_or(&Value::Null);
    match read_string_value(payload.get("type")).as_deref() {
        Some("reasoning") => {
            reasoning_notifications(state, thread_id, extract_reasoning_text(payload))
        }
        Some("function_call") => tool_start_notifications(state, thread_id, payload),
        Some("function_call_output") => tool_output_notifications(state, thread_id, payload),
        _ => Vec::new(),
    }
}

fn task_started_notifications(
    payload: &Value,
    state: &mut MirrorState,
    thread_id: &str,
) -> Vec<Value> {
    let Some(turn_id) = turn_id_from_payload(payload) else {
        return Vec::new();
    };

    state.mark_turn_started(thread_id, &turn_id);

    let mut notifications = vec![create_notification(
        "turn/started",
        json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "id": turn_id,
        }),
    )];
    notifications.extend(ensure_thinking_notifications(state, thread_id));
    notifications
}

fn user_message_notifications(payload: &Value, state: &MirrorState, thread_id: &str) -> Vec<Value> {
    let Some(message) = read_string_value(payload.get("message"))
        .or_else(|| read_string_value(payload.get("text")))
    else {
        return Vec::new();
    };
    let turn_id = turn_id_from_payload(payload)
        .or_else(|| state.active_turn_id.clone())
        .unwrap_or_default();

    vec![create_notification(
        "codex/event/user_message",
        json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "message": message,
        }),
    )]
}

fn task_completed_notifications(
    payload: &Value,
    state: &mut MirrorState,
    thread_id: &str,
) -> Vec<Value> {
    let Some(turn_id) = turn_id_from_payload(payload).or_else(|| state.active_turn_id.clone())
    else {
        return Vec::new();
    };

    let notifications = vec![create_notification(
        "turn/completed",
        json!({
            "threadId": thread_id,
            "turnId": turn_id.clone(),
            "id": turn_id,
        }),
    )];
    state.reset_run_state();
    notifications
}

fn agent_message_notifications(
    payload: &Value,
    state: &MirrorState,
    thread_id: &str,
) -> Vec<Value> {
    let Some(message) = read_string_value(payload.get("message"))
        .or_else(|| read_string_value(payload.get("text")))
    else {
        return Vec::new();
    };
    if !should_mirror_agent_message(payload) {
        return Vec::new();
    }
    let turn_id = turn_id_from_payload(payload)
        .or_else(|| state.active_turn_id.clone())
        .unwrap_or_default();

    vec![create_notification(
        "codex/event/agent_message",
        json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "message": message,
        }),
    )]
}

fn reasoning_notifications(
    state: &mut MirrorState,
    thread_id: &str,
    text: Option<String>,
) -> Vec<Value> {
    let Some(active_turn_id) = state.active_turn_id.clone() else {
        return Vec::new();
    };

    let Some(delta) = text else {
        return ensure_thinking_notifications(state, thread_id);
    };

    state.has_thinking = true;
    let item_id = state.ensure_reasoning_item_id(thread_id, &active_turn_id);
    vec![create_notification(
        "item/reasoning/textDelta",
        json!({
            "threadId": thread_id,
            "turnId": active_turn_id,
            "itemId": item_id,
            "delta": delta,
        }),
    )]
}

fn tool_start_notifications(
    state: &mut MirrorState,
    thread_id: &str,
    payload: &Value,
) -> Vec<Value> {
    let Some(active_turn_id) = state.active_turn_id.clone() else {
        return Vec::new();
    };
    let Some(call_id) = read_string_value(payload.get("call_id"))
        .or_else(|| read_string_value(payload.get("callId")))
    else {
        return Vec::new();
    };
    let Some(tool_name) = read_string_value(payload.get("name")) else {
        return Vec::new();
    };

    let arguments_object = parse_tool_arguments(payload.get("arguments"));
    let command = resolve_tool_command(&tool_name, &arguments_object);
    let cwd = resolve_tool_working_directory(&arguments_object, state);

    state.command_calls.insert(
        call_id.clone(),
        CommandCall {
            tool_name: tool_name.clone(),
            command: command.clone(),
            cwd: cwd.clone(),
        },
    );

    let mut notifications = ensure_thinking_notifications(state, thread_id);
    if is_command_tool_name(&tool_name) {
        notifications.push(create_notification(
            "codex/event/exec_command_begin",
            json!({
                "threadId": thread_id,
                "turnId": active_turn_id,
                "call_id": call_id,
                "command": command,
                "cwd": cwd,
                "status": "running",
            }),
        ));
        return notifications;
    }

    let Some(activity_message) = generic_tool_activity_message(&tool_name) else {
        return notifications;
    };
    notifications.push(create_notification(
        "codex/event/background_event",
        json!({
            "threadId": thread_id,
            "turnId": active_turn_id,
            "call_id": call_id,
            "message": activity_message,
        }),
    ));
    notifications
}

fn tool_output_notifications(
    state: &mut MirrorState,
    thread_id: &str,
    payload: &Value,
) -> Vec<Value> {
    let Some(active_turn_id) = state.active_turn_id.clone() else {
        return Vec::new();
    };
    let Some(call_id) = read_string_value(payload.get("call_id"))
        .or_else(|| read_string_value(payload.get("callId")))
    else {
        return Vec::new();
    };

    let Some(tool_call) = state.command_calls.remove(&call_id) else {
        return Vec::new();
    };
    if !is_command_tool_name(&tool_call.tool_name) {
        return Vec::new();
    }

    let output = read_string_value(payload.get("output")).unwrap_or_default();
    let mut notifications = ensure_thinking_notifications(state, thread_id);
    if !output.is_empty() {
        notifications.push(create_notification(
            "codex/event/exec_command_output_delta",
            json!({
                "threadId": thread_id,
                "turnId": active_turn_id,
                "call_id": call_id,
                "command": tool_call.command,
                "cwd": tool_call.cwd,
                "chunk": output,
            }),
        ));
    }
    notifications.push(create_notification(
        "codex/event/exec_command_end",
        json!({
            "threadId": thread_id,
            "turnId": active_turn_id,
            "call_id": call_id,
            "command": tool_call.command,
            "cwd": tool_call.cwd,
            "status": "completed",
            "output": output,
        }),
    ));
    notifications
}

fn ensure_thinking_notifications(state: &mut MirrorState, thread_id: &str) -> Vec<Value> {
    let Some(active_turn_id) = state.active_turn_id.clone() else {
        return Vec::new();
    };
    if state.has_thinking {
        return Vec::new();
    }

    state.has_thinking = true;
    let item_id = state.ensure_reasoning_item_id(thread_id, &active_turn_id);
    vec![create_notification(
        "item/reasoning/textDelta",
        json!({
            "threadId": thread_id,
            "turnId": active_turn_id,
            "itemId": item_id,
            "delta": "Thinking...",
        }),
    )]
}

fn turn_id_from_payload(payload: &Value) -> Option<String> {
    read_string_value(payload.get("turn_id")).or_else(|| read_string_value(payload.get("turnId")))
}

fn extract_reasoning_text(payload: &Value) -> Option<String> {
    if let Some(summary) = payload.get("summary").and_then(Value::as_array) {
        let text = summary
            .iter()
            .filter_map(|part| {
                read_string_value(part.get("text"))
                    .or_else(|| read_string_value(part.get("summary")))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            return Some(text);
        }
    }

    read_string_value(payload.get("text")).or_else(|| read_string_value(payload.get("content")))
}

fn parse_tool_arguments(raw_arguments: Option<&Value>) -> Value {
    raw_arguments
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| json!({}))
}

fn resolve_tool_command(tool_name: &str, arguments_object: &Value) -> String {
    if is_command_tool_name(tool_name) {
        return read_string_value(arguments_object.get("cmd"))
            .or_else(|| read_string_value(arguments_object.get("command")))
            .or_else(|| read_string_value(arguments_object.get("raw_command")))
            .or_else(|| read_string_value(arguments_object.get("rawCommand")))
            .unwrap_or_else(|| tool_name.to_owned());
    }

    tool_name.to_owned()
}

fn resolve_tool_working_directory(arguments_object: &Value, state: &MirrorState) -> String {
    read_string_value(arguments_object.get("workdir"))
        .or_else(|| read_string_value(arguments_object.get("cwd")))
        .or_else(|| read_string_value(arguments_object.get("working_directory")))
        .or_else(|| {
            state
                .session_meta
                .as_ref()
                .map(|meta| meta.cwd.clone())
                .filter(|cwd| !cwd.is_empty())
        })
        .unwrap_or_default()
}

fn is_command_tool_name(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "exec_command" | "shell_command"
    )
}

fn generic_tool_activity_message(tool_name: &str) -> Option<String> {
    match tool_name.to_ascii_lowercase().as_str() {
        "apply_patch" => Some("Applying patch".to_owned()),
        "write_stdin" => Some("Writing to terminal".to_owned()),
        "read_thread_terminal" => Some("Reading terminal output".to_owned()),
        _ if !tool_name.trim().is_empty() => Some(format!("Running {tool_name}")),
        _ => None,
    }
}

fn should_mirror_agent_message(payload: &Value) -> bool {
    !read_string_value(payload.get("phase"))
        .unwrap_or_default()
        .eq_ignore_ascii_case("commentary")
}

fn create_notification(method: &str, params: Value) -> Value {
    json!({
        "method": method,
        "params": params,
    })
}
