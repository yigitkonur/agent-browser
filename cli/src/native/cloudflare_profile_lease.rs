//! An OS-held, per-profile lease preventing multiple fork-owned browsers.
//!
//! The lease file is never unlinked: removing a lock file while another process
//! has it open permits two independent inodes and defeats flock. Its contents
//! are a durable allocation marker, not a token or profile state.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

#[derive(Debug, Serialize, Deserialize)]
struct Allocation {
    account: String,
    session_id: Option<String>,
}

#[derive(Debug)]
pub(super) struct ProfileLease {
    file: File,
}

#[cfg(unix)]
fn open_locked(path: &Path) -> Result<File, String> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "Unable to open the Cloudflare profile lease")?;
    if !file
        .metadata()
        .map_err(|_| "Unable to inspect the Cloudflare profile lease")?
        .is_file()
    {
        return Err("Cloudflare profile lease is not a regular file".into());
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err("This Cloudflare profile already has an active browser in another agent-browser session. Reuse its --session instead of opening another browser.".into());
        }
        return Err("Unable to acquire the Cloudflare profile lease".into());
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_locked(_path: &Path) -> Result<File, String> {
    Err("Shared Cloudflare profiles require an OS-backed lock, which is not available on this platform; use CLOUDFLARE_PROFILE_MODE=none for an isolated session".into())
}

fn profile_name() -> Result<String, String> {
    let profile = env::var("CLOUDFLARE_PROFILE").unwrap_or_else(|_| "main".into());
    let bytes = profile.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
        || !bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(*b, b'.' | b'_' | b'-')
        })
    {
        return Err("CLOUDFLARE_PROFILE must start with a lowercase letter or digit and contain at most 64 lowercase letters, digits, dots, underscores or hyphens".into());
    }
    Ok(profile)
}

fn profile_home(mode: &str) -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or("Cannot locate the Cloudflare profile home")?;
    let raw = env::var_os("CFB_HOME").filter(|s| !s.is_empty());
    let path = match raw {
        None => home.join(".cf-browser"),
        Some(raw) if raw == "~" => home,
        Some(raw) => {
            let path = PathBuf::from(raw);
            if let Ok(rest) = path.strip_prefix("~") {
                home.join(rest)
            } else if path.is_absolute() {
                path
            } else {
                env::current_dir()
                    .map_err(|_| "Cannot resolve CFB_HOME")?
                    .join(path)
            }
        }
    };
    if mode != "read" {
        fs::create_dir_all(&path).map_err(|_| "Cannot access the Cloudflare profile home")?;
    }
    path.canonicalize()
        .map_err(|_| "Cannot normalize the Cloudflare profile home".into())
}

pub(super) fn acquire(mode: &str) -> Result<Option<ProfileLease>, String> {
    if mode == "none" {
        return Ok(None);
    }
    let profile = profile_name()?;
    let home = profile_home(mode)?;
    preflight_at(&home, &profile, mode)?;
    acquire_at(&home, &profile).map(Some)
}

fn preflight_at(home: &Path, profile: &str, mode: &str) -> Result<(), String> {
    if mode == "read"
        && !fs::metadata(home.join("profiles").join(profile).join("state.json"))
            .is_ok_and(|m| m.is_file())
    {
        return Err(format!("Cloudflare profile '{profile}' has no saved state; create or sign in through the Cloudflare plugin before using read mode"));
    }
    Ok(())
}

fn acquire_at(home: &Path, profile: &str) -> Result<ProfileLease, String> {
    let dir = home.join(".agent-browser-leases");
    fs::create_dir_all(&dir).map_err(|_| "Cannot create Cloudflare profile lease directory")?;
    if fs::symlink_metadata(&dir)
        .map_err(|_| "Cannot inspect Cloudflare profile lease directory")?
        .file_type()
        .is_symlink()
    {
        return Err("Cloudflare profile lease directory cannot be a symbolic link".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|_| "Cannot secure Cloudflare profile lease directory")?;
    }
    let name = format!("{}.lock", hex::encode(Sha256::digest(profile.as_bytes())));
    Ok(ProfileLease {
        file: open_locked(&dir.join(name))?,
    })
}

