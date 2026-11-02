//! User preferences, persisted as JSON in `<data>/mobile.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub current_dir: Option<PathBuf>,
    pub recent_dirs: Vec<PathBuf>,
    pub collapsed: Vec<String>,
    /// 0 = follow the system, 1 = light, 2 = dark.
    pub theme: i32,
    pub lock_enabled: bool,
    pub lock_send: bool,
    pub notify_finished: bool,
    pub notify_permission: bool,
    pub notify_error: bool,
    pub expand_thinking: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            current_dir: None,
            recent_dirs: vec![],
            collapsed: vec![],
            theme: 0,
            lock_enabled: false,
            lock_send: false,
            notify_finished: true,
            notify_permission: true,
            notify_error: true,
            expand_thinking: false,
        }
    }
}

fn file() -> PathBuf {
    opencode_core::paths::get().data.join("mobile.json")
}

impl Prefs {
    pub fn load() -> Prefs {
        std::fs::read_to_string(file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = file();
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default());
    }

    pub fn push_recent(&mut self, dir: &PathBuf) {
        self.recent_dirs.retain(|d| d != dir);
        self.recent_dirs.insert(0, dir.clone());
        self.recent_dirs.truncate(10);
    }
}
