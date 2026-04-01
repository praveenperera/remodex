use serde_json::{json, Value};

pub fn success_response(id: Value, result: Value) -> String {
    json!({
        "id": id,
        "result": result,
    })
    .to_string()
}

pub fn error_response(id: Value, code: &str, message: impl Into<String>) -> String {
    json!({
        "id": id,
        "error": {
            "code": -32000,
            "message": message.into(),
            "data": {
                "errorCode": code,
            }
        }
    })
    .to_string()
}
