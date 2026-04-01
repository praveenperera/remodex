use std::fs;
use std::process::Command;

use color_eyre::eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub source: Option<String>,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

const DEFAULT_BUNDLE_ID: &str = "com.openai.codex";

fn state_file() -> std::path::PathBuf {
    crate::daemon_state::resolve_remodex_state_dir().join("last-thread.json")
}

pub fn remember_active_thread(thread_id: &str, source: &str) -> Result<()> {
    if thread_id.trim().is_empty() {
        return Ok(());
    }

    fs::create_dir_all(crate::daemon_state::resolve_remodex_state_dir())?;
    fs::write(
        state_file(),
        serde_json::to_string_pretty(&SessionState {
            thread_id: thread_id.to_owned(),
            source: Some(source.to_owned()),
            updated_at: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_default(),
        })?,
    )?;
    Ok(())
}

pub fn open_last_active_thread(bundle_id: Option<&str>) -> Result<SessionState> {
    let state = read_last_active_thread()
        .ok_or_else(|| eyre!("No remembered Remodex thread found yet."))?;
    let status = Command::new("open")
        .args([
            "-b",
            bundle_id.unwrap_or(DEFAULT_BUNDLE_ID),
            &format!("codex://threads/{}", state.thread_id),
        ])
        .status()?;
    if !status.success() {
        return Err(eyre!(
            "Failed to open the last active Remodex thread on Mac."
        ));
    }
    Ok(state)
}

pub fn read_last_active_thread() -> Option<SessionState> {
    let raw = fs::read_to_string(state_file()).ok()?;
    serde_json::from_str(&raw).ok()
}
