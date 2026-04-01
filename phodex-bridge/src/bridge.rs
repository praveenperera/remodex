use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Context, Result};
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

use crate::account_status::compose_sanitized_auth_status;
use crate::codex_transport::{CodexEvent, CodexTransport};
use crate::config::{read_bridge_config, BridgeConfig};
use crate::daemon_state::{clear_bridge_status, write_bridge_status, write_pairing_session};
use crate::desktop::handle_desktop_request;
use crate::git_handler::handle_git_request;
use crate::json_rpc::{error_response, success_response};
use crate::package_version_status::BridgePackageVersionStatusReader;
use crate::push::handle_notifications_request;
use crate::qr::print_qr;
use crate::rollout::thread_context_read;
use crate::secure_device_state::{
    load_or_create_bridge_device_state, resolve_bridge_relay_session, BridgeDeviceState,
};
use crate::secure_transport::BridgeSecureTransport;
use crate::session_state::remember_active_thread;
use crate::voice::handle_voice_request;
use crate::workspace::handle_workspace_request;

const RELAY_WATCHDOG_PING_INTERVAL_MS: u64 = 10_000;
const RELAY_WATCHDOG_STALE_AFTER_MS: u64 = 25_000;
const BRIDGE_STATUS_HEARTBEAT_INTERVAL_MS: u64 = 5_000;
const FORWARDED_REQUEST_METHOD_TTL_MS: u64 = 2 * 60_000;
const RELAY_HISTORY_IMAGE_REFERENCE_URL: &str = "remodex://history-image-elided";
const STALE_RELAY_STATUS_MESSAGE: &str = "Relay heartbeat stalled; reconnect pending.";

type CodexWaiters = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

#[derive(Clone, Debug)]
pub struct StartBridgeOptions {
    pub config: Option<BridgeConfig>,
    pub print_pairing_qr: bool,
}

impl Default for StartBridgeOptions {
    fn default() -> Self {
        Self {
            config: None,
            print_pairing_qr: true,
        }
    }
}

#[derive(Clone)]
struct CodexRequestClient {
    transport: CodexTransport,
    waiters: CodexWaiters,
}

#[derive(Clone, Default)]
struct PendingAuthLogin {
    login_id: Option<String>,
    auth_url: Option<String>,
    request_id: Option<String>,
    started_at: Option<Instant>,
}

#[derive(Clone)]
struct TrackedRequest {
    method: String,
    created_at: Instant,
}

#[derive(Clone)]
struct BridgeStatusSnapshot {
    state: String,
    connection_status: String,
    last_error: String,
}

struct ContextUsageWatch {
    key: String,
    thread_id: String,
    turn_id: Option<String>,
    started_at: Instant,
    last_usage_json: Option<String>,
}

enum RelayCommand {
    Text(String),
    Ping,
    Close,
}

struct RelayConnection {
    reader: futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    command_tx: UnboundedSender<RelayCommand>,
    wire_text_tx: UnboundedSender<String>,
}

struct BridgeRuntime {
    config: BridgeConfig,
    codex: CodexTransport,
    codex_events: UnboundedReceiver<CodexEvent>,
    codex_request_client: CodexRequestClient,
    version_reader: BridgePackageVersionStatusReader,
    relay_url: String,
    session_id: String,
    notification_secret: String,
    secure_transport: BridgeSecureTransport,
    bridge_response_tx: UnboundedSender<String>,
    bridge_response_rx: UnboundedReceiver<String>,
    application_tx: UnboundedSender<String>,
    application_rx: UnboundedReceiver<String>,
    pending_auth_login: Arc<Mutex<PendingAuthLogin>>,
    forwarded_initialize_request_ids: HashSet<String>,
    forwarded_request_methods_by_id: HashMap<String, TrackedRequest>,
    relay_sanitized_response_methods_by_id: HashMap<String, TrackedRequest>,
    last_relay_activity_at: Option<Instant>,
    last_connection_status: Option<String>,
    last_published_status: Option<BridgeStatusSnapshot>,
    codex_handshake_warm: bool,
    context_usage_watch: Option<ContextUsageWatch>,
}

