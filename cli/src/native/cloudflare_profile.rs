//! Private JSONL bridge to the Cloudflare Playwright profile companion.
use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

pub struct ProfileCompanion {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
    pub mode: String,
}

fn companion_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("AGENT_BROWSER_CLOUDFLARE_COMPANION") {
        return Ok(path.into());
    }
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../packages/cloudflare-profile/companion.mjs");
    if source.is_file() {
        return Ok(source);
    }
    let executable =
        std::env::current_exe().map_err(|_| "Cannot locate Cloudflare profile companion")?;
    for dir in executable.ancestors() {
        let candidate = dir.join("packages/cloudflare-profile/companion.mjs");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err("Cloudflare profile companion is missing; set AGENT_BROWSER_CLOUDFLARE_COMPANION to its path".into())
}

impl ProfileCompanion {
    async fn spawn(mode: String) -> Result<Self, String> {
        let mut child = Command::new("node").arg(companion_path()?).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().map_err(|_| "Could not start the Cloudflare profile companion; install Node.js and the companion dependencies")?;
        let input = child.stdin.take().ok_or("Could not open companion input")?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or("Could not open companion output")?,
        );
        Ok(Self {
            child,
            input,
            output,
            next_id: 1,
            mode,
        })
    }

    pub async fn start(ws_url: &str, session_id: &str) -> Result<(Self, String), String> {
        let mode = validate_mode()?;
        let token = std::env::var("CLOUDFLARE_API_TOKEN")
            .map_err(|_| "CLOUDFLARE_API_TOKEN is required")?;
        let profile = std::env::var("CLOUDFLARE_PROFILE").unwrap_or_else(|_| "main".into());
        let mut companion = Self::spawn(mode.clone()).await?;
        let result = companion.request("open", json!({ "wsUrl":ws_url, "token":token, "sessionId":session_id, "profile":profile, "mode":mode, "compatibilityVerified":false })).await?;
        let context_id = result
            .get("contextId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or("Cloudflare profile companion returned no context ID")?
            .to_owned();
        Ok((companion, context_id))
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut body = serde_json::to_vec(&json!({"id":id, "method":method, "params":params}))
            .map_err(|_| "Could not encode companion request")?;
        body.push(b'\n');
        let operation = async {
            self.input
                .write_all(&body)
                .await
                .map_err(|_| "Cloudflare profile companion input failed")?;
            self.input
                .flush()
                .await
                .map_err(|_| "Cloudflare profile companion input failed")?;
            let mut line = String::new();
            let count = self
                .output
                .read_line(&mut line)
                .await
                .map_err(|_| "Cloudflare profile companion output failed")?;
            if count == 0 || count > 64 * 1024 {
                return Err("Cloudflare profile companion returned an invalid response".to_string());
            }
            let response: Value = serde_json::from_str(&line)
                .map_err(|_| "Cloudflare profile companion returned an invalid response")?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                return Err("Cloudflare profile companion response ID mismatch".into());
            }
            if response.get("ok").and_then(Value::as_bool) != Some(true) {
                let code = response
                    .pointer("/error/code")
                    .and_then(Value::as_str)
                    .unwrap_or("COMPANION_FAILED");
                return Err(format!("Cloudflare profile operation failed ({code})"));
            }
            Ok(response.get("result").cloned().unwrap_or(Value::Null))
        };
        tokio::time::timeout(Duration::from_secs(45), operation)
            .await
            .map_err(|_| "Cloudflare profile companion timed out".to_string())?
    }

    pub async fn save(&mut self) -> Result<(), String> {
        self.request("save", Value::Null).await.map(|_| ())
    }
    pub async fn close_context(&mut self) -> Result<(), String> {
        self.request("close", Value::Null).await.map(|_| ())
    }
    pub async fn release(&mut self) -> Result<(), String> {
        self.request("release", Value::Null).await.map(|_| ())
    }
    pub async fn terminate(&mut self) {
        let _ = self.child.kill().await;
    }
}

pub async fn manage(method: &str, params: Value) -> Result<Value, String> {
    if !matches!(method, "clone" | "restore") {
        return Err("Unknown Cloudflare profile operation".into());
    }
    let mut companion = ProfileCompanion::spawn("none".into()).await?;
    let result = companion.request(method, params).await;
    companion.terminate().await;
    result
}

pub fn validate_mode() -> Result<String, String> {
    let mode = std::env::var("CLOUDFLARE_PROFILE_MODE").unwrap_or_else(|_| "write".into());
    if !matches!(mode.as_str(), "write" | "read" | "none") {
        return Err("CLOUDFLARE_PROFILE_MODE must be write, read, or none".into());
    }
    // This is a release gate, not an environment-controlled override.
    if mode == "write" {
        return Err("Cloudflare shared-profile writes are disabled until live compatibility is verified; use CLOUDFLARE_PROFILE_MODE=read or none".into());
    }
    Ok(mode)
}
