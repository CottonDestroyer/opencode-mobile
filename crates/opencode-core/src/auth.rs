//! Credential storage in `<data>/auth.json`, in the format the TypeScript client uses:
//! `{ "<provider>": { "type": "api", "key": "..." } }`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Credential {
    Api {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<BTreeMap<String, String>>,
    },
    Oauth {
        refresh: String,
        access: String,
        expires: i64,
        #[serde(default, rename = "accountId", skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(default, rename = "enterpriseUrl", skip_serializing_if = "Option::is_none")]
        enterprise_url: Option<String>,
    },
    Wellknown {
        key: String,
        token: String,
    },
}

impl Credential {
    pub fn secret(&self) -> &str {
        match self {
            Credential::Api { key, .. } => key,
            Credential::Oauth { access, .. } => access,
            Credential::Wellknown { token, .. } => token,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Credential::Api { .. } => "api",
            Credential::Oauth { .. } => "oauth",
            Credential::Wellknown { .. } => "wellknown",
        }
    }
}

/// All stored credentials. `auth.json` wins over API keys the TypeScript client saved in `opencode.db`;
/// that database is read-only here, and its OAuth entries are skipped because they need refresh flows.
pub fn all() -> BTreeMap<String, Credential> {
    let mut out = from_database(&crate::paths::get().data.join("opencode.db"));
    out.extend(from_file());
    out
}

fn from_file() -> BTreeMap<String, Credential> {
    let Ok(text) = std::fs::read_to_string(crate::paths::get().auth_file()) else {
        return BTreeMap::new();
    };
    let raw: BTreeMap<String, serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
    raw.into_iter().filter_map(|(k, v)| serde_json::from_value(v).ok().map(|c| (k.trim_end_matches('/').to_string(), c))).collect()
}

pub fn from_database(path: &std::path::Path) -> BTreeMap<String, Credential> {
    let mut out = BTreeMap::new();
    if !path.is_file() {
        return out;
    }
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(db) = rusqlite::Connection::open_with_flags(path, flags) else { return out };
    let _ = db.busy_timeout(std::time::Duration::from_millis(500));
    // Prefer active credentials; older schemas have no `active` column.
    let stmt = db
        .prepare("SELECT integration_id, value FROM credential ORDER BY coalesce(active, 0), time_updated")
        .or_else(|_| db.prepare("SELECT integration_id, value FROM credential ORDER BY time_updated"));
    let Ok(mut stmt) = stmt else { return out };
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
    for (id, value) in rows.into_iter().flatten().flatten() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&value) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("key") {
            continue;
        }
        if let Some(key) = v.get("key").and_then(|k| k.as_str()).filter(|k| !k.is_empty()) {
            out.insert(id.trim_end_matches('/').to_string(), Credential::Api { key: key.to_string(), metadata: None });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_api_keys_from_database() {
        let path = std::env::temp_dir().join(format!("opencode-auth-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE credential (id text PRIMARY KEY, integration_id text, label text NOT NULL, value text NOT NULL, connector_id text, method_id text, active integer, time_created integer NOT NULL, time_updated integer NOT NULL);
                 INSERT INTO credential VALUES ('1', 'anthropic', 'API key', '{\"type\":\"key\",\"key\":\"sk-active\"}', NULL, NULL, 1, 1, 1);
                 INSERT INTO credential VALUES ('3', 'anthropic', 'old key', '{\"type\":\"key\",\"key\":\"sk-inactive\"}', NULL, NULL, 0, 1, 5);
                 INSERT INTO credential VALUES ('2', 'openai', 'OAuth', '{\"type\":\"oauth\",\"access\":\"a\",\"refresh\":\"r\",\"expires\":0}', NULL, NULL, 1, 1, 1);",
            )
            .unwrap();
        }
        let creds = super::from_database(&path);
        assert_eq!(creds.get("anthropic").map(|c| c.secret()), Some("sk-active"));
        assert!(!creds.contains_key("openai"));
        let _ = std::fs::remove_file(&path);
    }
}

pub fn get(provider: &str) -> Option<Credential> {
    all().remove(provider)
}

fn write(map: &BTreeMap<String, Credential>) -> Result<()> {
    let path = crate::paths::get().auth_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(map)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn set(provider: &str, cred: Credential) -> Result<()> {
    let mut map = from_file();
    map.insert(provider.to_string(), cred);
    write(&map)
}

pub fn remove(provider: &str) -> Result<bool> {
    let mut map = from_file();
    let existed = map.remove(provider).is_some();
    write(&map)?;
    Ok(existed)
}
