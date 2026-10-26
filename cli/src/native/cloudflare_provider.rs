//! Managed Cloudflare Browser Run sessions.
//!
//! Only sessions acquired by this provider are recorded and eligible for
//! recovery. The journal never contains the API token or WebSocket URL.
use super::{ProviderConnection, ProviderSession};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{env, fs, io::Write, path::PathBuf, time::Duration};

const API: &str = "https://api.cloudflare.com/client/v4/accounts";
const DEADLINE: Duration = Duration::from_secs(15);
const RECOVERY_DEADLINE: Duration = Duration::from_secs(10);
const RETRY_DEADLINE: Duration = Duration::from_secs(60);
const MAX_BODY: usize = 64 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct OwnedSession {
    account: String,
    session_id: String,
    pid: u32,
}

fn credentials() -> Result<(String, String), String> {
    let account =
        env::var("CLOUDFLARE_ACCOUNT_ID").map_err(|_| "CLOUDFLARE_ACCOUNT_ID is required")?;
    let token = env::var("CLOUDFLARE_API_TOKEN").map_err(|_| "CLOUDFLARE_API_TOKEN is required")?;
    if account.is_empty() || !account.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return Err("CLOUDFLARE_ACCOUNT_ID must contain only letters and digits".into());
    }
    let header = format!("Bearer {token}");
    if token.is_empty()
        || !token.bytes().all(|b| b.is_ascii_graphic())
        || reqwest::header::HeaderValue::from_str(&header).is_err()
    {
        return Err("CLOUDFLARE_API_TOKEN cannot be used as an HTTP authorization header".into());
    }
    Ok((account, token))
}

fn duration_setting(name: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
    let value = match env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|_| format!("{name} must be an integer between {min} and {max}"))?,
        Err(env::VarError::NotPresent) => default,
        Err(_) => return Err(format!("{name} is not valid UTF-8")),
    };
    if !(min..=max).contains(&value) {
        return Err(format!("{name} must be between {min} and {max}"));
    }
    Ok(value)
}

pub(crate) fn idle_and_max() -> Result<(u64, u64), String> {
    Ok((
        duration_setting("CLOUDFLARE_IDLE_TIMEOUT_MS", 300_000, 60_000, 1_140_000)?,
        duration_setting("CLOUDFLARE_MAX_SESSION_MS", 1_800_000, 300_000, 14_400_000)?,
    ))
}

fn keep_alive(idle_ms: u64) -> Result<u64, String> {
    let value = match env::var("CLOUDFLARE_KEEP_ALIVE_MS") {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|_| "CLOUDFLARE_KEEP_ALIVE_MS must be an integer between 10000 and 1200000")?,
        Err(env::VarError::NotPresent) => idle_ms + 60_000,
        Err(_) => return Err("CLOUDFLARE_KEEP_ALIVE_MS is not valid UTF-8".into()),
    };
    if !(10_000..=1_200_000).contains(&value) {
        return Err("CLOUDFLARE_KEEP_ALIVE_MS must be between 10000 and 1200000".into());
    }
    if value < idle_ms {
        return Err("CLOUDFLARE_KEEP_ALIVE_MS must be at least CLOUDFLARE_IDLE_TIMEOUT_MS".into());
    }
    Ok(value)
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(DEADLINE)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Could not initialize Cloudflare HTTP client".into())
}