pub async fn start_bridge(options: StartBridgeOptions) -> Result<()> {
    let config = options.config.unwrap_or_else(read_bridge_config);
    let relay_url = config.relay_url.trim().trim_end_matches('/').to_owned();
    if relay_url.is_empty() {
        return Err(eyre!(
            "[remodex] No relay URL configured. Run ./run-local-remodex.sh or set REMODEX_RELAY."
        ));
    }

    let device_state = load_or_create_bridge_device_state()?;
    let relay_session = resolve_bridge_relay_session(device_state);
    let mut secure_transport = BridgeSecureTransport::new(
        relay_session.session_id.clone(),
        relay_url.clone(),
        relay_session.device_state,
    );
    let pairing_payload = secure_transport.create_pairing_payload();
    write_pairing_session(pairing_payload.clone())?;
    if options.print_pairing_qr {
        print_qr(&pairing_payload)?;
    }

    let (codex, codex_events) = CodexTransport::connect(&config.codex_endpoint).await?;
    let waiters = Arc::new(Mutex::new(HashMap::new()));
    let codex_request_client = CodexRequestClient {
        transport: codex.clone(),
        waiters: waiters.clone(),
    };
    let (bridge_response_tx, bridge_response_rx) = unbounded_channel();
    let (application_tx, application_rx) = unbounded_channel();

    let mut runtime = BridgeRuntime {
        config: config.clone(),
        codex,
        codex_events,
        codex_request_client,
        version_reader: BridgePackageVersionStatusReader::new(),
        relay_url,
        session_id: relay_session.session_id,
        notification_secret: random_hex(24),
        secure_transport,
        bridge_response_tx,
        bridge_response_rx,
        application_tx,
        application_rx,
        pending_auth_login: Arc::new(Mutex::new(PendingAuthLogin::default())),
        forwarded_initialize_request_ids: HashSet::new(),
        forwarded_request_methods_by_id: HashMap::new(),
        relay_sanitized_response_methods_by_id: HashMap::new(),
        last_relay_activity_at: None,
        last_connection_status: None,
        last_published_status: None,
        codex_handshake_warm: !config.codex_endpoint.trim().is_empty(),
        context_usage_watch: None,
    };

    runtime.run().await
}

