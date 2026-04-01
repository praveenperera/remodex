use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use color_eyre::eyre::{eyre, Result, WrapErr};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug)]
pub enum CodexEvent {
    Message(String),
    Closed,
    Error(String),
}

#[derive(Clone)]
pub struct CodexTransport {
    outbound_tx: UnboundedSender<String>,
    shutdown_tx: watch::Sender<bool>,
    ready: Arc<AtomicBool>,
}

impl CodexTransport {
    pub async fn connect(endpoint: &str) -> Result<(Self, UnboundedReceiver<CodexEvent>)> {
        if endpoint.trim().is_empty() {
            Self::connect_spawn().await
        } else {
            Self::connect_websocket(endpoint.trim()).await
        }
    }

    pub fn send(&self, message: String) {
        if !self.ready.load(Ordering::Relaxed) {
            return;
        }

        let _ = self.outbound_tx.send(message);
    }

    pub fn shutdown(&self) {
        self.ready.store(false, Ordering::Relaxed);
        let _ = self.shutdown_tx.send(true);
    }

    async fn connect_spawn() -> Result<(Self, UnboundedReceiver<CodexEvent>)> {
        let description: Arc<str> = "`codex app-server`".into();
        let mut child = Command::new("codex")
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .wrap_err("Failed to launch `codex app-server`")?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| eyre!("`codex app-server` did not expose stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| eyre!("`codex app-server` did not expose stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| eyre!("`codex app-server` did not expose stderr"))?;

        let (outbound_tx, mut outbound_rx) = unbounded_channel::<String>();
        let (event_tx, event_rx) = unbounded_channel::<CodexEvent>();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let ready = Arc::new(AtomicBool::new(true));
        let ready_for_task = ready.clone();
        let description_for_task = description.clone();

        tokio::spawn(async move {
            let mut stdin = stdin;
            let mut stdout_lines = BufReader::new(stdout).lines();
            let mut stderr_lines = BufReader::new(stderr).lines();
            let mut stderr_buffer = String::new();
            let mut did_request_shutdown = false;
            let mut poll = tokio::time::interval(Duration::from_millis(200));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    line = stdout_lines.next_line() => match line {
                        Ok(Some(line)) => {
                            let line = line.trim();
                            if !line.is_empty() {
                                let _ = event_tx.send(CodexEvent::Message(line.to_owned()));
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let _ = event_tx.send(CodexEvent::Error(error.to_string()));
                            break;
                        }
                    },
                    line = stderr_lines.next_line() => match line {
                        Ok(Some(line)) => {
                            stderr_buffer.push_str(line.trim());
                            if stderr_buffer.len() > 4_096 {
                                let keep_from = stderr_buffer.len().saturating_sub(4_096);
                                stderr_buffer = stderr_buffer.split_off(keep_from);
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let _ = event_tx.send(CodexEvent::Error(error.to_string()));
                            break;
                        }
                    },
                    maybe_message = outbound_rx.recv() => {
                        let Some(message) = maybe_message else {
                            break;
                        };
                        if stdin.write_all(message.as_bytes()).await.is_err() {
                            break;
                        }
                        if stdin.write_all(b"\n").await.is_err() {
                            break;
                        }
                        let _ = stdin.flush().await;
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_ok() && *shutdown_rx.borrow() {
                            did_request_shutdown = true;
                            ready_for_task.store(false, Ordering::Relaxed);
                            let _ = child.start_kill();
                        }
                    }
                    _ = poll.tick() => {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                ready_for_task.store(false, Ordering::Relaxed);
                                if !did_request_shutdown && !status.success() {
                                    let reason = if stderr_buffer.trim().is_empty() {
                                        format!(
                                            "Codex launcher {} failed with status {}",
                                            description_for_task,
                                            status
                                        )
                                    } else {
                                        format!(
                                            "Codex launcher {} failed: {}",
                                            description_for_task,
                                            stderr_buffer.trim()
                                        )
                                    };
                                    let _ = event_tx.send(CodexEvent::Error(reason));
                                } else {
                                    let _ = event_tx.send(CodexEvent::Closed);
                                }
                                break;
                            }
                            Ok(None) => {}
                            Err(error) => {
                                ready_for_task.store(false, Ordering::Relaxed);
                                let _ = event_tx.send(CodexEvent::Error(error.to_string()));
                                break;
                            }
                        }
                    }
                }
            }

            ready_for_task.store(false, Ordering::Relaxed);
        });

        Ok((
            Self {
                outbound_tx,
                shutdown_tx,
                ready,
            },
            event_rx,
        ))
    }

    async fn connect_websocket(endpoint: &str) -> Result<(Self, UnboundedReceiver<CodexEvent>)> {
        let (outbound_tx, mut outbound_rx) = unbounded_channel::<String>();
        let (event_tx, event_rx) = unbounded_channel::<CodexEvent>();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let ready = Arc::new(AtomicBool::new(false));
        let ready_for_task = ready.clone();
        let endpoint_owned = endpoint.to_owned();

        tokio::spawn(async move {
            let connection = connect_async(&endpoint_owned).await;
            let Ok((socket, _)) = connection else {
                let message = connection
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "Failed to connect to Codex endpoint".to_owned());
                let _ = event_tx.send(CodexEvent::Error(message));
                return;
            };
            ready_for_task.store(true, Ordering::Relaxed);

            let (mut write, mut read) = socket.split();

            loop {
                tokio::select! {
                    maybe_message = outbound_rx.recv() => {
                        let Some(message) = maybe_message else {
                            break;
                        };
                        if write.send(Message::Text(message.into())).await.is_err() {
                            let _ = event_tx.send(CodexEvent::Closed);
                            break;
                        }
                    }
                    maybe_frame = read.next() => match maybe_frame {
                        Some(Ok(Message::Text(text))) => {
                            if !text.trim().is_empty() {
                                let _ = event_tx.send(CodexEvent::Message(text.to_string()));
                            }
                        }
                        Some(Ok(Message::Binary(binary))) => {
                            if let Ok(text) = String::from_utf8(binary.to_vec()) {
                                if !text.trim().is_empty() {
                                    let _ = event_tx.send(CodexEvent::Message(text));
                                }
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = write.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Close(_))) => {
                            let _ = event_tx.send(CodexEvent::Closed);
                            break;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(error)) => {
                            let _ = event_tx.send(CodexEvent::Error(error.to_string()));
                            break;
                        }
                        None => {
                            let _ = event_tx.send(CodexEvent::Closed);
                            break;
                        }
                    },
                    changed = shutdown_rx.changed() => {
                        if changed.is_ok() && *shutdown_rx.borrow() {
                            let _ = write.send(Message::Close(None)).await;
                            let _ = event_tx.send(CodexEvent::Closed);
                            break;
                        }
                    }
                }
            }

            ready_for_task.store(false, Ordering::Relaxed);
        });

        Ok((
            Self {
                outbound_tx,
                shutdown_tx,
                ready,
            },
            event_rx,
        ))
    }
}
