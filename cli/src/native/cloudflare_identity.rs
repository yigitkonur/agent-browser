//! Verify that a client does not silently reuse a daemon with different Cloudflare settings.
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const SETTINGS: &[(&str, &str, Option<&str>)] = &[
    ("CLOUDFLARE_ACCOUNT_ID", "account", None),
    ("CLOUDFLARE_API_TOKEN", "token", None),
    ("CLOUDFLARE_PROFILE", "profile", Some("main")),
    ("CLOUDFLARE_PROFILE_MODE", "mode", Some("read")),
    ("CFB_HOME", "profile directory", Some("~/.cf-browser")),
];

fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

/// Only explicitly supplied settings need to match: a bare follow-up command
/// can safely use the already-running daemon without re-exporting credentials.
pub fn client_settings() -> Value {
    let mut result = Map::new();
    for (name, label, _) in SETTINGS {
        if let Ok(value) = std::env::var(name) {
            result.insert((*label).into(), Value::String(digest(&value)));
        }
    }
    Value::Object(result)
}

pub fn matches_daemon(client: &Value) -> bool {
    let Some(values) = client.as_object() else {
        return false;
    };
    values.iter().all(|(label, fingerprint)| {
        SETTINGS
            .iter()
            .find(|(_, known, _)| *known == label)
            .is_some_and(|(name, _, default)| {
                std::env::var(name)
                    .ok()
                    .or_else(|| default.map(str::to_owned))
                    .is_some_and(|value| fingerprint.as_str() == Some(digest(&value).as_str()))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::EnvGuard;
    use serde_json::json;

    #[test]
    fn explicit_settings_detect_changed_profile_or_credentials_without_revealing_values() {
        let env = EnvGuard::new(&[
            "CLOUDFLARE_API_TOKEN",
            "CLOUDFLARE_PROFILE",
            "CLOUDFLARE_PROFILE_MODE",
        ]);
        env.set("CLOUDFLARE_API_TOKEN", "secret-original");
        env.set("CLOUDFLARE_PROFILE", "main");
        env.remove("CLOUDFLARE_PROFILE_MODE");
        let original = client_settings();
        assert!(!original.to_string().contains("secret-original"));
        assert!(matches_daemon(&original));
        assert!(matches_daemon(&json!({})));
        assert!(matches_daemon(&json!({"mode": digest("read")})));
        assert!(!matches_daemon(&json!({"mode": digest("none")})));
        env.set("CLOUDFLARE_API_TOKEN", "secret-different");
        assert!(!matches_daemon(&original));
    }
}
