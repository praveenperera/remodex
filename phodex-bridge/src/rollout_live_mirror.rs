use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::rollout::find_recent_rollout_file_for_context_read;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct RolloutLiveMirrorController {
    sessions_root: PathBuf,
    mirrors_by_thread_id: HashMap<String, ThreadRolloutMirror>,
}

struct ThreadRolloutMirror {
    thread_id: String,
    started_at: Instant,
    last_activity_at: Instant,
    rollout_path: Option<PathBuf>,
    last_size: u64,
    partial_line: String,
    did_bootstrap: bool,
    state: MirrorState,
}

#[derive(Default)]
struct MirrorState {
    session_meta: Option<SessionMeta>,
    is_desktop_origin: Option<bool>,
    active_turn_id: Option<String>,
    reasoning_item_id: Option<String>,
    has_thinking: bool,
    command_calls: HashMap<String, CommandCall>,
}

struct SessionMeta {
    originator: String,
    source: String,
    cwd: String,
}

struct CommandCall {
    tool_name: String,
    command: String,
    cwd: String,
}

impl RolloutLiveMirrorController {
    pub fn new(sessions_root: PathBuf) -> Self {
        Self {
            sessions_root,
            mirrors_by_thread_id: HashMap::new(),
        }
    }

    pub fn observe_inbound(&mut self, raw_message: &str) {
        let Ok(request) = serde_json::from_str::<Value>(raw_message) else {
            return;
        };
        let method = read_string(request.get("method"));
        if method != "thread/read" && method != "thread/resume" {
            return;
        }

        let thread_id = request
            .get("params")
            .and_then(Value::as_object)
            .and_then(|params| {
                read_string_value(params.get("threadId"))
                    .or_else(|| read_string_value(params.get("thread_id")))
            });
        let Some(thread_id) = thread_id else {
            return;
        };

        if let Some(existing) = self.mirrors_by_thread_id.get_mut(&thread_id) {
            existing.bump();
            return;
        }

        self.mirrors_by_thread_id
            .insert(thread_id.clone(), ThreadRolloutMirror::new(thread_id));
    }

    pub fn poll_notifications(&mut self) -> Vec<String> {
        let mut notifications = Vec::new();
        self.mirrors_by_thread_id.retain(|_, mirror| {
            let (keep, mut next_notifications) = mirror.poll(&self.sessions_root);
            notifications.append(&mut next_notifications);
            keep
        });
        notifications
    }

    pub fn stop_all(&mut self) {
        self.mirrors_by_thread_id.clear();
    }
}

impl ThreadRolloutMirror {
    fn new(thread_id: String) -> Self {
        let now = Instant::now();
        Self {
            state: MirrorState::default(),
            thread_id,
            started_at: now,
            last_activity_at: now,
            rollout_path: None,
            last_size: 0,
            partial_line: String::new(),
            did_bootstrap: false,
        }
    }

    fn bump(&mut self) {
        self.last_activity_at = Instant::now();
    }

    fn poll(&mut self, sessions_root: &Path) -> (bool, Vec<String>) {
        if self.rollout_path.is_none() {
            if self.started_at.elapsed() >= LOOKUP_TIMEOUT {
                return (false, Vec::new());
            }

            self.rollout_path =
                find_recent_rollout_file_for_context_read(sessions_root, &self.thread_id, None);
            if self.rollout_path.is_none() {
                return (true, Vec::new());
            }
        }

        let Some(rollout_path) = self.rollout_path.clone() else {
            return (true, Vec::new());
        };

        let file_size = match fs::metadata(&rollout_path) {
            Ok(metadata) => metadata.len(),
            Err(_) => return (false, Vec::new()),
        };

        if !self.did_bootstrap {
            self.did_bootstrap = true;
            self.last_size = file_size;
            self.last_activity_at = Instant::now();
            let notifications = self.bootstrap_from_existing_rollout(&rollout_path);
            if self.state.is_desktop_origin == Some(false) {
                return (false, notifications);
            }
            return (true, notifications);
        }

        if file_size > self.last_size {
            let chunk = match read_file_slice(&rollout_path, self.last_size, file_size) {
                Ok(chunk) => chunk,
                Err(_) => return (false, Vec::new()),
            };
            self.last_size = file_size;
            self.last_activity_at = Instant::now();

            if chunk.is_empty() {
                return (true, Vec::new());
            }

            let combined = format!("{}{}", self.partial_line, chunk);
            let mut lines: Vec<&str> = combined.split('\n').collect();
            self.partial_line = lines.pop().unwrap_or_default().to_owned();
            let notifications = self.process_rollout_lines(lines.into_iter());
            if self.state.is_desktop_origin == Some(false) {
                return (false, notifications);
            }
            return (true, notifications);
        }

        if self.last_activity_at.elapsed() >= IDLE_TIMEOUT {
            return (false, Vec::new());
        }

        (true, Vec::new())
    }

