use color_eyre::eyre::Result;
use qrcode::render::unicode;
use qrcode::QrCode;
use serde_json::Value;

pub fn print_qr(pairing_payload: &Value) -> Result<()> {
    let payload = pairing_payload.to_string();
    let qr = QrCode::new(payload.as_bytes())?;
    let rendered = qr.render::<unicode::Dense1x2>().build();

    println!("\nScan this QR with the iPhone:\n");
    println!("{rendered}");

    if let Some(session_id) = pairing_payload.get("sessionId").and_then(Value::as_str) {
        println!("\nSession ID: {session_id}");
    }
    if let Some(device_id) = pairing_payload.get("macDeviceId").and_then(Value::as_str) {
        println!("Device ID: {device_id}");
    }
    if let Some(expires_at) = pairing_payload.get("expiresAt") {
        println!("Expires: {expires_at}\n");
    }

    Ok(())
}
