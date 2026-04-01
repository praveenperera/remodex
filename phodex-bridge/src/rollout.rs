use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use color_eyre::eyre::{eyre, Result};
use serde_json::{json, Value};
use walkdir::WalkDir;

pub fn resolve_sessions_root() -> PathBuf {
    std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap().join(".codex"))
        .join("sessions")
}

pub fn read_latest_context_window_usage(thread_id: &str, turn_id: Option<&str>) -> Option<Value> {
    let rollout_path = find_recent_rollout_file_for_context_read(thread_id, turn_id)?;
    let raw = fs::read_to_string(rollout_path).ok()?;
    let mut usage = None;
    for line in raw.lines() {
        usage = extract_context_usage_from_rollout_line(line).or(usage);
    }
    usage
}

pub fn thread_context_read(thread_id: &str, turn_id: Option<&str>) -> Value {
    let usage = read_latest_context_window_usage(thread_id, turn_id);
    json!({
        "threadId": thread_id,
        "usage": usage,
        "rolloutPath": find_recent_rollout_file_for_context_read(thread_id, turn_id)
            .map(|path| path.display().to_string()),
    })
}

pub fn watch_thread_rollout(thread_id: Option<&str>) -> Result<()> {
    let resolved_thread_id = thread_id
        .map(ToOwned::to_owned)
        .or_else(|| crate::session_state::read_last_active_thread().map(|state| state.thread_id))
        .ok_or_else(|| eyre!("No thread id provided and no remembered Remodex thread found."))?;
    let rollout_path =
        find_rollout_file_for_thread(resolve_sessions_root(), &resolved_thread_id)
            .ok_or_else(|| eyre!("No rollout file found for thread {resolved_thread_id}."))?;

    let mut offset = fs::metadata(&rollout_path)?.len();
    println!("[remodex] Watching thread {resolved_thread_id}");
    println!("[remodex] Rollout file: {}", rollout_path.display());
    println!("[remodex] Waiting for new persisted events... (Ctrl+C to stop)");

    loop {
        std::thread::sleep(std::time::Duration::from_millis(700));
        let size = fs::metadata(&rollout_path)?.len();
        if size <= offset {
            continue;
        }
        let mut file = fs::File::open(&rollout_path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut chunk = String::new();
        file.read_to_string(&mut chunk)?;
        offset = size;
        for line in chunk.lines() {
            let line = line.trim();
            if !line.is_empty() {
                println!("{line}");
            }
        }
    }
}

pub fn find_rollout_file_for_thread(root: PathBuf, thread_id: &str) -> Option<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.contains(thread_id))
                .unwrap_or(false)
        })
}

fn find_recent_rollout_file_for_context_read(
    thread_id: &str,
    turn_id: Option<&str>,
) -> Option<PathBuf> {
    let root = resolve_sessions_root();
    let mut candidates: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .collect();
    candidates.sort_by_key(|path| fs::metadata(path).and_then(|stat| stat.modified()).ok());
    candidates.reverse();

    if let Some(path) = candidates
        .iter()
        .find(|path| rollout_matches_thread(path, thread_id, turn_id))
    {
        return Some(path.clone());
    }

    candidates.into_iter().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.contains(thread_id))
            .unwrap_or(false)
    })
}

fn rollout_matches_thread(path: &Path, thread_id: &str, turn_id: Option<&str>) -> bool {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return false,
    };
    if let Some(turn_id) = turn_id {
        if !raw.contains(turn_id) {
            return false;
        }
    }
    raw.contains(thread_id)
}

fn extract_context_usage_from_rollout_line(raw_line: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(raw_line.trim()).ok()?;
    let payload = parsed.get("payload")?;
    let info = payload.get("info")?;
    let usage_root = info
        .get("last_token_usage")
        .or_else(|| info.get("lastTokenUsage"))
        .or_else(|| info.get("total_token_usage"))
        .or_else(|| info.get("totalTokenUsage"))?;
    let token_limit = info
        .get("model_context_window")
        .or_else(|| info.get("modelContextWindow"))
        .and_then(Value::as_u64)?;
    let tokens_used = usage_root
        .get("total_tokens")
        .or_else(|| usage_root.get("totalTokens"))
        .and_then(Value::as_u64)?;
    Some(json!({
        "tokensUsed": tokens_used,
        "tokenLimit": token_limit,
    }))
}