    fn bootstrap_from_existing_rollout(&mut self, rollout_path: &Path) -> Vec<String> {
        let Ok(raw) = fs::read_to_string(rollout_path) else {
            return Vec::new();
        };

        let mut active_run_lines = Vec::new();
        let mut inside_active_run = false;
        let mut active_turn_id: Option<String> = None;
        let mut pending_user_prelude_line: Option<String> = None;

        for raw_line in raw.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }

            let Ok(parsed) = serde_json::from_str::<Value>(line) else {
                continue;
            };

            if read_string(parsed.get("type")) == "session_meta" {
                populate_session_meta_state(&mut self.state, parsed.get("payload"));
            }

            let task_event_type = if read_string(parsed.get("type")) == "event_msg" {
                read_string(
                    parsed
                        .get("payload")
                        .and_then(|payload| payload.get("type")),
                )
            } else {
                String::new()
            };

            if task_event_type == "user_message" {
                pending_user_prelude_line = Some(line.to_owned());
            }

            if task_event_type == "task_started" {
                inside_active_run = true;
                active_turn_id = read_string_value(
                    parsed
                        .get("payload")
                        .and_then(|payload| payload.get("turn_id")),
                )
                .or_else(|| {
                    read_string_value(
                        parsed
                            .get("payload")
                            .and_then(|payload| payload.get("turnId")),
                    )
                });
                active_run_lines.clear();
                if let Some(pending_user_prelude_line) = pending_user_prelude_line.as_ref() {
                    active_run_lines.push(pending_user_prelude_line.clone());
                }
                active_run_lines.push(line.to_owned());
                continue;
            }

            if !inside_active_run {
                continue;
            }

            active_run_lines.push(line.to_owned());
            if task_event_type == "task_complete" {
                inside_active_run = false;
                active_turn_id = None;
                active_run_lines.clear();
                pending_user_prelude_line = None;
            }
        }

        if !is_desktop_rollout_origin(self.state.session_meta.as_ref()) {
            self.state.is_desktop_origin = Some(false);
            return Vec::new();
        }

        self.state.is_desktop_origin = Some(true);
        self.state.active_turn_id = active_turn_id;
        self.process_rollout_lines(active_run_lines.iter().map(String::as_str))
    }

    fn process_rollout_lines<'a>(&mut self, lines: impl Iterator<Item = &'a str>) -> Vec<String> {
        let mut notifications = Vec::new();
        for raw_line in lines {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }

            let Ok(parsed) = serde_json::from_str::<Value>(line) else {
                continue;
            };

            for notification in synthesize_notifications_from_rollout_entry(
                &parsed,
                &mut self.state,
                &self.thread_id,
            ) {
                notifications.push(notification.to_string());
            }
        }
        notifications
    }
}

