mod controller;
mod state;
mod translate;

use serde_json::Value;

pub(crate) use controller::RolloutLiveMirrorController;

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
