use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::UnboundedSender;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::secure_device_state::{
    get_trusted_phone_public_key, remember_trusted_phone, BridgeDeviceState,
};

pub const PAIRING_QR_VERSION: u32 = 2;
pub const SECURE_PROTOCOL_VERSION: u32 = 1;

const HANDSHAKE_TAG: &str = "remodex-e2ee-v1";
const HANDSHAKE_MODE_QR_BOOTSTRAP: &str = "qr_bootstrap";
const HANDSHAKE_MODE_TRUSTED_RECONNECT: &str = "trusted_reconnect";
const SECURE_SENDER_MAC: &str = "mac";
const SECURE_SENDER_IPHONE: &str = "iphone";
const MAX_PAIRING_AGE_MS: i128 = 5 * 60 * 1000;
const MAX_BRIDGE_OUTBOUND_MESSAGES: usize = 500;
const MAX_BRIDGE_OUTBOUND_BYTES: usize = 10 * 1024 * 1024;

pub struct BridgeSecureTransport {
    session_id: String,
    relay_url: String,
    current_device_state: BridgeDeviceState,
    pending_handshake: Option<PendingHandshake>,
    active_session: Option<ActiveSession>,
    live_send_wire_message: Option<UnboundedSender<String>>,
    last_relayed_bridge_outbound_seq: u64,
    current_pairing_expires_at: i128,
    next_key_epoch: u64,
    next_bridge_outbound_seq: u64,
    outbound_buffer_bytes: usize,
    outbound_buffer: Vec<OutboundBufferEntry>,
}

pub struct IncomingWireResult {
    pub trusted_phone_updated: bool,
}

struct PendingHandshake {
    session_id: String,
    handshake_mode: String,
    key_epoch: u64,
    phone_device_id: String,
    phone_identity_public_key: String,
    phone_ephemeral_public_key: String,
    mac_ephemeral_private_key: String,
    transcript_bytes: Vec<u8>,
}

struct TranscriptInput<'a> {
    session_id: &'a str,
    protocol_version: u64,
    handshake_mode: &'a str,
    key_epoch: u64,
    mac_device_id: &'a str,
    phone_device_id: &'a str,
    mac_identity_public_key: &'a str,
    phone_identity_public_key: &'a str,
    mac_ephemeral_public_key: &'a str,
    phone_ephemeral_public_key: &'a str,
    client_nonce: &'a [u8],
    server_nonce: &'a [u8],
    expires_at_for_transcript: i128,
}

struct ActiveSession {
    key_epoch: u64,
    phone_to_mac_key: [u8; 32],
    mac_to_phone_key: [u8; 32],
    last_inbound_counter: i64,
    next_outbound_counter: u64,
    is_resumed: bool,
}

struct OutboundBufferEntry {
    bridge_outbound_seq: u64,
    payload_text: String,
    size_bytes: usize,
}

impl BridgeSecureTransport {
    pub fn new(session_id: String, relay_url: String, device_state: BridgeDeviceState) -> Self {
        Self {
            session_id,
            relay_url,
            current_device_state: device_state,
            pending_handshake: None,
            active_session: None,
            live_send_wire_message: None,
            last_relayed_bridge_outbound_seq: 0,
            current_pairing_expires_at: current_time_ms() + MAX_PAIRING_AGE_MS,
            next_key_epoch: 1,
            next_bridge_outbound_seq: 1,
            outbound_buffer_bytes: 0,
            outbound_buffer: Vec::new(),
        }
    }

    pub fn create_pairing_payload(&mut self) -> Value {
        self.current_pairing_expires_at = current_time_ms() + MAX_PAIRING_AGE_MS;
        json!({
            "v": PAIRING_QR_VERSION,
            "relay": self.relay_url,
            "sessionId": self.session_id,
            "macDeviceId": self.current_device_state.mac_device_id,
            "macIdentityPublicKey": self.current_device_state.mac_identity_public_key,
            "expiresAt": self.current_pairing_expires_at,
        })
    }

