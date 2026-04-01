use color_eyre::eyre::Result;
use serde_json::{json, Value};

pub async fn handle_notifications_request(method: &str, params: &Value) -> Result<Option<Value>> {
    if method != "notifications/push/register" {
        return Ok(None);
    }

    Ok(Some(json!({
        "ok": true,
        "alertsEnabled": params.get("alertsEnabled").and_then(Value::as_bool).unwrap_or(false),
        "apnsEnvironment": if params.get("appEnvironment").and_then(Value::as_str) == Some("development") {
            "development"
        } else {
            "production"
        }
    })))
}
