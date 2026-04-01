use base64::Engine;
use color_eyre::eyre::{eyre, Result};
use reqwest::multipart::{Form, Part};
use serde_json::{json, Value};

const OPENAI_TRANSCRIPTIONS_URL: &str = "https://api.openai.com/v1/audio/transcriptions";
const CHATGPT_TRANSCRIPTIONS_URL: &str = "https://chatgpt.com/backend-api/transcribe";
const DEFAULT_TRANSCRIPTION_MODEL: &str = "gpt-4o-mini-transcribe";
const MAX_AUDIO_BYTES: usize = 10 * 1024 * 1024;
const MAX_DURATION_MS: u64 = 60_000;

pub async fn handle_voice_request<F>(
    method: &str,
    params: &Value,
    mut resolve_auth_status: F,
) -> Result<Option<Value>>
where
    F: FnMut() -> futures_util::future::BoxFuture<'static, Result<Value>>,
{
    match method {
        "voice/transcribe" => transcribe_voice(params, &mut resolve_auth_status)
            .await
            .map(Some),
        "voice/resolveAuth" => resolve_voice_auth(&mut resolve_auth_status).await.map(Some),
        _ => Ok(None),
    }
}

async fn transcribe_voice<F>(params: &Value, resolve_auth_status: &mut F) -> Result<Value>
where
    F: FnMut() -> futures_util::future::BoxFuture<'static, Result<Value>>,
{
    let mime_type = params.get("mimeType").and_then(Value::as_str).unwrap_or("");
    if mime_type != "audio/wav" {
        return Err(eyre!(
            "Only WAV audio is supported for voice transcription."
        ));
    }
    let sample_rate_hz = params
        .get("sampleRateHz")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if sample_rate_hz != 24_000 {
        return Err(eyre!("Voice transcription requires 24 kHz mono WAV audio."));
    }
    let duration_ms = params
        .get("durationMs")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if duration_ms == 0 {
        return Err(eyre!("Voice messages must include a positive duration."));
    }
    if duration_ms > MAX_DURATION_MS {
        return Err(eyre!("Voice messages are limited to 60 seconds."));
    }
    let audio_base64 = params
        .get("audioBase64")
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace(char::is_whitespace, "");
    if audio_base64.is_empty() {
        return Err(eyre!("The voice request did not include any audio."));
    }
    let audio_buffer = base64::engine::general_purpose::STANDARD
        .decode(audio_base64)
        .map_err(|_| eyre!("The recorded audio could not be decoded."))?;
    if audio_buffer.len() > MAX_AUDIO_BYTES {
        return Err(eyre!("Voice messages are limited to 10 MB."));
    }
    if audio_buffer.len() < 44 || &audio_buffer[0..4] != b"RIFF" || &audio_buffer[8..12] != b"WAVE"
    {
        return Err(eyre!("The recorded audio is not a valid WAV file."));
    }

    let auth_status = resolve_auth_status().await?;
    let auth_method = auth_status
        .get("authMethod")
        .and_then(Value::as_str)
        .unwrap_or("");
    let token = auth_status
        .get("authToken")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| eyre!("Sign in with ChatGPT before using voice transcription."))?;
    let is_chatgpt = matches!(auth_method, "chatgpt" | "chatgptAuthTokens");
    let url = if is_chatgpt {
        CHATGPT_TRANSCRIPTIONS_URL
    } else {
        OPENAI_TRANSCRIPTIONS_URL
    };

    let mut form = Form::new().part(
        "file",
        Part::bytes(audio_buffer)
            .mime_str(mime_type)?
            .file_name("voice.wav"),
    );
    if !is_chatgpt {
        form = form.text("model", DEFAULT_TRANSCRIPTION_MODEL.to_owned());
    }

    let client = reqwest::Client::new();
    let response = client
        .post(url)
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(eyre!(
            "Transcription failed with status {}.",
            response.status()
        ));
    }

    let payload: Value = response.json().await?;
    let text = payload
        .get("text")
        .or_else(|| payload.get("transcript"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| eyre!("The transcription response did not include any text."))?;

    Ok(json!({ "text": text }))
}

async fn resolve_voice_auth<F>(resolve_auth_status: &mut F) -> Result<Value>
where
    F: FnMut() -> futures_util::future::BoxFuture<'static, Result<Value>>,
{
    let auth_status = resolve_auth_status().await?;
    let auth_method = auth_status
        .get("authMethod")
        .and_then(Value::as_str)
        .unwrap_or("");
    let token = auth_status
        .get("authToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    let is_chatgpt = matches!(auth_method, "chatgpt" | "chatgptAuthTokens");

    if is_chatgpt && !token.trim().is_empty() {
        return Ok(json!({ "token": token }));
    }
    if token.trim().is_empty() {
        return Err(eyre!(
            "No ChatGPT session token available. Sign in to ChatGPT on the Mac."
        ));
    }
    Err(eyre!("Voice transcription requires a ChatGPT account."))
}
