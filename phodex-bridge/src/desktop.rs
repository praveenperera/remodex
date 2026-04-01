use color_eyre::eyre::{eyre, Result};
use serde_json::{json, Value};

pub async fn handle_desktop_request(method: &str, params: &Value) -> Result<Option<Value>> {
    if method != "desktop/continueOnMac" {
        return Ok(None);
    }

    let thread_id = params
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| eyre!("A thread id is required to continue on Mac."))?;

    let target_url = format!("codex://threads/{thread_id}");
    let status = std::process::Command::new("open")
        .args(["-b", "com.openai.codex", &target_url])
        .status()?;
    if !status.success() {
        return Err(eyre!("Could not open Codex.app on this Mac."));
    }

    Ok(Some(json!({
        "success": true,
        "relaunched": false,
        "targetUrl": target_url,
        "threadId": thread_id,
        "desktopKnown": true,
    })))
}