fn base(account: &str) -> String {
    format!("{API}/{account}/browser-run/devtools")
}
fn journal_dir() -> PathBuf {
    super::super::state::get_state_dir().join("cloudflare-owned")
}
fn journal_path(account: &str, session: &str) -> PathBuf {
    journal_dir().join(format!(
        "{}.json",
        hex::encode(Sha256::digest(format!("{account}/{session}").as_bytes()))
    ))
}
fn valid_session_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
fn save_journal(account: &str, session_id: &str) -> Result<(), String> {
    let dir = journal_dir();
    fs::create_dir_all(&dir).map_err(|_| "Unable to create Cloudflare ownership journal")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|_| "Unable to secure Cloudflare ownership journal")?;
    }
    let entry = OwnedSession {
        account: account.into(),
        session_id: session_id.into(),
        pid: std::process::id(),
    };
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(journal_path(account, session_id))
        .map_err(|_| "Unable to record Cloudflare session ownership")?;
    let bytes =
        serde_json::to_vec(&entry).map_err(|_| "Unable to encode Cloudflare ownership record")?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "Unable to persist Cloudflare session ownership".into())
}
fn remove_journal(account: &str, session_id: &str) -> Result<(), String> {
    match fs::remove_file(journal_path(account, session_id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => {
            Err("Cloudflare session closed but its ownership record could not be removed".into())
        }
    }
}

async fn read_response(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Cloudflare response could not be read or timed out")?
    {
        if out.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err("Cloudflare response exceeded size limit".into());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn status_error(response: &reqwest::Response, action: &str) -> String {
    let status = response.status();
    let retry = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 10 && v.bytes().all(|c| c.is_ascii_digit()));
    match retry {
        Some(retry) if status == reqwest::StatusCode::TOO_MANY_REQUESTS => {
            format!("Cloudflare {action} failed (HTTP {status}); Retry-After: {retry}")
        }
        _ => format!("Cloudflare {action} failed (HTTP {status})"),
    }
}

fn valid_ws_url(ws: &str, account: &str, session: &str) -> bool {
    let expected = format!("/client/v4/accounts/{account}/browser-run/devtools/browser/{session}");
    url::Url::parse(ws).is_ok_and(|url| {
        url.scheme() == "wss"
            && url.host_str() == Some("api.cloudflare.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == expected
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

async fn close_at(
    client: &reqwest::Client,
    account: &str,
    token: &str,
    session: &str,
) -> Result<(), String> {
    if !valid_session_id(session) {
        return Err("Invalid Cloudflare session ID".into());
    }
    let endpoint = format!("{}/browser/{session}", base(account));
    close_remote(client, &endpoint, token, session).await?;
    remove_journal(account, session)
}

async fn close_remote(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    session: &str,
) -> Result<(), String> {
    let response = client
        .delete(endpoint)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| {
            format!("Cloudflare session {session} deletion failed or timed out; retry cleanup")
        })?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    if !response.status().is_success() {
        return Err(status_error(&response, "session deletion"));
    }
    // DELETE can return `closing`, so verify that the documented target-list
    // endpoint no longer exists before clearing ownership.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let check = tokio::time::timeout(
            remaining,
            client
                .get(format!("{endpoint}/json/list"))
                .bearer_auth(token)
                .send(),
        )
        .await;
        match check {
            Ok(Ok(response)) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                return Ok(())
            }
            Ok(Ok(response)) if response.status().is_success() => {}
            _ => break,
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!("Cloudflare accepted closure of session {session}, but closure is unconfirmed; retry cleanup"))
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // EPERM also means the process exists, so only ESRCH denotes a dead PID.
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}
#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true
}

async fn recover(client: &reqwest::Client, account: &str, token: &str) -> Result<(), String> {
    let entries = match fs::read_dir(journal_dir()) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("Unable to read Cloudflare ownership journal".into()),
    };
    for entry in entries.flatten() {
        let Ok(data) = fs::read(entry.path()) else {
            continue;
        };
        if data.len() > 4096 {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<OwnedSession>(&data) else {
            continue;
        };
        if record.account != account
            || !valid_session_id(&record.session_id)
            || process_alive(record.pid)
            || entry.path() != journal_path(account, &record.session_id)
        {
            continue;
        }
        close_at(client, account, token, &record.session_id)
            .await
            .map_err(|e| format!("Previous Cloudflare session recovery failed: {e}"))?;
    }
    Ok(())
}

async fn bounded_recovery<F>(operation: F, deadline: Duration) -> Result<(), String>
where
    F: std::future::Future<Output = Result<(), String>>,
{
    tokio::time::timeout(deadline, operation).await.map_err(|_| {
        "Cloudflare previous-session recovery timed out; records were retained for unconfirmed closures; run cloudflare cleanup".to_string()
    })?
}

/// Enforce max age without acquiring the daemon state lock, which can be held
/// by a long-running command. The journal is checked immediately before the
/// request so a session already closed normally is not touched.
fn spawn_max_age_watchdog(
    client: reqwest::Client,
    endpoint: String,
    token: String,
    record_path: PathBuf,
    account: String,
    session: String,
    deadline: tokio::time::Instant,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let still_owned = || {
            fs::read(&record_path)
                .ok()
                .filter(|bytes| bytes.len() <= 4096)
                .and_then(|bytes| serde_json::from_slice::<OwnedSession>(&bytes).ok())
                .is_some_and(|record| {
                    record.account == account
                        && record.session_id == session
                        && record.pid == std::process::id()
                })
        };
        loop {
            if !still_owned() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
        if !still_owned() {
            return;
        }
        match close_remote(&client, &endpoint, &token, &session).await {
            Ok(()) => {
                // A normal close may have removed this concurrently. If the
                // record changed, leave it untouched for its new owner.
                if still_owned() {
                    if let Err(error) = fs::remove_file(&record_path) {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            eprintln!("Cloudflare session {session} closed but its ownership record could not be removed");
                        }
                    }
                }
            }
            Err(error) => eprintln!("Cloudflare maximum-age cleanup failed: {error}"),
        }
    })
}