fn synthesize_notifications_from_rollout_entry(
    entry: &Value,
    state: &mut MirrorState,
    thread_id: &str,
) -> Vec<Value> {
    if read_string(entry.get("type")) == "session_meta" {
        populate_session_meta_state(state, entry.get("payload"));
        if !is_desktop_rollout_origin(state.session_meta.as_ref()) {
            state.is_desktop_origin = Some(false);
        } else if state.is_desktop_origin.is_none() {
            state.is_desktop_origin = Some(true);
        }
        return Vec::new();
    }

    if state.is_desktop_origin == Some(false) {
        return Vec::new();
    }

    if read_string(entry.get("type")) == "event_msg" {
        let payload = entry.get("payload").unwrap_or(&Value::Null);
        let event_type = read_string(payload.get("type"));

        if event_type == "task_started" {
            let Some(turn_id) = read_string_value(payload.get("turn_id"))
                .or_else(|| read_string_value(payload.get("turnId")))
            else {
                return Vec::new();
            };

            state.active_turn_id = Some(turn_id.clone());
            state.reasoning_item_id =
                Some(build_synthetic_item_id("thinking", thread_id, &turn_id));
            state.has_thinking = false;
            state.command_calls.clear();

            let mut notifications = Vec::new();
            notifications.push(create_notification(
                "turn/started",
                json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "id": turn_id,
                }),
            ));
            notifications.extend(ensure_thinking_notifications(state, thread_id));
            return notifications;
        }

        if event_type == "user_message" {
            let Some(message) = read_string_value(payload.get("message"))
                .or_else(|| read_string_value(payload.get("text")))
            else {
                return Vec::new();
            };

            return vec![create_notification(
                "codex/event/user_message",
                json!({
                    "threadId": thread_id,
                    "turnId": read_string_value(payload.get("turn_id"))
                        .or_else(|| read_string_value(payload.get("turnId")))
                        .or_else(|| state.active_turn_id.clone())
                        .unwrap_or_default(),
                    "message": message,
                }),
            )];
        }

        if event_type == "task_complete" {
            let Some(turn_id) = read_string_value(payload.get("turn_id"))
                .or_else(|| read_string_value(payload.get("turnId")))
                .or_else(|| state.active_turn_id.clone())
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
            reset_run_state(state);
            return notifications;
        }

        if event_type == "agent_reasoning" {
            let text = read_string_value(payload.get("message"))
                .or_else(|| read_string_value(payload.get("text")))
                .or_else(|| read_string_value(payload.get("summary")));
            return reasoning_notifications(state, thread_id, text);
        }

        if event_type == "agent_message" {
            let Some(message) = read_string_value(payload.get("message"))
                .or_else(|| read_string_value(payload.get("text")))
            else {
                return Vec::new();
            };
            if !should_mirror_agent_message(payload) {
                return Vec::new();
            }

            return vec![create_notification(
                "codex/event/agent_message",
                json!({
                    "threadId": thread_id,
                    "turnId": read_string_value(payload.get("turn_id"))
                        .or_else(|| read_string_value(payload.get("turnId")))
                        .or_else(|| state.active_turn_id.clone())
                        .unwrap_or_default(),
                    "message": message,
                }),
            )];
        }

        return Vec::new();
    }

    if read_string(entry.get("type")) != "response_item" {
        return Vec::new();
    }

    let payload = entry.get("payload").unwrap_or(&Value::Null);
    let item_type = read_string(payload.get("type"));
    if item_type == "reasoning" {
        return reasoning_notifications(state, thread_id, extract_reasoning_text(payload));
    }
    if item_type == "function_call" {
        return tool_start_notifications(state, thread_id, payload);
    }
    if item_type == "function_call_output" {
        return tool_output_notifications(state, thread_id, payload);
    }

    Vec::new()
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
    vec![create_notification(
        "item/reasoning/textDelta",
        json!({
            "threadId": thread_id,
            "turnId": active_turn_id,
            "itemId": state
                .reasoning_item_id
                .clone()
                .unwrap_or_else(|| build_synthetic_item_id("thinking", thread_id, state.active_turn_id.as_deref().unwrap_or_default())),
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

    if is_command_tool_name(&tool_name) {
        let mut notifications = ensure_thinking_notifications(state, thread_id);
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
        return ensure_thinking_notifications(state, thread_id);
    };

    let mut notifications = ensure_thinking_notifications(state, thread_id);
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
    if state.reasoning_item_id.is_none() {
        state.reasoning_item_id = Some(build_synthetic_item_id(
            "thinking",
            thread_id,
            &active_turn_id,
        ));
    }

    vec![create_notification(
        "item/reasoning/textDelta",
        json!({
            "threadId": thread_id,
            "turnId": active_turn_id,
            "itemId": state.reasoning_item_id.clone().unwrap_or_default(),
            "delta": "Thinking...",
        }),
    )]
}

fn populate_session_meta_state(state: &mut MirrorState, payload: Option<&Value>) {
    let Some(payload) = payload else {
        return;
    };

    state.session_meta = Some(SessionMeta {
        originator: read_string(payload.get("originator")),
        source: read_string(payload.get("source")),
        cwd: read_string(payload.get("cwd")),
    });
}

fn is_desktop_rollout_origin(session_meta: Option<&SessionMeta>) -> bool {
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
    !read_string(payload.get("phase")).eq_ignore_ascii_case("commentary")
}

fn create_notification(method: &str, params: Value) -> Value {
    json!({
        "method": method,
        "params": params,
    })
}

fn build_synthetic_item_id(kind: &str, thread_id: &str, turn_id: &str) -> String {
    format!("rollout-{kind}:{thread_id}:{turn_id}")
}

fn reset_run_state(state: &mut MirrorState) {
    state.active_turn_id = None;
    state.reasoning_item_id = None;
    state.has_thinking = false;
    state.command_calls.clear();
}

fn read_file_slice(path: &Path, start: u64, end_exclusive: u64) -> std::io::Result<String> {
    if end_exclusive <= start {
        return Ok(String::new());
    }

    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = vec![0_u8; (end_exclusive - start) as usize];
    let bytes_read = file.read(&mut buffer)?;
    buffer.truncate(bytes_read);
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn read_string(value: Option<&Value>) -> String {
    read_string_value(value).unwrap_or_default()
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::Duration;

    use serde_json::{json, Value};
    use tempfile::tempdir;

    use super::RolloutLiveMirrorController;

    #[test]
    fn desktop_origin_resume_replays_live_run() {
        let temp = tempdir().unwrap();
        let rollout_path = write_rollout(
            temp.path(),
            "thread-desktop",
            "Codex Desktop",
            "vscode",
            &[
                task_started("turn-live"),
                function_call(
                    "call-1",
                    "exec_command",
                    json!({
                        "cmd": "git status",
                        "workdir": "/repo",
                    }),
                ),
                function_call_output("call-1", "On branch main"),
            ],
        );

        let mut controller = RolloutLiveMirrorController::new(temp.path().join("sessions"));
        controller.observe_inbound(
            &json!({
                "method": "thread/resume",
                "params": {
                    "threadId": "thread-desktop",
                }
            })
            .to_string(),
        );

        let notifications = controller.poll_notifications();
        assert!(rollout_path.exists());
        assert_eq!(
            notification_methods(&notifications),
            vec![
                "turn/started",
                "item/reasoning/textDelta",
                "codex/event/exec_command_begin",
                "codex/event/exec_command_output_delta",
                "codex/event/exec_command_end",
            ]
        );
    }

    #[test]
    fn phone_origin_rollouts_do_not_emit_notifications() {
        let temp = tempdir().unwrap();
        write_rollout(
            temp.path(),
            "thread-phone",
            "codexmobile_ios",
            "ios",
            &[task_started("turn-live")],
        );

        let mut controller = RolloutLiveMirrorController::new(temp.path().join("sessions"));
        controller.observe_inbound(
            &json!({
                "method": "thread/read",
                "params": {
                    "threadId": "thread-phone",
                }
            })
            .to_string(),
        );

        assert!(controller.poll_notifications().is_empty());
    }

    #[test]
    fn desktop_origin_growth_streams_new_notifications() {
        let temp = tempdir().unwrap();
        let rollout_path = write_rollout(temp.path(), "thread-grow", "codex_vscode", "vscode", &[]);

        let mut controller = RolloutLiveMirrorController::new(temp.path().join("sessions"));
        controller.observe_inbound(
            &json!({
                "method": "thread/resume",
                "params": {
                    "threadId": "thread-grow",
                }
            })
            .to_string(),
        );
        assert!(controller.poll_notifications().is_empty());

        append_rollout_lines(
            &rollout_path,
            &[
                task_started("turn-next"),
                function_call("call-2", "apply_patch", json!({})),
            ],
        );
        thread::sleep(Duration::from_millis(5));

        let notifications = controller.poll_notifications();
        assert_eq!(
            notification_methods(&notifications),
            vec![
                "turn/started",
                "item/reasoning/textDelta",
                "codex/event/background_event",
            ]
        );
    }

    fn notification_methods(raw_notifications: &[String]) -> Vec<String> {
        raw_notifications
            .iter()
            .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
            .filter_map(|notification| {
                notification
                    .get("method")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect()
    }

    fn write_rollout(
        root: &Path,
        thread_id: &str,
        originator: &str,
        source: &str,
        lines: &[String],
    ) -> PathBuf {
        let thread_dir = root.join("sessions").join("2026").join("03").join("15");
        fs::create_dir_all(&thread_dir).unwrap();
        let rollout_path =
            thread_dir.join(format!("rollout-2026-03-15T19-47-36-{thread_id}.jsonl"));
        let header = json!({
            "timestamp": "2026-03-15T19:47:36.019Z",
            "type": "session_meta",
            "payload": {
                "id": thread_id,
                "cwd": "/repo",
                "originator": originator,
                "source": source,
            },
        })
        .to_string();
        let mut contents = vec![header];
        contents.extend(lines.iter().cloned());
        contents.push(String::new());
        fs::write(&rollout_path, contents.join("\n")).unwrap();
        rollout_path
    }

    fn append_rollout_lines(path: &Path, lines: &[String]) {
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut file| {
                use std::io::Write;
                file.write_all(format!("{}\n", lines.join("\n")).as_bytes())
            })
            .unwrap();
    }

    fn task_started(turn_id: &str) -> String {
        json!({
            "timestamp": "2026-03-15T19:47:37.000Z",
            "type": "event_msg",
            "payload": {
                "type": "task_started",
                "turn_id": turn_id,
                "model_context_window": 258400,
            },
        })
        .to_string()
    }

    fn function_call(call_id: &str, name: &str, arguments: Value) -> String {
        json!({
            "timestamp": "2026-03-15T19:47:38.000Z",
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments.to_string(),
            },
        })
        .to_string()
    }

    fn function_call_output(call_id: &str, output: &str) -> String {
        json!({
            "timestamp": "2026-03-15T19:47:39.000Z",
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            },
        })
        .to_string()
    }
}