impl ProfileLease {
    fn allocation(&mut self) -> Result<Option<Allocation>, String> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|_| "Cannot read Cloudflare profile lease")?;
        let mut bytes = Vec::new();
        (&mut self.file)
            .take(4097)
            .read_to_end(&mut bytes)
            .map_err(|_| "Cannot read Cloudflare profile lease")?;
        if bytes.is_empty() {
            return Ok(None);
        }
        if bytes.len() > 4096 {
            return Err(
                "Cloudflare profile lease is invalid; refusing to allocate a browser".into(),
            );
        }
        serde_json::from_slice(&bytes).map(Some).map_err(|_| {
            "Cloudflare profile lease is invalid; refusing to allocate a browser".into()
        })
    }
    fn write(&mut self, value: Option<&Allocation>) -> Result<(), String> {
        let bytes = value
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| "Cannot encode Cloudflare profile lease")?
            .unwrap_or_default();
        // If writing fails or the process crashes, a malformed nonempty marker
        // blocks future creation. Never treat malformed state as unlocked.
        self.file
            .seek(SeekFrom::Start(0))
            .and_then(|_| self.file.write_all(&bytes))
            .and_then(|_| self.file.set_len(bytes.len() as u64))
            .and_then(|_| self.file.sync_all())
            .map_err(|_| "Cannot persist Cloudflare profile lease".into())
    }
    pub(super) async fn recover(
        &mut self,
        client: &reqwest::Client,
        account: &str,
        token: &str,
    ) -> Result<(), String> {
        let Some(allocation) = self.allocation()? else {
            return Ok(());
        };
        if allocation.account != account {
            return Err("This profile has an unconfirmed browser in a different Cloudflare account. Use the original account to clean up before opening another browser.".into());
        }
        let session = allocation.session_id.ok_or("A previous Cloudflare browser creation has an unknown outcome. Check the Cloudflare dashboard and do not create another session until the uncertainty is resolved.")?;
        if !super::valid_session_id(&session) {
            return Err("Cloudflare profile lease contains an invalid session ID".into());
        }
        super::close_remote(client, &format!("{}/browser/{session}", super::base(account)), token, &session).await
            .map_err(|_| format!("The previous browser for this profile ({session}) could not be confirmed closed; retry with the same account before opening another."))?;
        self.write(None)
    }
    pub(super) fn mark_pending(&mut self, account: &str) -> Result<(), String> {
        self.write(Some(&Allocation {
            account: account.into(),
            session_id: None,
        }))
    }
    pub(super) fn mark_session(&mut self, account: &str, session: &str) -> Result<(), String> {
        self.write(Some(&Allocation {
            account: account.into(),
            session_id: Some(session.into()),
        }))
    }
    pub(super) fn clear(&mut self) -> Result<(), String> {
        self.write(None)
    }
}

fn registry() -> &'static Mutex<HashMap<String, ProfileLease>> {
    static ACTIVE: OnceLock<Mutex<HashMap<String, ProfileLease>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(account: &str, session: &str) -> String {
    format!("{account}/{session}")
}

pub(super) fn hold(account: &str, session: &str, lease: Option<ProfileLease>) {
    if let Some(lease) = lease {
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key(account, session), lease);
    }
}

/// Preserve a failed-to-attach allocation for an explicit cleanup retry.
pub(super) fn mark_recovery(account: &str, session: &str) {
    let mut active = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(lease) = active.remove(&key(account, session)) {
        active.insert(key(account, "recovery"), lease);
    }
}

/// Called only after the remote session has been confirmed closed.
pub(super) fn release(account: &str, session: &str) -> Result<(), String> {
    let mut active = registry().lock().unwrap_or_else(|e| e.into_inner());
    let direct = key(account, session);
    if let Some(lease) = active.get_mut(&direct) {
        lease.write(None)?;
        active.remove(&direct);
    } else {
        let recovery = key(account, "recovery");
        if let Some(lease) = active.get_mut(&recovery) {
            let matches = lease.allocation()?.is_some_and(|entry| {
                entry.account == account && entry.session_id.as_deref() == Some(session)
            });
            if matches {
                lease.write(None)?;
                active.remove(&recovery);
            }
        }
    }
    Ok(())
}

/// Retry a failed pre-allocation recovery without dropping its OS lease.
pub(super) async fn retry_held(
    client: &reqwest::Client,
    account: &str,
    token: &str,
) -> Result<Option<&'static str>, String> {
    let held = {
        let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
        ["recovery", "unknown"]
            .into_iter()
            .find_map(|name| map.remove(&key(account, name)).map(|lease| (name, lease)))
    };
    let Some((name, mut lease)) = held else {
        return Ok(None);
    };
    match lease.recover(client, account, token).await {
        Ok(()) => Ok(Some("profile recovery confirmed")),
        Err(error) => {
            hold(account, name, Some(lease));
            Err(error)
        }
    }
}

