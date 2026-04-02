mod account;
mod messages;
mod relay;
mod support;
mod thread_runtime;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Result};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;

use self::thread_runtime::ThreadRuntimeRegistry;
use crate::codex_transport::{CodexEvent, CodexTransport};
use crate::config::{read_bridge_config, BridgeConfig};
use crate::daemon_state::write_pairing_session;
use crate::package_version_status::BridgePackageVersionStatusReader;
use crate::qr::print_qr;
use crate::rollout_live_mirror::RolloutLiveMirrorController;
use crate::secure_device_state::{
    load_or_create_bridge_device_state, resolve_bridge_relay_session,
};
use crate::secure_transport::BridgeSecureTransport;

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

#[derive(Clone, Copy, Debug, Default)]
struct RelayLivenessState {
    pending_ping_sent_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayWatchdogAction {
    SendPing,
    Wait,
    Reconnect,
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
    thread_runtime_registry: ThreadRuntimeRegistry,
    relay_liveness: RelayLivenessState,
    last_connection_status: Option<String>,
    last_published_status: Option<BridgeStatusSnapshot>,
    codex_handshake_warm: bool,
    context_usage_watch: Option<ContextUsageWatch>,
    rollout_live_mirror: Option<RolloutLiveMirrorController>,
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
        waiters,
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
        notification_secret: support::random_hex(24),
        secure_transport,
        bridge_response_tx,
        bridge_response_rx,
        application_tx,
        application_rx,
        pending_auth_login: Arc::new(Mutex::new(PendingAuthLogin::default())),
        forwarded_initialize_request_ids: HashSet::new(),
        forwarded_request_methods_by_id: HashMap::new(),
        relay_sanitized_response_methods_by_id: HashMap::new(),
        thread_runtime_registry: ThreadRuntimeRegistry::default(),
        relay_liveness: RelayLivenessState::default(),
        last_connection_status: None,
        last_published_status: None,
        codex_handshake_warm: !config.codex_endpoint.trim().is_empty(),
        context_usage_watch: None,
        rollout_live_mirror: if config.codex_endpoint.trim().is_empty() {
            Some(RolloutLiveMirrorController::new(
                crate::rollout::resolve_sessions_root(),
            ))
        } else {
            None
        },
    };

    runtime.run().await
}

impl RelayLivenessState {
    fn mark_connected(&mut self) {
        self.mark_activity_at(Instant::now());
    }

    fn mark_activity(&mut self) {
        self.mark_activity_at(Instant::now());
    }

    fn mark_activity_at(&mut self, _at: Instant) {
        self.pending_ping_sent_at = None;
    }

    fn next_watchdog_action(&mut self, stale_after: Duration) -> RelayWatchdogAction {
        self.next_watchdog_action_at(Instant::now(), stale_after)
    }

    fn next_watchdog_action_at(
        &mut self,
        now: Instant,
        stale_after: Duration,
    ) -> RelayWatchdogAction {
        match self.pending_ping_sent_at {
            Some(sent_at) if now.duration_since(sent_at) >= stale_after => {
                RelayWatchdogAction::Reconnect
            }
            Some(_) => RelayWatchdogAction::Wait,
            None => {
                self.pending_ping_sent_at = Some(now);
                RelayWatchdogAction::SendPing
            }
        }
    }

    fn is_stale(&self, stale_after: Duration) -> bool {
        self.is_stale_at(Instant::now(), stale_after)
    }

    fn is_stale_at(&self, now: Instant, stale_after: Duration) -> bool {
        self.pending_ping_sent_at
            .map(|sent_at| now.duration_since(sent_at) >= stale_after)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::{RelayLivenessState, RelayWatchdogAction};
    use std::time::{Duration, Instant};

    #[test]
    fn watchdog_waits_for_the_probe_deadline_before_reconnecting() {
        let start = Instant::now();
        let mut liveness = RelayLivenessState::default();
        liveness.mark_activity_at(start);

        assert_eq!(
            liveness
                .next_watchdog_action_at(start + Duration::from_secs(10), Duration::from_secs(25)),
            RelayWatchdogAction::SendPing
        );
        assert_eq!(
            liveness
                .next_watchdog_action_at(start + Duration::from_secs(20), Duration::from_secs(25)),
            RelayWatchdogAction::Wait
        );
    }

    #[test]
    fn watchdog_reconnects_after_a_missed_probe_deadline() {
        let start = Instant::now();
        let mut liveness = RelayLivenessState::default();
        liveness.mark_activity_at(start);
        assert_eq!(
            liveness
                .next_watchdog_action_at(start + Duration::from_secs(10), Duration::from_secs(25)),
            RelayWatchdogAction::SendPing
        );

        assert_eq!(
            liveness
                .next_watchdog_action_at(start + Duration::from_secs(36), Duration::from_secs(25)),
            RelayWatchdogAction::Reconnect
        );
        assert!(liveness.is_stale_at(start + Duration::from_secs(36), Duration::from_secs(25)));
    }

    #[test]
    fn inbound_activity_clears_a_pending_probe() {
        let start = Instant::now();
        let mut liveness = RelayLivenessState::default();
        liveness.mark_activity_at(start);
        assert_eq!(
            liveness
                .next_watchdog_action_at(start + Duration::from_secs(10), Duration::from_secs(25)),
            RelayWatchdogAction::SendPing
        );

        liveness.mark_activity_at(start + Duration::from_secs(18));

        assert_eq!(
            liveness
                .next_watchdog_action_at(start + Duration::from_secs(28), Duration::from_secs(25)),
            RelayWatchdogAction::SendPing
        );
    }
}
