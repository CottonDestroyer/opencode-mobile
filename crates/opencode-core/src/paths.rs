//! XDG-style directories, laid out like the TypeScript client's so the two can share data.

use std::path::PathBuf;
use std::sync::OnceLock;

const APP: &str = "opencode";

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub tmp: PathBuf,
    pub log: PathBuf,
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn get() -> &'static Paths {
    static PATHS: OnceLock<Paths> = OnceLock::new();
    PATHS.get_or_init(|| {
        let home = env_dir("OPENCODE_TEST_HOME")
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let data = env_dir("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local").join("share")).join(APP);
        let cache = env_dir("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache")).join(APP);
        let config = env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")).join(APP);
        let state = env_dir("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local").join("state")).join(APP);
        let tmp = std::env::temp_dir().join(APP);
        let log = data.join("log");
        Paths { home, data, cache, config, state, tmp, log }
    })
}

impl Paths {
    pub fn ensure(&self) {
        for d in [&self.data, &self.cache, &self.config, &self.state, &self.tmp, &self.log] {
            let _ = std::fs::create_dir_all(d);
        }
    }
    pub fn auth_file(&self) -> PathBuf {
        self.data.join("auth.json")
    }
    pub fn storage(&self) -> PathBuf {
        self.data.join("storage")
    }
    pub fn tool_output(&self) -> PathBuf {
        self.data.join("tool-output")
    }
    pub fn models_cache(&self) -> PathBuf {
        self.cache.join("models.json")
    }
    pub fn state_file(&self) -> PathBuf {
        self.state.join("state.json")
    }
}