impl BridgeRuntime {
    async fn run(&mut self) -> Result<()> {
        self.publish_status("starting", "starting", "")?;
        let mut reconnect_attempt = 0_u64;
        let (shutdown_tx, mut shutdown_rx) = unbounded_channel::<()>();
        tokio::spawn(async move {
            shutdown_signal().await;
            let _ = shutdown_tx.send(());
        });

        loop {
            self.log_connection_status("connecting")?;
            match self.connect_relay().await {
                Ok(connection) => {
                    reconnect_attempt = 0;
                    let reconnect = self.run_connected(connection, &mut shutdown_rx).await?;
                    if !reconnect {
                        self.codex.shutdown();
                        self.publish_status("stopped", "disconnected", "")?;
                        clear_bridge_status();
                        return Ok(());
                    }
                }
                Err(error) => {
                    self.publish_status("error", "error", &error.to_string())?;
                }
            }

            reconnect_attempt += 1;
            let delay_ms = (reconnect_attempt * 1_000).min(5_000);
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    self.codex.shutdown();
                    self.publish_status("stopped", "disconnected", "")?;
                    clear_bridge_status();
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
            }
        }
    }

    async fn connect_relay(&self) -> Result<RelayConnection> {
        let mut request = relay_ws_url(&self.relay_url, &self.session_id)?
            .into_client_request()
            .wrap_err("Failed to build relay websocket request")?;
        request
            .headers_mut()
            .insert("x-role", HeaderValue::from_static("mac"));
        request.headers_mut().insert(
            "x-notification-secret",
            HeaderValue::from_str(&self.notification_secret)?,
        );

        for (key, value) in
            build_mac_registration_headers(self.secure_transport.current_device_state())
        {
            request
                .headers_mut()
                .insert(key, HeaderValue::from_str(&value)?);
        }

        let (socket, _) = connect_async(request)
            .await
            .wrap_err("Failed to connect to relay")?;
        let (write, read) = socket.split();
        let (command_tx, mut command_rx) = unbounded_channel::<RelayCommand>();
        let (wire_text_tx, mut wire_text_rx) = unbounded_channel::<String>();

        let relay_command_tx = command_tx.clone();
        tokio::spawn(async move {
            while let Some(message) = wire_text_rx.recv().await {
                let _ = relay_command_tx.send(RelayCommand::Text(message));
            }
        });

        tokio::spawn(async move {
            let mut write = write;
            while let Some(command) = command_rx.recv().await {
                let result = match command {
                    RelayCommand::Text(text) => write.send(Message::Text(text)).await,
                    RelayCommand::Ping => write.send(Message::Ping(Vec::new())).await,
                    RelayCommand::Close => write.send(Message::Close(None)).await,
                };
                if result.is_err() {
                    break;
                }
            }
        });

        Ok(RelayConnection {
            reader: read,
            command_tx,
            wire_text_tx,
        })
    }

    async fn run_connected(
        &mut self,
        mut relay: RelayConnection,
        shutdown_rx: &mut UnboundedReceiver<()>,
    ) -> Result<bool> {
        self.last_relay_activity_at = Some(Instant::now());
        self.secure_transport
            .bind_live_send_wire_message(relay.wire_text_tx.clone());
        send_relay_registration_update(
            &relay.command_tx,
            self.secure_transport.current_device_state(),
        );
        self.log_connection_status("connected")?;

        let mut ping_interval =
            tokio::time::interval(Duration::from_millis(RELAY_WATCHDOG_PING_INTERVAL_MS));
        let mut heartbeat_interval =
            tokio::time::interval(Duration::from_millis(BRIDGE_STATUS_HEARTBEAT_INTERVAL_MS));
        let mut rollout_interval = tokio::time::interval(Duration::from_millis(700));
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        rollout_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    let _ = relay.command_tx.send(RelayCommand::Close);
                    return Ok(false);
                }
                maybe_event = self.codex_events.recv() => match maybe_event {
                    Some(CodexEvent::Message(message)) => {
                        self.handle_codex_message(message);
                    }
                    Some(CodexEvent::Error(message)) => {
                        return Err(eyre!(message));
                    }
                    Some(CodexEvent::Closed) | None => {
                        return Err(eyre!("Codex transport closed"));
                    }
                },
                maybe_response = self.bridge_response_rx.recv() => {
                    if let Some(response) = maybe_response {
                        self.secure_transport.queue_outbound_application_message(response);
                    }
                }
                maybe_application = self.application_rx.recv() => {
                    if let Some(message) = maybe_application {
                        self.handle_application_message(message);
                    }
                }
                next_message = relay.reader.next() => match next_message {
                    Some(Ok(Message::Text(text))) => {
                        self.mark_relay_activity();
                        let result = self.secure_transport.handle_incoming_wire_message(&text, {
                            let application_tx = self.application_tx.clone();
                            move |message| {
                                let _ = application_tx.send(message);
                            }
                        });
                        if result.trusted_phone_updated {
                            send_relay_registration_update(
                                &relay.command_tx,
                                self.secure_transport.current_device_state(),
                            );
                        }
                    }
                    Some(Ok(Message::Binary(binary))) => {
                        if let Ok(text) = String::from_utf8(binary.to_vec()) {
                            self.mark_relay_activity();
                            let result = self.secure_transport.handle_incoming_wire_message(&text, {
                                let application_tx = self.application_tx.clone();
                                move |message| {
                                    let _ = application_tx.send(message);
                                }
                            });
                            if result.trusted_phone_updated {
                                send_relay_registration_update(
                                    &relay.command_tx,
                                    self.secure_transport.current_device_state(),
                                );
                            }
                        }
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {
                        self.mark_relay_activity();
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        self.handle_transport_reset();
                        self.log_connection_status("disconnected")?;
                        return Ok(true);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => {
                        self.handle_transport_reset();
                        self.log_connection_status("disconnected")?;
                        return Ok(true);
                    }
                },
                _ = ping_interval.tick() => {
                    let stale = self.last_relay_activity_at
                        .map(|at| at.elapsed() >= Duration::from_millis(RELAY_WATCHDOG_STALE_AFTER_MS))
                        .unwrap_or(false);
                    if stale {
                        self.handle_transport_reset();
                        self.publish_status("running", "disconnected", STALE_RELAY_STATUS_MESSAGE)?;
                        return Ok(true);
                    }
                    let _ = relay.command_tx.send(RelayCommand::Ping);
                }
                _ = heartbeat_interval.tick() => {
                    self.write_heartbeat_status()?;
                }
                _ = rollout_interval.tick() => {
                    if let Some(notification) = self.poll_context_usage() {
                        self.secure_transport.queue_outbound_application_message(notification);
                    }
                }
            }
        }
    }

    fn handle_application_message(&mut self, raw_message: String) {
        if self.handle_bridge_managed_handshake_message(&raw_message) {
            return;
        }

        let parsed = match serde_json::from_str::<Value>(&raw_message) {
            Ok(parsed) => parsed,
            Err(_) => return,
        };
        let method = read_string(parsed.get("method")).unwrap_or_default();
        if method.is_empty() {
            return;
        }
        let request_id = parsed.get("id").cloned();
        let params = parsed.get("params").cloned().unwrap_or_else(|| json!({}));

        match method.as_str() {
            "account/status/read" | "getAuthStatus" | "account/login/openOnMac" => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::Account);
            }
            "voice/resolveAuth" | "voice/transcribe" => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::Voice);
            }
            "thread/contextWindow/read" => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::ThreadContext);
            }
            "desktop/continueOnMac" => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::Desktop);
            }
            "notifications/push/register" => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::Notifications);
            }
            other if other.starts_with("workspace/") => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::Workspace);
            }
            other if other.starts_with("git/") => {
                self.spawn_bridge_request(request_id, method, params, BridgeTask::Git);
            }
            _ => {
                self.remember_forwarded_request_method(&parsed);
                self.remember_thread_from_message("phone", &raw_message);
                self.codex.send(raw_message);
            }
        }
    }

    fn spawn_bridge_request(
        &self,
        request_id: Option<Value>,
        method: String,
        params: Value,
        task: BridgeTask,
    ) {
        let response_tx = self.bridge_response_tx.clone();
        let codex_client = self.codex_request_client.clone();
        let version_reader = self.version_reader.clone();
        let pending_auth_login = self.pending_auth_login.clone();
        let codex_bundle_id = self.config.codex_bundle_id.clone();
        tokio::spawn(async move {
            let result = match task {
                BridgeTask::Account => {
                    handle_account_request(
                        &method,
                        &params,
                        codex_client.clone(),
                        version_reader,
                        pending_auth_login.clone(),
                    )
                    .await
                }
                BridgeTask::Voice => handle_voice_request(&method, &params, move || {
                    let codex_client = codex_client.clone();
                    let version_reader = version_reader.clone();
                    let pending_auth_login = pending_auth_login.clone();
                    Box::pin(async move {
                        read_sanitized_auth_status(codex_client, version_reader, pending_auth_login)
                            .await
                    })
                })
                .await
                .and_then(|value| value.ok_or_else(|| eyre!("Unsupported voice method"))),
                BridgeTask::ThreadContext => handle_thread_context_request(&params).await,
                BridgeTask::Desktop => handle_desktop_request(&method, &params)
                    .await
                    .and_then(|value| value.ok_or_else(|| eyre!("Unsupported desktop method"))),
                BridgeTask::Notifications => handle_notifications_request(&method, &params)
                    .await
                    .and_then(|value| {
                        value.ok_or_else(|| eyre!("Unsupported notifications method"))
                    }),
                BridgeTask::Workspace => handle_workspace_request(&method, &params)
                    .await
                    .and_then(|value| value.ok_or_else(|| eyre!("Unsupported workspace method"))),
                BridgeTask::Git => handle_git_request(&method, &params)
                    .await
                    .and_then(|value| value.ok_or_else(|| eyre!("Unsupported git method"))),
            };

            if let Some(id) = request_id {
                let response = match result {
                    Ok(result) => success_response(id, result),
                    Err(error) => {
                        error_response(id, error_code_for_method(&method), error.to_string())
                    }
                };
                let _ = response_tx.send(response);
            } else if method == "account/login/openOnMac" {
                let _ = codex_bundle_id;
            }
        });
    }

    fn handle_codex_message(&mut self, raw_message: String) {
        if self.handle_bridge_managed_codex_response(&raw_message) {
            return;
        }

        self.update_pending_auth_login_from_codex_message(&raw_message);
        self.track_codex_handshake_state(&raw_message);
        self.remember_thread_from_message("codex", &raw_message);
        let sanitized = self.sanitize_relay_bound_codex_message(raw_message);
        self.secure_transport
            .queue_outbound_application_message(sanitized);
    }

    fn handle_bridge_managed_handshake_message(&mut self, raw_message: &str) -> bool {
        let Ok(parsed) = serde_json::from_str::<Value>(raw_message) else {
            return false;
        };
        let method = read_string(parsed.get("method")).unwrap_or_default();
        if method.is_empty() {
            return false;
        }

        if method == "initialize" && parsed.get("id").is_some() {
            let id = parsed.get("id").cloned().unwrap_or(Value::Null);
            if !self.codex_handshake_warm {
                if let Some(request_id) = stringify_request_id(&id) {
                    self.forwarded_initialize_request_ids.insert(request_id);
                }
                return false;
            }

            self.secure_transport
                .queue_outbound_application_message(success_response(
                    id,
                    json!({ "bridgeManaged": true }),
                ));
            return true;
        }

        method == "initialized" && self.codex_handshake_warm
    }

    fn track_codex_handshake_state(&mut self, raw_message: &str) {
        let Ok(parsed) = serde_json::from_str::<Value>(raw_message) else {
            return;
        };
        let Some(request_id) = parsed.get("id").and_then(stringify_request_id) else {
            return;
        };
        if !self.forwarded_initialize_request_ids.remove(&request_id) {
            return;
        }

        if parsed.get("result").is_some() {
            self.codex_handshake_warm = true;
            return;
        }

        let error_message = read_string(parsed.get("error").and_then(|value| value.get("message")))
            .unwrap_or_default();
        if error_message
            .to_ascii_lowercase()
            .contains("already initialized")
        {
            self.codex_handshake_warm = true;
        }
    }

    fn handle_bridge_managed_codex_response(&self, raw_message: &str) -> bool {
        let Ok(parsed) = serde_json::from_str::<Value>(raw_message) else {
            return false;
        };
        let Some(request_id) = parsed.get("id").and_then(stringify_request_id) else {
            return false;
        };

        let waiter = self
            .codex_request_client
            .waiters
            .lock()
            .ok()
            .and_then(|mut waiters| waiters.remove(&request_id));
        let Some(waiter) = waiter else {
            return false;
        };

        if let Some(error) = parsed.get("error") {
            let message = read_string(error.get("message")).unwrap_or_default();
            let _ = waiter.send(Err(if message.is_empty() {
                "Codex request failed".to_owned()
            } else {
                message
            }));
            return true;
        }

        let _ = waiter.send(Ok(parsed.get("result").cloned().unwrap_or(Value::Null)));
        true
    }

    fn update_pending_auth_login_from_codex_message(&mut self, raw_message: &str) {
        self.prune_tracked_requests();
        let Ok(parsed) = serde_json::from_str::<Value>(raw_message) else {
            return;
        };

        if let Some(response_id) = parsed.get("id").and_then(stringify_request_id) {
            if let Some(tracked) = self.forwarded_request_methods_by_id.remove(&response_id) {
                match tracked.method.as_str() {
                    "account/login/start" => {
                        let login_id = read_string(
                            parsed.get("result").and_then(|value| value.get("loginId")),
                        );
                        let auth_url = read_string(
                            parsed.get("result").and_then(|value| value.get("authUrl")),
                        );
                        if login_id.is_none() || auth_url.is_none() {
                            self.clear_pending_auth_login();
                            return;
                        }
                        if let Ok(mut pending) = self.pending_auth_login.lock() {
                            pending.login_id = login_id;
                            pending.auth_url = auth_url;
                            pending.request_id = Some(response_id);
                            pending.started_at = Some(Instant::now());
                        }
                        return;
                    }
                    "account/login/cancel" | "account/logout" => {
                        self.clear_pending_auth_login();
                        return;
                    }
                    _ => {}
                }
            }
        }

        let method = read_string(parsed.get("method")).unwrap_or_default();
        if method == "account/login/completed" || method == "account/updated" {
            self.clear_pending_auth_login();
        }
    }

    fn clear_pending_auth_login(&self) {
        if let Ok(mut pending) = self.pending_auth_login.lock() {
            *pending = PendingAuthLogin::default();
        }
    }

    fn remember_forwarded_request_method(&mut self, parsed: &Value) {
        self.prune_tracked_requests();
        let method = read_string(parsed.get("method")).unwrap_or_default();
        let Some(request_id) = parsed.get("id").and_then(stringify_request_id) else {
            return;
        };
        if matches!(
            method.as_str(),
            "account/login/start" | "account/login/cancel" | "account/logout"
        ) {
            self.forwarded_request_methods_by_id.insert(
                request_id.clone(),
                TrackedRequest {
                    method: method.clone(),
                    created_at: Instant::now(),
                },
            );
        }
        if matches!(method.as_str(), "thread/read" | "thread/resume") {
            self.relay_sanitized_response_methods_by_id.insert(
                request_id,
                TrackedRequest {
                    method,
                    created_at: Instant::now(),
                },
            );
        }
    }

    fn prune_tracked_requests(&mut self) {
        self.forwarded_request_methods_by_id.retain(|_, tracked| {
            tracked.created_at.elapsed() < Duration::from_millis(FORWARDED_REQUEST_METHOD_TTL_MS)
        });
        self.relay_sanitized_response_methods_by_id
            .retain(|_, tracked| {
                tracked.created_at.elapsed()
                    < Duration::from_millis(FORWARDED_REQUEST_METHOD_TTL_MS)
            });
    }

    fn sanitize_relay_bound_codex_message(&mut self, raw_message: String) -> String {
        self.prune_tracked_requests();
        let Ok(parsed) = serde_json::from_str::<Value>(&raw_message) else {
            return raw_message;
        };
        let Some(response_id) = parsed.get("id").and_then(stringify_request_id) else {
            return raw_message;
        };
        let Some(tracked) = self
            .relay_sanitized_response_methods_by_id
            .remove(&response_id)
        else {
            return raw_message;
        };
        sanitize_thread_history_images_for_relay(raw_message, &tracked.method)
    }

    fn remember_thread_from_message(&mut self, source: &str, raw_message: &str) {
        let context = extract_bridge_message_context(raw_message);
        if context.thread_id.is_empty() {
            return;
        }

        let _ = remember_active_thread(&context.thread_id, source);
        if matches!(context.method.as_str(), "turn/start" | "turn/started") {
            let key = format!(
                "{}|{}",
                context.thread_id,
                context
                    .turn_id
                    .clone()
                    .unwrap_or_else(|| "pending-turn".to_owned())
            );
            if self
                .context_usage_watch
                .as_ref()
                .map(|watch| watch.key.as_str())
                == Some(key.as_str())
            {
                return;
            }
            self.context_usage_watch = Some(ContextUsageWatch {
                key,
                thread_id: context.thread_id,
                turn_id: context.turn_id,
                started_at: Instant::now(),
                last_usage_json: None,
            });
        }
    }

    fn poll_context_usage(&mut self) -> Option<String> {
        let watch = self.context_usage_watch.as_mut()?;
        if watch.started_at.elapsed() > Duration::from_secs(90) {
            self.context_usage_watch = None;
            return None;
        }
        let result = thread_context_read(&watch.thread_id, watch.turn_id.as_deref());
        let usage = result.get("usage")?.clone();
        let usage_json = serde_json::to_string(&usage).ok()?;
        if watch.last_usage_json.as_deref() == Some(usage_json.as_str()) {
            return None;
        }
        watch.last_usage_json = Some(usage_json);
        Some(
            json!({
                "method": "thread/tokenUsage/updated",
                "params": {
                    "threadId": watch.thread_id,
                    "usage": usage,
                }
            })
            .to_string(),
        )
    }

    fn mark_relay_activity(&mut self) {
        self.last_relay_activity_at = Some(Instant::now());
    }

    fn handle_transport_reset(&mut self) {
        self.last_relay_activity_at = None;
        self.context_usage_watch = None;
    }

    fn log_connection_status(&mut self, status: &str) -> Result<()> {
        if self.last_connection_status.as_deref() == Some(status) {
            return Ok(());
        }

        self.last_connection_status = Some(status.to_owned());
        self.publish_status("running", status, "")?;
        println!("[remodex] {status}");
        Ok(())
    }

    fn publish_status(
        &mut self,
        state: &str,
        connection_status: &str,
        last_error: &str,
    ) -> Result<()> {
        self.last_published_status = Some(BridgeStatusSnapshot {
            state: state.to_owned(),
            connection_status: connection_status.to_owned(),
            last_error: last_error.to_owned(),
        });
        write_bridge_status(state, connection_status, std::process::id(), last_error)
    }

    fn write_heartbeat_status(&mut self) -> Result<()> {
        let Some(status) = self.last_published_status.clone() else {
            return Ok(());
        };

        if status.connection_status != "connected" {
            return write_bridge_status(
                &status.state,
                &status.connection_status,
                std::process::id(),
                &status.last_error,
            );
        }

        let stale = self
            .last_relay_activity_at
            .map(|at| at.elapsed() >= Duration::from_millis(RELAY_WATCHDOG_STALE_AFTER_MS))
            .unwrap_or(false);
        if stale {
            return write_bridge_status(
                &status.state,
                "disconnected",
                std::process::id(),
                STALE_RELAY_STATUS_MESSAGE,
            );
        }

        write_bridge_status(
            &status.state,
            &status.connection_status,
            std::process::id(),
            &status.last_error,
        )
    }
}

