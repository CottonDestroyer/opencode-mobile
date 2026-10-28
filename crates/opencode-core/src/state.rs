//! Small persisted client state: recent and favorite models.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub recent_models: Vec<String>,
    pub favorite_models: Vec<String>,
    pub show_thinking: bool,
    pub show_details: Option<bool>,
}

pub fn get() -> State {
    std::fs::read_to_string(crate::paths::get().state_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn update(f: impl FnOnce(&mut State)) {
    let mut s = get();
    f(&mut s);
    let path = crate::paths::get().state_file();
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(path, serde_json::to_string_pretty(&s).unwrap_or_default());
}

pub fn push_recent_model(model: &str) {
    update(|s| {
        s.recent_models.retain(|m| m != model);
        s.recent_models.insert(0, model.to_string());
        s.recent_models.truncate(10);
    });
}