    pub fn bind_live_send_wire_message(&mut self, sender: UnboundedSender<String>) {
        self.live_send_wire_message = Some(sender);
        self.replay_buffered_outbound_messages();
    }

    pub fn current_device_state(&self) -> &BridgeDeviceState {
        &self.current_device_state
    }

    pub fn is_secure_channel_ready(&self) -> bool {
        self.active_session
            .as_ref()
            .map(|session| session.is_resumed)
            .unwrap_or(false)
    }

    pub fn queue_outbound_application_message(&mut self, payload_text: String) {
        if payload_text.trim().is_empty() {
            return;
        }

        let entry = OutboundBufferEntry {
            bridge_outbound_seq: self.next_bridge_outbound_seq,
            size_bytes: payload_text.len(),
            payload_text,
        };
        self.next_bridge_outbound_seq += 1;
        self.outbound_buffer_bytes += entry.size_bytes;
        self.outbound_buffer.push(entry);
        self.trim_outbound_buffer();

        if self
            .active_session
            .as_ref()
            .map(|session| session.is_resumed)
            .unwrap_or(false)
        {
            let entry_index = self.outbound_buffer.len().saturating_sub(1);
            self.send_buffered_entry_by_index(entry_index);
        }
    }

    pub fn handle_incoming_wire_message<F>(
        &mut self,
        raw_message: &str,
        mut on_application_message: F,
    ) -> IncomingWireResult
    where
        F: FnMut(String),
    {
        let Some(parsed) = serde_json::from_str::<Value>(raw_message).ok() else {
            return IncomingWireResult {
                trusted_phone_updated: false,
            };
        };

        let kind = parsed
            .get("kind")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if kind.is_empty() {
            if parsed.get("method").is_some() || parsed.get("id").is_some() {
                self.send_control_message(create_secure_error(
                    "update_required",
                    "This bridge requires the latest Remodex iPhone app for secure pairing.",
                ));
                return IncomingWireResult {
                    trusted_phone_updated: false,
                };
            }
            return IncomingWireResult {
                trusted_phone_updated: false,
            };
        }

        match kind {
            "clientHello" => {
                self.handle_client_hello(&parsed);
                IncomingWireResult {
                    trusted_phone_updated: false,
                }
            }
            "clientAuth" => IncomingWireResult {
                trusted_phone_updated: self.handle_client_auth(&parsed),
            },
            "resumeState" => {
                self.handle_resume_state(&parsed);
                IncomingWireResult {
                    trusted_phone_updated: false,
                }
            }
            "encryptedEnvelope" => {
                self.handle_encrypted_envelope(&parsed, &mut on_application_message);
                IncomingWireResult {
                    trusted_phone_updated: false,
                }
            }
            _ => IncomingWireResult {
                trusted_phone_updated: false,
            },
        }
    }

