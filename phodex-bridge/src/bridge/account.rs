use std::sync::{Arc, Mutex};
use std::time::Duration;

use color_eyre::eyre::{eyre, Result};
use serde_json::{json, Value};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::account_status::compose_sanitized_auth_status;
use crate::desktop::handle_desktop_request;
use crate::git_handler::handle_git_request;
use crate::json_rpc::{error_response, success_response};
use crate::package_version_status::BridgePackageVersionStatusReader;
use crate::push::handle_notifications_request;
use crate::rollout::thread_context_read;
use crate::voice::handle_voice_request;
use crate::workspace::handle_workspace_request;

use super::support::{error_code_for_method, read_string};
use super::{BridgeRuntime, BridgeTask, CodexRequestClient, PendingAuthLogin};

impl BridgeRuntime {
    pub(super) fn spawn_bridge_request(
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
}

impl CodexRequestClient {
    pub(super) async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
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

pub(super) async fn handle_account_request(
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

pub(super) async fn read_sanitized_auth_status(
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

pub(super) async fn handle_thread_context_request(params: &Value) -> Result<Value> {
    let thread_id = read_string(params.get("threadId"))
        .or_else(|| read_string(params.get("thread_id")))
        .ok_or_else(|| eyre!("thread/contextWindow/read requires a threadId."))?;
    let turn_id = read_string(params.get("turnId")).or_else(|| read_string(params.get("turn_id")));
    Ok(thread_context_read(&thread_id, turn_id.as_deref()))
}