/// Explicit recovery after a human has inspected the Cloudflare dashboard.
/// Known session IDs cannot be cleared this way: they must be confirmed closed.
pub(super) fn clear_unknown(account: &str, confirm: bool) -> Result<(), String> {
    if !confirm {
        return Err("Confirm that the Cloudflare dashboard has no orphan browser before clearing the unknown allocation marker".into());
    }
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    let held_key = ["unknown", "recovery"]
        .into_iter()
        .map(|s| key(account, s))
        .find(|k| map.contains_key(k));
    let mut lease = if let Some(held_key) = held_key.as_ref() {
        map.remove(held_key)
            .expect("held lease disappeared while locked")
    } else {
        drop(map);
        let profile = profile_name()?;
        let home = profile_home("read")?;
        let mut lease = acquire_at(&home, &profile)?;
        clear_unknown_marker(&mut lease, account)?;
        return Ok(());
    };
    let result = clear_unknown_marker(&mut lease, account);
    if result.is_err() {
        map.insert(held_key.expect("held lease key"), lease);
    }
    result
}

fn clear_unknown_marker(lease: &mut ProfileLease, account: &str) -> Result<(), String> {
    let Some(allocation) = lease.allocation()? else {
        return Ok(());
    };
    if allocation.account != account {
        return Err("The unknown allocation belongs to a different Cloudflare account".into());
    }
    if allocation.session_id.is_some() {
        return Err("This profile has a known session ID; retry close in its owning session or use Cloudflare cleanup after that daemon exits".into());
    }
    lease.clear()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn lease_is_exclusive_by_profile_and_releases_on_drop() {
        let home = tempfile::tempdir().unwrap();
        let first = acquire_at(home.path(), "main").unwrap();
        assert!(acquire_at(home.path(), "main")
            .unwrap_err()
            .contains("active browser"));
        let other = acquire_at(home.path(), "other").unwrap();
        drop(first);
        assert!(acquire_at(home.path(), "main").is_ok());
        drop(other);
    }
    #[cfg(unix)]
    #[test]
    fn persisted_pending_allocation_survives_process_lock_release() {
        let home = tempfile::tempdir().unwrap();
        let mut first = acquire_at(home.path(), "main").unwrap();
        first.mark_pending("account").unwrap();
        drop(first);
        let mut next = acquire_at(home.path(), "main").unwrap();
        let marker = next.allocation().unwrap().unwrap();
        assert_eq!(marker.account, "account");
        assert_eq!(marker.session_id, None);
        next.mark_session("account", "test-123").unwrap();
        next.write(None).unwrap();
        assert!(next.allocation().unwrap().is_none());
    }
    #[test]
    fn read_mode_requires_a_saved_profile_before_allocation() {
        let home = tempfile::tempdir().unwrap();
        assert!(preflight_at(home.path(), "main", "read")
            .unwrap_err()
            .contains("no saved state"));
        let profile = home.path().join("profiles/main");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("state.json"), "fixture").unwrap();
        assert!(preflight_at(home.path(), "main", "read").is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn clearing_unknown_requires_matching_account_and_never_clears_known_session() {
        assert!(clear_unknown("account", false).is_err());
        let home = tempfile::tempdir().unwrap();
        let mut lease = acquire_at(home.path(), "main").unwrap();
        lease.mark_pending("account").unwrap();
        assert!(clear_unknown_marker(&mut lease, "other").is_err());
        assert!(lease.allocation().unwrap().is_some());
        lease.mark_session("account", "test-123").unwrap();
        assert!(clear_unknown_marker(&mut lease, "account").is_err());
        assert!(lease.allocation().unwrap().is_some());
        lease.mark_pending("account").unwrap();
        clear_unknown_marker(&mut lease, "account").unwrap();
        assert!(lease.allocation().unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn child_profile_lock_probe() {
        let Some(home) = std::env::var_os("AGENT_BROWSER_TEST_LEASE_HOME") else {
            return;
        };
        let expect_blocked = std::env::var_os("AGENT_BROWSER_TEST_LEASE_BLOCKED").is_some();
        let result = acquire_at(Path::new(&home), "main");
        assert_eq!(result.is_err(), expect_blocked);
    }

    #[cfg(unix)]
    #[test]
    fn profile_lease_excludes_other_processes() {
        let home = tempfile::tempdir().unwrap();
        let lease = acquire_at(home.path(), "main").unwrap();
        let probe = |blocked: bool| {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.arg("--exact").arg("native::providers::cloudflare_provider::profile_lease::tests::child_profile_lock_probe")
                .arg("--test-threads=1").env("AGENT_BROWSER_TEST_LEASE_HOME", home.path());
            if blocked {
                child.env("AGENT_BROWSER_TEST_LEASE_BLOCKED", "1");
            } else {
                child.env_remove("AGENT_BROWSER_TEST_LEASE_BLOCKED");
            }
            assert!(child.status().unwrap().success());
        };
        probe(true);
        drop(lease);
        probe(false);
    }
}