#[derive(Clone, Copy)]
enum BridgeTask {
    Account,
    Voice,
    ThreadContext,
    Desktop,
    Notifications,
    Workspace,
    Git,
}

impl CodexRequestClient {
    async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let request_id = format!("bridge-managed-{}", Uuid::new_v4());
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .map_err(|_| eyre!("Failed to lock Codex request waiters"))?
            .insert(request_id.clone(), tx);

        self.transport.send(
            json!({
                "id": request_id,
                "method": method,
                "params": params,
            })
            .to_string(),
        );

        match tokio::time::timeout(Duration::from_secs(20), rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(message))) => Err(eyre!(message)),
            Ok(Err(_)) => Err(eyre!("Codex response waiter dropped")),
            Err(_) => Err(eyre!("Codex request timed out: {method}")),
        }
    }
}

async fn handle_account_request(
    method: &str,
    params: &Value,
    codex_client: CodexRequestClient,
    version_reader: BridgePackageVersionStatusReader,
    pending_auth_login: Arc<Mutex<PendingAuthLogin>>,
) -> Result<Value> {
    match method {
        "account/status/read" | "getAuthStatus" => {
            read_sanitized_auth_status(codex_client, version_reader, pending_auth_login).await
        }
        "account/login/openOnMac" => {
            let auth_url = read_string(params.get("authUrl")).or_else(|| {
                pending_auth_login
                    .lock()
                    .ok()
                    .and_then(|pending| pending.auth_url.clone())
            });
            let auth_url = auth_url.ok_or_else(|| {
                eyre!("No pending ChatGPT sign-in URL is available on this bridge.")
            })?;
            let status = std::process::Command::new("open").arg(&auth_url).status()?;
            if !status.success() {
                return Err(eyre!("Could not open the ChatGPT sign-in URL on this Mac."));
            }
            Ok(json!({
                "success": true,
                "openedOnMac": true,
            }))
        }
        other => Err(eyre!("Unsupported bridge-managed account method: {other}")),
    }
}