pub async fn connect() -> Result<ProviderConnection, String> {
    crate::native::cloudflare_profile::validate_mode()?;
    let (account, token) = credentials()?;
    let (idle_ms, max_ms) = idle_and_max()?;
    let keep_alive = keep_alive(idle_ms)?;
    let client = client()?;
    bounded_recovery(recover(&client, &account, &token), RECOVERY_DEADLINE).await?;
    let acquisition_started = tokio::time::Instant::now();
    let json = create_remote(
        &client,
        &format!("{}/browser", base(&account)),
        &token,
        keep_alive,
    )
    .await?;
    let session = json
        .get("sessionId")
        .and_then(|v| v.as_str())
        .filter(|s| valid_session_id(s))
        .ok_or(
            "Cloudflare creation response lacks a valid session ID; check the Cloudflare dashboard",
        )?;
    if let Err(e) = save_journal(&account, session) {
        let cleanup = close_at(&client, &account, &token, session).await;
        return Err(format!(
            "{e}; session {session} cleanup: {}",
            cleanup.err().unwrap_or_else(|| "confirmed".into())
        ));
    }
    std::mem::drop(spawn_max_age_watchdog(
        client.clone(),
        format!("{}/browser/{session}", base(&account)),
        token.clone(),
        journal_path(&account, session),
        account.clone(),
        session.to_string(),
        acquisition_started + Duration::from_millis(max_ms),
    ));
    let ws = json
        .get("webSocketDebuggerUrl")
        .and_then(|v| v.as_str())
        .filter(|ws| valid_ws_url(ws, &account, session));
    let Some(ws) = ws else {
        let cleanup = close_at(&client, &account, &token, session).await;
        return Err(format!(
            "Cloudflare returned an invalid CDP URL for session {session}; cleanup: {}",
            cleanup.err().unwrap_or_else(|| "confirmed".into())
        ));
    };
    Ok(ProviderConnection {
        ws_url: ws.into(),
        ws_headers: Some(vec![("Authorization".into(), format!("Bearer {token}"))]),
        session: Some(ProviderSession {
            provider: "cloudflare".into(),
            session_id: session.into(),
        }),
        direct_page: false,
        metadata: None,
    })
}