    fn handle_client_hello(&mut self, message: &Value) {
        let protocol_version = message
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let incoming_session_id = read_string(message.get("sessionId"));
        let handshake_mode = read_string(message.get("handshakeMode"));
        let phone_device_id = read_string(message.get("phoneDeviceId"));
        let phone_identity_public_key = read_string(message.get("phoneIdentityPublicKey"));
        let phone_ephemeral_public_key = read_string(message.get("phoneEphemeralPublicKey"));
        let client_nonce_base64 = read_string(message.get("clientNonce"));

        if protocol_version != SECURE_PROTOCOL_VERSION as u64
            || incoming_session_id != self.session_id
        {
            self.send_control_message(create_secure_error(
                "update_required",
                "The bridge and iPhone are not using the same secure transport version.",
            ));
            return;
        }

        if phone_device_id.is_empty()
            || phone_identity_public_key.is_empty()
            || phone_ephemeral_public_key.is_empty()
            || client_nonce_base64.is_empty()
        {
            self.send_control_message(create_secure_error(
                "invalid_client_hello",
                "The iPhone handshake is missing required secure fields.",
            ));
            return;
        }

        if handshake_mode != HANDSHAKE_MODE_QR_BOOTSTRAP
            && handshake_mode != HANDSHAKE_MODE_TRUSTED_RECONNECT
        {
            self.send_control_message(create_secure_error(
                "invalid_handshake_mode",
                "The iPhone requested an unknown secure pairing mode.",
            ));
            return;
        }

        if handshake_mode == HANDSHAKE_MODE_QR_BOOTSTRAP
            && current_time_ms() > self.current_pairing_expires_at
        {
            self.send_control_message(create_secure_error(
                "pairing_expired",
                "The pairing QR code has expired. Generate a new QR code from the bridge.",
            ));
            return;
        }

        let trusted_phone_public_key =
            get_trusted_phone_public_key(&self.current_device_state, &phone_device_id);
        if handshake_mode == HANDSHAKE_MODE_TRUSTED_RECONNECT {
            if trusted_phone_public_key.is_none() {
                self.send_control_message(create_secure_error(
                    "phone_not_trusted",
                    "This iPhone is not trusted by the current bridge session. Scan a fresh QR code to pair again.",
                ));
                return;
            }
            if trusted_phone_public_key.as_deref() != Some(phone_identity_public_key.as_str()) {
                self.send_control_message(create_secure_error(
                    "phone_identity_changed",
                    "The trusted iPhone identity does not match this reconnect attempt.",
                ));
                return;
            }
        }

        let Some(client_nonce) = decode_base64(&client_nonce_base64) else {
            self.send_control_message(create_secure_error(
                "invalid_client_nonce",
                "The iPhone secure nonce could not be decoded.",
            ));
            return;
        };

        let (ephemeral_private, ephemeral_public) = generate_ephemeral_keypair();
        let mut server_nonce = [0_u8; 32];
        let _ = getrandom::fill(&mut server_nonce);
        let key_epoch = self.next_key_epoch;
        let expires_at_for_transcript = if handshake_mode == HANDSHAKE_MODE_QR_BOOTSTRAP {
            self.current_pairing_expires_at
        } else {
            0
        };
        let mac_ephemeral_public_key = BASE64.encode(ephemeral_public.to_bytes());
        let transcript_bytes = build_transcript_bytes(&TranscriptInput {
            session_id: &self.session_id,
            protocol_version: SECURE_PROTOCOL_VERSION as u64,
            handshake_mode: &handshake_mode,
            key_epoch,
            mac_device_id: &self.current_device_state.mac_device_id,
            phone_device_id: &phone_device_id,
            mac_identity_public_key: &self.current_device_state.mac_identity_public_key,
            phone_identity_public_key: &phone_identity_public_key,
            mac_ephemeral_public_key: &mac_ephemeral_public_key,
            phone_ephemeral_public_key: &phone_ephemeral_public_key,
            client_nonce: &client_nonce,
            server_nonce: &server_nonce,
            expires_at_for_transcript,
        });

        let mac_signature = sign_transcript(
            &self.current_device_state.mac_identity_private_key,
            &transcript_bytes,
        );

        self.pending_handshake = Some(PendingHandshake {
            session_id: self.session_id.clone(),
            handshake_mode: handshake_mode.clone(),
            key_epoch,
            phone_device_id: phone_device_id.clone(),
            phone_identity_public_key: phone_identity_public_key.clone(),
            phone_ephemeral_public_key: phone_ephemeral_public_key.clone(),
            mac_ephemeral_private_key: BASE64.encode(ephemeral_private.to_bytes()),
            transcript_bytes: transcript_bytes.clone(),
        });
        self.active_session = None;

        self.send_control_message(json!({
            "kind": "serverHello",
            "protocolVersion": SECURE_PROTOCOL_VERSION,
            "sessionId": self.session_id,
            "handshakeMode": handshake_mode,
            "macDeviceId": self.current_device_state.mac_device_id,
            "macIdentityPublicKey": self.current_device_state.mac_identity_public_key,
            "macEphemeralPublicKey": mac_ephemeral_public_key,
            "serverNonce": BASE64.encode(server_nonce),
            "keyEpoch": key_epoch,
            "expiresAtForTranscript": expires_at_for_transcript,
            "macSignature": mac_signature,
            "clientNonce": client_nonce_base64,
        }));
    }