async fn read_sanitized_auth_status(
    codex_client: CodexRequestClient,
    version_reader: BridgePackageVersionStatusReader,
    pending_auth_login: Arc<Mutex<PendingAuthLogin>>,
) -> Result<Value> {
    let account_read = codex_client
        .send_request("account/read", json!({ "refreshToken": false }))
        .await
        .ok();
    let auth_status = codex_client
        .send_request(
            "getAuthStatus",
            json!({
                "includeToken": true,
                "refreshToken": true,
            }),
        )
        .await
        .ok();
    let version_info = version_reader.read().await;
    let login_in_flight = pending_auth_login
        .lock()
        .ok()
        .and_then(|pending| pending.login_id.clone())
        .is_some();
    compose_sanitized_auth_status(
        account_read,
        auth_status,
        login_in_flight,
        Some(version_info),
    )
}

async fn handle_thread_context_request(params: &Value) -> Result<Value> {
    let thread_id = read_string(params.get("threadId"))
        .or_else(|| read_string(params.get("thread_id")))
        .ok_or_else(|| eyre!("thread/contextWindow/read requires a threadId."))?;
    let turn_id = read_string(params.get("turnId")).or_else(|| read_string(params.get("turn_id")));
    Ok(thread_context_read(&thread_id, turn_id.as_deref()))
}

