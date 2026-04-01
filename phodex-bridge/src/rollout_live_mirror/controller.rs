use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::rollout::find_recent_rollout_file_for_context_read;

use super::read_string_value;
use super::state::{is_desktop_rollout_origin, MirrorState};
use super::translate::synthesize_notifications_from_rollout_entry;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct RolloutLiveMirrorController {
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

impl RolloutLiveMirrorController {
    pub(crate) fn new(sessions_root: PathBuf) -> Self {
        Self {
            sessions_root,
            mirrors_by_thread_id: HashMap::new(),
        }
    }

    pub(crate) fn observe_inbound(&mut self, raw_message: &str) {
        let Ok(request) = serde_json::from_str::<Value>(raw_message) else {
            return;
        };
        let method = read_string_value(request.get("method")).unwrap_or_default();
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

    pub(crate) fn poll_notifications(&mut self) -> Vec<String> {
        let mut notifications = Vec::new();
        self.mirrors_by_thread_id.retain(|_, mirror| {
            let (keep, mut next_notifications) = mirror.poll(&self.sessions_root);
            notifications.append(&mut next_notifications);
            keep
        });
        notifications
    }

    pub(crate) fn stop_all(&mut self) {
        self.mirrors_by_thread_id.clear();
    }
}

impl ThreadRolloutMirror {
    fn new(thread_id: String) -> Self {
        let now = Instant::now();
        Self {
            thread_id,
            started_at: now,
            last_activity_at: now,
            rollout_path: None,
            last_size: 0,
            partial_line: String::new(),
            did_bootstrap: false,
            state: MirrorState::default(),
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

            if read_string_value(parsed.get("type")).as_deref() == Some("session_meta") {
                self.state.update_session_meta(parsed.get("payload"));
            }

            let task_event_type =
                if read_string_value(parsed.get("type")).as_deref() == Some("event_msg") {
                    read_string_value(
                        parsed
                            .get("payload")
                            .and_then(|payload| payload.get("type")),
                    )
                } else {
                    None
                };

            if task_event_type.as_deref() == Some("user_message") {
                pending_user_prelude_line = Some(line.to_owned());
            }

            if task_event_type.as_deref() == Some("task_started") {
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
            if task_event_type.as_deref() == Some("task_complete") {
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