async fn create_remote(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    keep_alive: u64,
) -> Result<serde_json::Value, String> {
    let response = client.post(endpoint).query(&[("keep_alive", keep_alive)])
        .bearer_auth(&token).send().await.map_err(|_| "Cloudflare session creation failed or timed out; its outcome may be unknown, check the Cloudflare dashboard before retrying")?;
    if !response.status().is_success() {
        return Err(status_error(&response, "session creation"));
    }
    let body = read_response(response).await.map_err(|e| {
        format!("{e}; creation outcome may be unknown, check the Cloudflare dashboard")
    })?;
    serde_json::from_slice(&body).map_err(|_| "Invalid Cloudflare creation response; session outcome may be unknown, check the Cloudflare dashboard".into())
}

pub async fn close(session_id: &str) -> Result<(), String> {
    let (account, token) = credentials()?;
    close_at(&client()?, &account, &token, session_id).await
}

/// Inspect only this account's local ownership records, without fetching or
/// listing any unrelated account sessions.
pub(crate) async fn status_owned() -> Result<serde_json::Value, String> {
    let (account, _) = credentials()?;
    let records = owned_records(&account)?;
    Ok(
        serde_json::json!({ "sessions": records.iter().map(|r| serde_json::json!({
        "sessionId": r.session_id, "ownerPid": r.pid,
        "ownerAlive": process_alive(r.pid), "state": "recorded; remote status unconfirmed"
    })).collect::<Vec<_>>() }),
    )
}

/// Retry deletion only for records whose owning process is no longer alive.
pub(crate) async fn retry_owned() -> Result<serde_json::Value, String> {
    let (account, token) = credentials()?;
    let records = owned_records(&account)?;
    let client = client()?;
    let operation = async {
        let mut results = Vec::new();
        for record in records {
            if process_alive(record.pid) {
                results.push(serde_json::json!({"sessionId": record.session_id, "result": "owner is alive; skipped"}));
                continue;
            }
            match close_at(&client, &account, &token, &record.session_id).await {
            Ok(()) => results.push(serde_json::json!({"sessionId": record.session_id, "result": "closed"})),
            Err(error) => results.push(serde_json::json!({"sessionId": record.session_id, "result": "unconfirmed", "error": error})),
        }
        }
        Ok(serde_json::json!({"sessions": results}))
    };
    tokio::time::timeout(RETRY_DEADLINE, operation).await.map_err(|_| "Cloudflare cleanup timed out; unconfirmed sessions remain in the ownership journal, run cloudflare sessions and retry cleanup".to_string())?
}