fn send_relay_registration_update(
    command_tx: &UnboundedSender<RelayCommand>,
    device_state: &BridgeDeviceState,
) {
    let _ = command_tx.send(RelayCommand::Text(
        json!({
            "kind": "relayMacRegistration",
            "registration": build_mac_registration(device_state),
        })
        .to_string(),
    ));
}

fn build_mac_registration_headers(device_state: &BridgeDeviceState) -> Vec<(&'static str, String)> {
    let registration = build_mac_registration(device_state);
    let mut headers = vec![
        (
            "x-mac-device-id",
            read_string(registration.get("macDeviceId")).unwrap_or_default(),
        ),
        (
            "x-mac-identity-public-key",
            read_string(registration.get("macIdentityPublicKey")).unwrap_or_default(),
        ),
        (
            "x-machine-name",
            read_string(registration.get("displayName")).unwrap_or_default(),
        ),
    ];
    if let Some(trusted_phone_device_id) = read_string(registration.get("trustedPhoneDeviceId")) {
        headers.push(("x-trusted-phone-device-id", trusted_phone_device_id));
    }
    if let Some(trusted_phone_public_key) = read_string(registration.get("trustedPhonePublicKey")) {
        headers.push(("x-trusted-phone-public-key", trusted_phone_public_key));
    }
    headers
}