    fn handle_client_auth(&mut self, message: &Value) -> bool {
        let Some(pending_handshake) = self.pending_handshake.take() else {
            self.send_control_message(create_secure_error(
                "unexpected_client_auth",
                "The bridge did not have a pending secure handshake to finalize.",
            ));
            return false;
        };

        let incoming_session_id = read_string(message.get("sessionId"));
        let phone_device_id = read_string(message.get("phoneDeviceId"));
        let key_epoch = message
            .get("keyEpoch")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let phone_signature = read_string(message.get("phoneSignature"));

        if incoming_session_id != pending_handshake.session_id
            || phone_device_id != pending_handshake.phone_device_id
            || key_epoch != pending_handshake.key_epoch
            || phone_signature.is_empty()
        {
            self.send_control_message(create_secure_error(
                "invalid_client_auth",
                "The secure client authentication payload was invalid.",
            ));
            return false;
        }

        let mut client_auth_transcript = pending_handshake.transcript_bytes.clone();
        client_auth_transcript.extend(encode_length_prefixed_utf8("client-auth"));
        let phone_verified = verify_transcript(
            &pending_handshake.phone_identity_public_key,
            &client_auth_transcript,
            &phone_signature,
        );
        if !phone_verified {
            self.send_control_message(create_secure_error(
                "invalid_phone_signature",
                "The iPhone secure signature could not be verified.",
            ));
            return false;
        }

        let Some(shared_secret) = diffie_hellman(
            &pending_handshake.mac_ephemeral_private_key,
            &pending_handshake.phone_ephemeral_public_key,
        ) else {
            self.send_control_message(create_secure_error(
                "invalid_client_auth",
                "The secure client authentication payload was invalid.",
            ));
            return false;
        };
        let salt = Sha256::digest(&pending_handshake.transcript_bytes);
        let info_prefix = format!(
            "{HANDSHAKE_TAG}|{}|{}|{}|{}",
            pending_handshake.session_id,
            self.current_device_state.mac_device_id,
            pending_handshake.phone_device_id,
            pending_handshake.key_epoch
        );

        self.active_session = Some(ActiveSession {
            key_epoch: pending_handshake.key_epoch,
            phone_to_mac_key: derive_aes_key(
                &shared_secret,
                &salt,
                &format!("{info_prefix}|phoneToMac"),
            ),
            mac_to_phone_key: derive_aes_key(
                &shared_secret,
                &salt,
                &format!("{info_prefix}|macToPhone"),
            ),
            last_inbound_counter: -1,
            next_outbound_counter: 0,
            is_resumed: false,
        });
        self.next_key_epoch = pending_handshake.key_epoch + 1;

        let previous_trusted_phone_public_key = get_trusted_phone_public_key(
            &self.current_device_state,
            &pending_handshake.phone_device_id,
        );
        let mut trusted_phone_updated = false;
        if pending_handshake.handshake_mode == HANDSHAKE_MODE_QR_BOOTSTRAP
            || previous_trusted_phone_public_key.is_some()
        {
            if let Ok(updated_state) = remember_trusted_phone(
                &self.current_device_state,
                &pending_handshake.phone_device_id,
                &pending_handshake.phone_identity_public_key,
            ) {
                trusted_phone_updated = previous_trusted_phone_public_key.as_deref()
                    != Some(pending_handshake.phone_identity_public_key.as_str());
                self.current_device_state = updated_state;
            }
        }
        if pending_handshake.handshake_mode == HANDSHAKE_MODE_QR_BOOTSTRAP {
            self.reset_outbound_replay_state();
        }

        self.send_control_message(json!({
            "kind": "secureReady",
            "sessionId": self.session_id,
            "keyEpoch": pending_handshake.key_epoch,
            "macDeviceId": self.current_device_state.mac_device_id,
        }));
        trusted_phone_updated
    }