fn owned_records(account: &str) -> Result<Vec<OwnedSession>, String> {
    let entries = match fs::read_dir(journal_dir()) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("Unable to read Cloudflare ownership journal".into()),
    };
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let Ok(data) = fs::read(entry.path()) else {
            continue;
        };
        if data.len() > 4096 {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<OwnedSession>(&data) else {
            continue;
        };
        if record.account == account
            && valid_session_id(&record.session_id)
            && entry.path() == journal_path(account, &record.session_id)
        {
            records.push(record);
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn recovery_has_an_overall_deadline() {
        let error = bounded_recovery(
            async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(())
            },
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(error.contains("recovery timed out") && error.contains("records were retained"));
    }
    #[test]
    fn websocket_url_must_be_cloudflare_session_and_not_contain_credentials() {
        let account = "abcdef";
        let session = "1234-abcd";
        assert!(valid_ws_url(&format!("wss://api.cloudflare.com/client/v4/accounts/{account}/browser-run/devtools/browser/{session}"), account, session));
        assert!(!valid_ws_url(&format!("wss://evil.example/client/v4/accounts/{account}/browser-run/devtools/browser/{session}"), account, session));
        assert!(!valid_ws_url(&format!("wss://api.cloudflare.com/client/v4/accounts/{account}/browser-run/devtools/browser/{session}?token=secret"), account, session));
        assert!(!valid_session_id("../../x"));
    }

    async fn mock_server(
        status: &str,
        headers: &str,
        body: &str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/browser", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 8192];
            let n = socket.read(&mut bytes).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&bytes[..n]).into_owned()
        });
        (endpoint, task)
    }

    #[tokio::test]
    async fn creation_sends_query_and_bearer_header() {
        let (endpoint, server) = mock_server(
            "200 OK",
            "Content-Type: application/json\r\n",
            r#"{"sessionId":"test-123"}"#,
        )
        .await;
        let json = create_remote(&client().unwrap(), &endpoint, "test-secret", 360_000)
            .await
            .unwrap();
        assert_eq!(json["sessionId"], "test-123");
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("post /browser?keep_alive=360000 "));
        assert!(request.contains("authorization: bearer test-secret\r\n"));
        assert!(!request.contains("{\"keep_alive\""));
    }

    #[tokio::test]
    async fn rate_limit_reports_retry_after_without_echoing_body() {
        let (endpoint, server) = mock_server(
            "429 Too Many Requests",
            "Retry-After: 20\r\n",
            "secret-response-body",
        )
        .await;
        let error = create_remote(&client().unwrap(), &endpoint, "test-secret", 360_000)
            .await
            .unwrap_err();
        assert!(error.contains("429") && error.contains("Retry-After: 20"));
        assert!(!error.contains("secret"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn creation_rejects_oversized_response() {
        let body = "x".repeat(MAX_BODY + 1);
        let (endpoint, server) = mock_server("200 OK", "", &body).await;
        let error = create_remote(&client().unwrap(), &endpoint, "test-secret", 360_000)
            .await
            .unwrap_err();
        assert!(error.contains("size limit") && !error.contains("test-secret"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn delete_waits_for_absence_after_closing_acknowledgement() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/browser/test-123", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for status in ["200 OK", "404 Not Found"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 8192];
                let n = socket.read(&mut buf).await.unwrap();
                requests.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            requests
        });
        close_remote(&client().unwrap(), &endpoint, "test-secret", "test-123")
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("DELETE /browser/test-123 "));
        assert!(requests[1].starts_with("GET /browser/test-123/json/list "));
    }

    #[tokio::test]
    async fn delete_failure_does_not_report_success_or_echo_token() {
        let (endpoint, server) = mock_server("401 Unauthorized", "", "test-secret").await;
        let error = close_remote(&client().unwrap(), &endpoint, "test-secret", "test-123")
            .await
            .unwrap_err();
        assert!(error.contains("401"));
        assert!(!error.contains("test-secret"));
        assert!(server.await.unwrap().starts_with("DELETE /browser "));
    }

    fn test_record(path: &std::path::Path, account: &str, session: &str) {
        fs::write(
            path,
            serde_json::to_vec(&OwnedSession {
                account: account.into(),
                session_id: session.into(),
                pid: std::process::id(),
            })
            .unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn watchdog_closes_owned_session_without_daemon_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owned.json");
        test_record(&path, "account", "test-123");
        let (endpoint, server) = mock_server("404 Not Found", "", "").await;
        let task = spawn_max_age_watchdog(
            client().unwrap(),
            endpoint,
            "test-secret".into(),
            path.clone(),
            "account".into(),
            "test-123".into(),
            tokio::time::Instant::now() + Duration::from_millis(10),
        );
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(!path.exists());
        assert!(server.await.unwrap().starts_with("DELETE /browser "));
    }

    #[tokio::test]
    async fn watchdog_does_not_close_if_ownership_is_already_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owned.json");
        let task = spawn_max_age_watchdog(
            client().unwrap(),
            "http://127.0.0.1:1/browser".into(),
            "test-secret".into(),
            path,
            "account".into(),
            "test-123".into(),
            tokio::time::Instant::now() + Duration::from_millis(10),
        );
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn watchdog_preserves_journal_when_remote_cleanup_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owned.json");
        test_record(&path, "account", "test-123");
        let (endpoint, server) = mock_server("401 Unauthorized", "", "").await;
        let task = spawn_max_age_watchdog(
            client().unwrap(),
            endpoint,
            "test-secret".into(),
            path.clone(),
            "account".into(),
            "test-123".into(),
            tokio::time::Instant::now() + Duration::from_millis(10),
        );
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(path.exists());
        server.await.unwrap();
    }
}