fn build_mac_registration(device_state: &BridgeDeviceState) -> Value {
    let trusted_phone_entry = device_state
        .trusted_phones
        .iter()
        .next()
        .map(|(device_id, public_key)| (device_id.clone(), public_key.clone()));
    json!({
        "macDeviceId": device_state.mac_device_id,
        "macIdentityPublicKey": device_state.mac_identity_public_key,
        "displayName": hostname::get()
            .ok()
            .and_then(|value| value.into_string().ok())
            .unwrap_or_default(),
        "trustedPhoneDeviceId": trusted_phone_entry.as_ref().map(|entry| entry.0.clone()),
        "trustedPhonePublicKey": trusted_phone_entry.as_ref().map(|entry| entry.1.clone()),
    })
}

fn relay_ws_url(base_url: &str, session_id: &str) -> Result<String> {
    let mut url = url::Url::parse(base_url)?;
    match url.scheme() {
        "http" => {
            let _ = url.set_scheme("ws");
        }
        "https" => {
            let _ = url.set_scheme("wss");
        }
        "ws" | "wss" => {}
        other => return Err(eyre!("Unsupported relay URL scheme: {other}")),
    }
    let next_path = format!("{}/{}", url.path().trim_end_matches('/'), session_id);
    url.set_path(&next_path);
    Ok(url.to_string())
}