    fn handle_resume_state(&mut self, message: &Value) {
        let Some(active_session) = self.active_session.as_mut() else {
            return;
        };
        let incoming_session_id = read_string(message.get("sessionId"));
        let key_epoch = message
            .get("keyEpoch")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        if incoming_session_id != self.session_id || key_epoch != active_session.key_epoch {
            return;
        }
        let last_applied_bridge_outbound_seq = message
            .get("lastAppliedBridgeOutboundSeq")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        self.last_relayed_bridge_outbound_seq = last_applied_bridge_outbound_seq;
        active_session.is_resumed = true;
        self.replay_buffered_outbound_messages();
    }

    fn handle_encrypted_envelope<F>(&mut self, message: &Value, on_application_message: &mut F)
    where
        F: FnMut(String),
    {
        let Some(active_session) = self.active_session.as_mut() else {
            self.send_control_message(create_secure_error(
                "secure_channel_unavailable",
                "The secure channel is not ready yet on the bridge.",
            ));
            return;
        };

        let incoming_session_id = read_string(message.get("sessionId"));
        let key_epoch = message
            .get("keyEpoch")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let sender = read_string(message.get("sender"));
        let counter = message.get("counter").and_then(Value::as_i64).unwrap_or(-1);
        if incoming_session_id != self.session_id
            || key_epoch != active_session.key_epoch
            || sender != SECURE_SENDER_IPHONE
            || counter <= active_session.last_inbound_counter
        {
            self.send_control_message(create_secure_error(
                "invalid_envelope",
                "The bridge rejected an invalid or replayed secure envelope.",
            ));
            return;
        }

        let ciphertext = read_string(message.get("ciphertext"));
        let tag = read_string(message.get("tag"));
        let Some(plaintext_buffer) = decrypt_payload(
            &ciphertext,
            &tag,
            &active_session.phone_to_mac_key,
            &nonce_for_direction(SECURE_SENDER_IPHONE, counter as u64),
        ) else {
            self.send_control_message(create_secure_error(
                "decrypt_failed",
                "The bridge could not decrypt the iPhone secure payload.",
            ));
            return;
        };

        active_session.last_inbound_counter = counter;
        let payload_object = serde_json::from_slice::<Value>(&plaintext_buffer).ok();
        let payload_text = payload_object
            .as_ref()
            .and_then(|value| value.get("payloadText"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if payload_text.is_empty() {
            self.send_control_message(create_secure_error(
                "invalid_payload",
                "The secure payload did not contain a usable application message.",
            ));
            return;
        }

        on_application_message(payload_text.to_owned());
    }

    fn trim_outbound_buffer(&mut self) {
        while self.outbound_buffer.len() > MAX_BRIDGE_OUTBOUND_MESSAGES
            || self.outbound_buffer_bytes > MAX_BRIDGE_OUTBOUND_BYTES
        {
            if let Some(removed) = self.outbound_buffer.first() {
                self.outbound_buffer_bytes = self
                    .outbound_buffer_bytes
                    .saturating_sub(removed.size_bytes);
            }
            if !self.outbound_buffer.is_empty() {
                self.outbound_buffer.remove(0);
            }
        }
    }

    fn reset_outbound_replay_state(&mut self) {
        self.outbound_buffer.clear();
        self.outbound_buffer_bytes = 0;
        self.last_relayed_bridge_outbound_seq = 0;
        self.next_bridge_outbound_seq = 1;
    }

    fn send_buffered_entry_by_index(&mut self, entry_index: usize) {
        let Some(sender) = self.live_send_wire_message.as_ref() else {
            return;
        };
        let Some(active_session) = self.active_session.as_mut() else {
            return;
        };
        if !active_session.is_resumed {
            return;
        }
        let Some(entry) = self.outbound_buffer.get(entry_index) else {
            return;
        };

        let envelope = json!({
            "kind": "encryptedEnvelope",
            "v": SECURE_PROTOCOL_VERSION,
            "sessionId": self.session_id,
            "keyEpoch": active_session.key_epoch,
            "sender": SECURE_SENDER_MAC,
            "counter": active_session.next_outbound_counter,
        });
        let payload = json!({
            "bridgeOutboundSeq": entry.bridge_outbound_seq,
            "payloadText": entry.payload_text,
        });
        let nonce = nonce_for_direction(SECURE_SENDER_MAC, active_session.next_outbound_counter);
        let Some((ciphertext, tag)) =
            encrypt_payload(&payload, &active_session.mac_to_phone_key, &nonce)
        else {
            return;
        };
        active_session.next_outbound_counter += 1;

        let mut envelope = envelope;
        if let Some(object) = envelope.as_object_mut() {
            object.insert("ciphertext".to_owned(), Value::String(ciphertext));
            object.insert("tag".to_owned(), Value::String(tag));
        }
        let _ = sender.send(envelope.to_string());
    }

    fn replay_buffered_outbound_messages(&mut self) {
        if !self.is_secure_channel_ready() {
            return;
        }

        let replayable = self
            .outbound_buffer
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.bridge_outbound_seq > self.last_relayed_bridge_outbound_seq)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        for index in replayable {
            self.send_buffered_entry_by_index(index);
        }
    }

    fn send_control_message(&self, value: Value) {
        if let Some(sender) = &self.live_send_wire_message {
            let _ = sender.send(value.to_string());
        }
    }
}

fn create_secure_error(code: &str, message: &str) -> Value {
    json!({
        "kind": "secureError",
        "code": code,
        "message": message,
    })
}

fn build_transcript_bytes(input: &TranscriptInput<'_>) -> Vec<u8> {
    [
        encode_length_prefixed_utf8(HANDSHAKE_TAG),
        encode_length_prefixed_utf8(input.session_id),
        encode_length_prefixed_utf8(input.protocol_version.to_string()),
        encode_length_prefixed_utf8(input.handshake_mode),
        encode_length_prefixed_utf8(input.key_epoch.to_string()),
        encode_length_prefixed_utf8(input.mac_device_id),
        encode_length_prefixed_utf8(input.phone_device_id),
        encode_length_prefixed_buffer(
            &decode_base64(input.mac_identity_public_key).unwrap_or_default(),
        ),
        encode_length_prefixed_buffer(
            &decode_base64(input.phone_identity_public_key).unwrap_or_default(),
        ),
        encode_length_prefixed_buffer(
            &decode_base64(input.mac_ephemeral_public_key).unwrap_or_default(),
        ),
        encode_length_prefixed_buffer(
            &decode_base64(input.phone_ephemeral_public_key).unwrap_or_default(),
        ),
        encode_length_prefixed_buffer(input.client_nonce),
        encode_length_prefixed_buffer(input.server_nonce),
        encode_length_prefixed_utf8(input.expires_at_for_transcript.to_string()),
    ]
    .concat()
}

fn encode_length_prefixed_utf8(value: impl ToString) -> Vec<u8> {
    encode_length_prefixed_buffer(value.to_string().as_bytes())
}

fn encode_length_prefixed_buffer(buffer: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(4 + buffer.len());
    output.extend((buffer.len() as u32).to_be_bytes());
    output.extend(buffer);
    output
}

fn sign_transcript(private_key_base64: &str, transcript_bytes: &[u8]) -> String {
    let private_key = decode_base64(private_key_base64).unwrap_or_default();
    let signing_key =
        SigningKey::from_bytes(private_key.as_slice().try_into().unwrap_or(&[0_u8; 32]));
    BASE64.encode(signing_key.sign(transcript_bytes).to_bytes())
}

fn verify_transcript(
    public_key_base64: &str,
    transcript_bytes: &[u8],
    signature_base64: &str,
) -> bool {
    let public_key = match decode_base64(public_key_base64) {
        Some(bytes) => bytes,
        None => return false,
    };
    let signature = match decode_base64(signature_base64) {
        Some(bytes) => bytes,
        None => return false,
    };
    let verifying_key =
        match VerifyingKey::from_bytes(public_key.as_slice().try_into().unwrap_or(&[0_u8; 32])) {
            Ok(key) => key,
            Err(_) => return false,
        };
    let signature = match Signature::from_slice(&signature) {
        Ok(signature) => signature,
        Err(_) => return false,
    };
    verifying_key.verify(transcript_bytes, &signature).is_ok()
}

fn diffie_hellman(private_key_base64: &str, public_key_base64: &str) -> Option<Vec<u8>> {
    let private_key = decode_base64(private_key_base64)?;
    let public_key = decode_base64(public_key_base64)?;
    let private = StaticSecret::from(to_32_bytes(&private_key)?);
    let public = X25519PublicKey::from(to_32_bytes(&public_key)?);
    Some(private.diffie_hellman(&public).to_bytes().to_vec())
}

fn generate_ephemeral_keypair() -> (StaticSecret, X25519PublicKey) {
    let mut private_bytes = [0_u8; 32];
    let _ = getrandom::fill(&mut private_bytes);
    let private = StaticSecret::from(private_bytes);
    let public = X25519PublicKey::from(&private);
    (private, public)
}

fn derive_aes_key(shared_secret: &[u8], salt: &[u8], info_label: &str) -> [u8; 32] {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), shared_secret);
    let mut key = [0_u8; 32];
    let _ = hkdf.expand(info_label.as_bytes(), &mut key);
    key
}

