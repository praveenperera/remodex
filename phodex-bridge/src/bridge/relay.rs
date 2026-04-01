use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::codex_transport::CodexEvent;
use crate::daemon_state::{clear_bridge_status, write_bridge_status};

use super::support::{
    build_mac_registration_headers, relay_ws_url, send_relay_registration_update, shutdown_signal,
};
use super::{BridgeRuntime, BridgeStatusSnapshot, RelayCommand, RelayConnection};

impl BridgeRuntime {
    pub(super) async fn run(&mut self) -> Result<()> {
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
                    RelayCommand::Text(text) => write.send(Message::Text(text.into())).await,
                    RelayCommand::Ping => write.send(Message::Ping(Vec::new().into())).await,
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

        let mut ping_interval = tokio::time::interval(Duration::from_millis(
            super::RELAY_WATCHDOG_PING_INTERVAL_MS,
        ));
        let mut heartbeat_interval = tokio::time::interval(Duration::from_millis(
            super::BRIDGE_STATUS_HEARTBEAT_INTERVAL_MS,
        ));
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
                    Some(CodexEvent::Message(message)) => self.handle_codex_message(message),
                    Some(CodexEvent::Error(message)) => return Err(eyre!(message)),
                    Some(CodexEvent::Closed) | None => return Err(eyre!("Codex transport closed")),
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
                        self.handle_relay_text(text.as_ref(), &relay.command_tx);
                    }
                    Some(Ok(Message::Binary(binary))) => {
                        if let Ok(text) = String::from_utf8(binary.to_vec()) {
                            self.handle_relay_text(&text, &relay.command_tx);
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
                        .map(|at| at.elapsed() >= Duration::from_millis(super::RELAY_WATCHDOG_STALE_AFTER_MS))
                        .unwrap_or(false);
                    if stale {
                        self.handle_transport_reset();
                        self.publish_status("running", "disconnected", super::STALE_RELAY_STATUS_MESSAGE)?;
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
                    if let Some(rollout_live_mirror) = self.rollout_live_mirror.as_mut() {
                        for notification in rollout_live_mirror.poll_notifications() {
                            self.secure_transport.queue_outbound_application_message(notification);
                        }
                    }
                }
            }
        }
    }

    fn handle_relay_text(
        &mut self,
        text: &str,
        command_tx: &tokio::sync::mpsc::UnboundedSender<RelayCommand>,
    ) {
        self.mark_relay_activity();
        let result = self.secure_transport.handle_incoming_wire_message(text, {
            let application_tx = self.application_tx.clone();
            move |message| {
                let _ = application_tx.send(message);
            }
        });
        if result.trusted_phone_updated {
            send_relay_registration_update(
                command_tx,
                self.secure_transport.current_device_state(),
            );
        }
    }

    fn mark_relay_activity(&mut self) {
        self.last_relay_activity_at = Some(Instant::now());
    }

    fn handle_transport_reset(&mut self) {
        self.last_relay_activity_at = None;
        self.context_usage_watch = None;
        if let Some(rollout_live_mirror) = self.rollout_live_mirror.as_mut() {
            rollout_live_mirror.stop_all();
        }
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
            .map(|at| at.elapsed() >= Duration::from_millis(super::RELAY_WATCHDOG_STALE_AFTER_MS))
            .unwrap_or(false);
        if stale {
            return write_bridge_status(
                &status.state,
                "disconnected",
                std::process::id(),
                super::STALE_RELAY_STATUS_MESSAGE,
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