fn random_hex(len: usize) -> String {
    let mut bytes = vec![0_u8; len];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sanitize_thread_history_images_for_relay(raw_message: String, request_method: &str) -> String {
    if request_method != "thread/read" && request_method != "thread/resume" {
        return raw_message;
    }

    let Ok(mut parsed) = serde_json::from_str::<Value>(&raw_message) else {
        return raw_message;
    };
    let Some(turns) = parsed
        .get_mut("result")
        .and_then(|value| value.get_mut("thread"))
        .and_then(|value| value.get_mut("turns"))
        .and_then(Value::as_array_mut)
    else {
        return raw_message;
    };

    let mut did_change = false;
    for turn in turns {
        let Some(items) = turn.get_mut("items").and_then(Value::as_array_mut) else {
            continue;
        };
        for item in items {
            let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for content_item in content {
                if sanitize_inline_history_image_content_item(content_item) {
                    did_change = true;
                }
            }
        }
    }

    if did_change {
        parsed.to_string()
    } else {
        raw_message
    }
}

fn sanitize_inline_history_image_content_item(content_item: &mut Value) -> bool {
    let normalized_type = read_string(content_item.get("type"))
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace([' ', '_', '-'], "");
    if normalized_type != "image" && normalized_type != "localimage" {
        return false;
    }

    let has_inline_url = is_inline_history_image_data_url(content_item.get("url"))
        || is_inline_history_image_data_url(content_item.get("image_url"))
        || is_inline_history_image_data_url(content_item.get("path"));
    if !has_inline_url {
        return false;
    }

    if let Some(object) = content_item.as_object_mut() {
        object.remove("path");
        object.remove("image_url");
        object.insert(
            "url".to_owned(),
            Value::String(RELAY_HISTORY_IMAGE_REFERENCE_URL.to_owned()),
        );
        return true;
    }

    false
}

fn is_inline_history_image_data_url(value: Option<&Value>) -> bool {
    read_string(value)
        .map(|value| value.to_ascii_lowercase().starts_with("data:image"))
        .unwrap_or(false)
}

struct MessageContext {
    method: String,
    thread_id: String,
    turn_id: Option<String>,
}

fn extract_bridge_message_context(raw_message: &str) -> MessageContext {
    let Ok(parsed) = serde_json::from_str::<Value>(raw_message) else {
        return MessageContext {
            method: String::new(),
            thread_id: String::new(),
            turn_id: None,
        };
    };
    let method = read_string(parsed.get("method")).unwrap_or_default();
    let params = parsed.get("params").cloned().unwrap_or_else(|| json!({}));
    let thread_id = extract_thread_id(&method, &params).unwrap_or_default();
    let turn_id = extract_turn_id(&method, &params);

    MessageContext {
        method,
        thread_id,
        turn_id,
    }
}

fn extract_thread_id(method: &str, params: &Value) -> Option<String> {
    match method {
        "turn/start" | "turn/started" | "turn/completed" => read_string(params.get("threadId"))
            .or_else(|| read_string(params.get("thread_id")))
            .or_else(|| read_string(params.get("turn").and_then(|value| value.get("threadId"))))
            .or_else(|| read_string(params.get("turn").and_then(|value| value.get("thread_id")))),
        "thread/start" | "thread/started" => read_string(params.get("threadId"))
            .or_else(|| read_string(params.get("thread_id")))
            .or_else(|| read_string(params.get("thread").and_then(|value| value.get("id"))))
            .or_else(|| read_string(params.get("thread").and_then(|value| value.get("threadId"))))
            .or_else(|| {
                read_string(
                    params
                        .get("thread")
                        .and_then(|value| value.get("thread_id")),
                )
            }),
        _ => None,
    }
}

fn extract_turn_id(method: &str, params: &Value) -> Option<String> {
    match method {
        "turn/started" | "turn/completed" => read_string(params.get("turnId"))
            .or_else(|| read_string(params.get("turn_id")))
            .or_else(|| read_string(params.get("id")))
            .or_else(|| read_string(params.get("turn").and_then(|value| value.get("id"))))
            .or_else(|| read_string(params.get("turn").and_then(|value| value.get("turnId"))))
            .or_else(|| read_string(params.get("turn").and_then(|value| value.get("turn_id")))),
        _ => None,
    }
}

fn read_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn stringify_request_id(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if !value.trim().is_empty() => Some(value.trim().to_owned()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn error_code_for_method(method: &str) -> &str {
    match method {
        "account/status/read" | "getAuthStatus" | "account/login/openOnMac" => "auth_status_failed",
        "voice/resolveAuth" | "voice/transcribe" => "voice_error",
        "thread/contextWindow/read" => "thread_context_error",
        "desktop/continueOnMac" => "desktop_error",
        "notifications/push/register" => "push_registration_failed",
        _ if method.starts_with("workspace/") => "workspace_error",
        _ if method.starts_with("git/") => "git_error",
        _ => "bridge_error",
    }
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut terminate = signal(SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            if let Some(signal) = terminate.as_mut() {
                let _ = signal.recv().await;
            }
        } => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{relay_ws_url, sanitize_thread_history_images_for_relay};

    #[test]
    fn relay_ws_url_converts_http_scheme_and_appends_session() {
        let resolved = relay_ws_url("http://127.0.0.1:9000/relay", "session-123").unwrap();
        assert_eq!(resolved, "ws://127.0.0.1:9000/relay/session-123");
    }

    #[test]
    fn sanitize_thread_history_images_for_relay_elides_inline_data_urls() {
        let raw_message = json!({
            "id": "req-thread-read",
            "result": {
                "thread": {
                    "id": "thread-images",
                    "turns": [
                        {
                            "id": "turn-1",
                            "items": [
                                {
                                    "id": "item-user",
                                    "type": "user_message",
                                    "content": [
                                        {
                                            "type": "input_text",
                                            "text": "Look at this screenshot",
                                        },
                                        {
                                            "type": "image",
                                            "image_url": "data:image/png;base64,AAAA",
                                        }
                                    ],
                                }
                            ],
                        }
                    ],
                }
            }
        })
        .to_string();

        let sanitized = sanitize_thread_history_images_for_relay(raw_message, "thread/read");
        let sanitized: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        let content = &sanitized["result"]["thread"]["turns"][0]["items"][0]["content"];

        assert_eq!(
            content[0],
            json!({
                "type": "input_text",
                "text": "Look at this screenshot",
            })
        );
        assert_eq!(
            content[1],
            json!({
                "type": "image",
                "url": "remodex://history-image-elided",
            })
        );
    }

    #[test]
    fn sanitize_thread_history_images_for_relay_leaves_unrelated_payloads_unchanged() {
        let raw_message = json!({
            "id": "req-other",
            "result": {
                "ok": true,
            },
        })
        .to_string();

        assert_eq!(
            sanitize_thread_history_images_for_relay(raw_message.clone(), "turn/start"),
            raw_message
        );
    }
}