fn encrypt_payload(
    payload: &Value,
    key: &[u8; 32],
    nonce_seed: &[u8; 12],
) -> Option<(String, String)> {
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    let nonce = Nonce::from_slice(nonce_seed);
    let ciphertext = cipher.encrypt(nonce, payload.to_string().as_bytes()).ok()?;
    if ciphertext.len() < 16 {
        return None;
    }
    let split = ciphertext.len() - 16;
    Some((
        BASE64.encode(&ciphertext[..split]),
        BASE64.encode(&ciphertext[split..]),
    ))
}

fn decrypt_payload(
    ciphertext: &str,
    tag: &str,
    key: &[u8; 32],
    nonce_seed: &[u8; 12],
) -> Option<Vec<u8>> {
    let mut combined = decode_base64(ciphertext)?;
    combined.extend(decode_base64(tag)?);
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    let nonce = Nonce::from_slice(nonce_seed);
    cipher.decrypt(nonce, combined.as_ref()).ok()
}

pub fn nonce_for_direction(sender: &str, counter: u64) -> [u8; 12] {
    let mut nonce = [0_u8; 12];
    nonce[0] = if sender == SECURE_SENDER_MAC { 1 } else { 2 };
    let mut value = counter;
    for index in (1..12).rev() {
        nonce[index] = (value & 0xff) as u8;
        value >>= 8;
    }
    nonce
}

fn read_string(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_default()
}

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    BASE64.decode(value).ok()
}

fn to_32_bytes(value: &[u8]) -> Option<[u8; 32]> {
    value.try_into().ok()
}

fn current_time_ms() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i128)
        .unwrap_or_default()
}
