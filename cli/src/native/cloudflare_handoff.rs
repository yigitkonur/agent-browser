//! Cloudflare Live View and structured human handoff over the active CDP page.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::task::JoinHandle;

use super::browser::BrowserManager;

const HANDOFF_TIMEOUT_MS: u64 = 600_000;

/// Shared progress is updated by a task so the daemon can answer other requests.
pub struct Handoff {
    status: Arc<Mutex<Value>>,
    task: JoinHandle<()>,
}

impl Handoff {
    pub fn status(&self) -> Value {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn active(&self) -> bool {
        self.status()["status"] == "active"
    }

    /// Keep automation paused until a successful handoff's profile is saved.
    pub fn blocks_actions(&self) -> bool {
        let status = self.status();
        status["status"] == "active"
            || status["status"] == "unknown"
            || (status["status"] == "done" && status["acknowledged"] != true)
    }

    pub fn acknowledge(&self) {
        self.status.lock().unwrap_or_else(|e| e.into_inner())["acknowledged"] = json!(true);
    }

    /// A failed request may have reached Cloudflare; require an explicit state check.
    pub fn uncertain(client: Arc<super::cdp::client::CdpClient>) -> Self {
        let status = Arc::new(Mutex::new(json!({"status": "unknown"})));
        let task = tokio::spawn(keep_unknown_alive(client, status.clone()));
        Self { status, task }
    }

    pub fn resolve_unknown(&self) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) =
            json!({"status": "ended", "reason": "Handoff result could not be recovered"});
    }
}

async fn keep_unknown_alive(client: Arc<super::cdp::client::CdpClient>, status: Arc<Mutex<Value>>) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(HANDOFF_TIMEOUT_MS);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if status.lock().unwrap_or_else(|e| e.into_inner())["status"] != "unknown" {
            break;
        }
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            client.send_command_no_params("Browser.getVersion", None),
        )
        .await;
    }
}

impl Drop for Handoff {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn live_view(browser: &BrowserManager) -> Result<Value, String> {
    let session = browser.active_session_id()?;
    let reply = browser
        .client
        .send_command(
            "Cloudflare.getLiveView",
            Some(json!({"mode": "tab"})),
            Some(session),
        )
        .await?;
    let url = reply
        .get("devtoolsFrontendUrl")
        .and_then(Value::as_str)
        .filter(|url| url.starts_with("https://live.browser.run/"))
        .ok_or("Cloudflare did not return a valid Live View URL")?;
    Ok(json!({"url": url}))
}

pub async fn start(
    browser: &BrowserManager,
    instructions: &str,
) -> Result<(Handoff, Value), String> {
    if instructions.trim().is_empty() || instructions.len() > 4096 {
        return Err("Handoff instructions must contain 1 to 4096 characters".into());
    }
    let client = browser.client.clone();
    // Use a dedicated page CDP session so private screencast routing on the
    // active page cannot consume the handoff completion event.
    let attached = client
        .send_command(
            "Target.attachToTarget",
            Some(json!({
                "targetId": browser.active_target_id()?, "flatten": true
            })),
            None,
        )
        .await?;
    let session = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("Cloudflare handoff could not attach to the active tab")?
        .to_string();
    let mut events = client.subscribe_methods(&["Cloudflare.handoffComplete"]);
    let view = match live_view(browser).await {
        Ok(view) => view,
        Err(error) => {
            let _ = client
                .send_command(
                    "Target.detachFromTarget",
                    Some(json!({"sessionId": session})),
                    None,
                )
                .await;
            return Err(error);
        }
    };
    let result = match client
        .send_command(
            "Cloudflare.handoff",
            Some(json!({
                "instructions": instructions, "timeout": HANDOFF_TIMEOUT_MS
            })),
            Some(&session),
        )
        .await
    {
        Ok(result) => result,
        Err(error) => {
            let _ = client
                .send_command(
                    "Target.detachFromTarget",
                    Some(json!({"sessionId": session})),
                    None,
                )
                .await;
            return Err(error);
        }
    };
    let id = match result
        .get("handoffId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        Some(id) => id.to_string(),
        None => {
            let _ = client
                .send_command(
                    "Target.detachFromTarget",
                    Some(json!({"sessionId": session})),
                    None,
                )
                .await;
            return Err("Cloudflare did not return a handoff ID".into());
        }
    };
    let status = Arc::new(Mutex::new(json!({"status": "active", "handoffId": id})));
    let status_task = status.clone();
    let id_task = id.clone();
    let task = tokio::spawn(async move {
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(HANDOFF_TIMEOUT_MS + 5_000);
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        ticker.tick().await;
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) if event.session_id.as_deref() == Some(session.as_str())
                        && event.params.get("handoffId").and_then(Value::as_str).is_none_or(|value| value == id_task) => {
                        let success = event.params.get("success").and_then(Value::as_bool).unwrap_or(false);
                        *status_task.lock().unwrap_or_else(|e| e.into_inner()) = json!({
                            "status": if success {"done"} else {"failed"}, "handoffId": id_task,
                            "reason": event.params.get("reason")
                        });
                        break;
                    }
                    Some(_) => {},
                    None => {
                        *status_task.lock().unwrap_or_else(|e| e.into_inner()) = json!({"status": "unknown", "reason":"event stream closed", "handoffId": id_task});
                        break;
                    }
                },
                _ = ticker.tick() => {
                    if client.send_command_no_params("Browser.getVersion", None).await.is_err() {
                        *status_task.lock().unwrap_or_else(|e| e.into_inner()) = json!({"status": "unknown", "reason":"CDP connection lost", "handoffId": id_task});
                        break;
                    }
                },
                _ = tokio::time::sleep_until(deadline) => {
                    *status_task.lock().unwrap_or_else(|e| e.into_inner()) = json!({"status": "unknown", "reason":"completion event timed out", "handoffId": id_task});
                    break;
                }
            }
        }
        let _ = client
            .send_command(
                "Target.detachFromTarget",
                Some(json!({"sessionId": session})),
                None,
            )
            .await;
        let unknown = status_task.lock().unwrap_or_else(|e| e.into_inner())["status"] == "unknown";
        if unknown {
            keep_unknown_alive(client, status_task).await;
        }
    });
    Ok((
        Handoff { status, task },
        json!({"status":"active", "handoffId":id, "url":view["url"]}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_and_unsaved_success_block_automation() {
        let handoff = Handoff {
            status: Arc::new(Mutex::new(json!({"status":"unknown"}))),
            task: tokio::spawn(std::future::pending()),
        };
        assert!(handoff.blocks_actions());
        handoff.resolve_unknown();
        assert!(!handoff.blocks_actions());
        *handoff.status.lock().unwrap() = json!({"status":"done"});
        assert!(handoff.blocks_actions());
        handoff.acknowledge();
        assert!(!handoff.blocks_actions());
    }
}
