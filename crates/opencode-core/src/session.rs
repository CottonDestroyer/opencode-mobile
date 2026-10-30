//! Sessions, messages and parts, persisted as JSON at
//! `<data>/storage/session/<project>/<session>.json`.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn total_context(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }
    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.reasoning += o.reasoning;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ToolState {
    Pending,
    Running,
    Completed {
        output: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        metadata: Value,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<FilePart>,
    },
    Error {
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FilePart {
    pub mime: String,
    /// Base64-encoded content.
    pub data: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Part {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        synthetic: bool,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        /// Opaque payload: Anthropic's redacted thinking or OpenAI's encrypted reasoning content.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redacted: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
    },
    Tool {
        id: String,
        name: String,
        input: Value,
        state: ToolState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_meta: Option<Value>,
    },
    File(FilePart),
    /// A compaction point: everything before it is replaced by `summary`.
    Compaction {
        summary: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub parts: Vec<Part>,
    pub time: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub cost: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Message {
    pub fn new(role: Role) -> Self {
        Message {
            id: crate::util::id("msg"),
            role,
            parts: vec![],
            time: crate::util::now_ms(),
            completed: None,
            model: None,
            agent: None,
            usage: Usage::default(),
            cost: 0.0,
            finish: None,
            error: None,
        }
    }
    pub fn user_text(text: impl Into<String>) -> Self {
        let mut m = Message::new(Role::User);
        m.parts.push(Part::Text { text: text.into(), synthetic: false });
        m
    }
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    pub fn tool_calls(&self) -> impl Iterator<Item = &Part> {
        self.parts.iter().filter(|p| matches!(p, Part::Tool { .. }))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Todo {
    pub content: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub project: String,
    pub directory: PathBuf,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub todos: Vec<Todo>,
    /// Permission rules the user approved with "always" during this session.
    #[serde(default)]
    pub approved: Vec<(String, String)>,
    /// Instruction files already injected, e.g. found while reading subdirectories.
    #[serde(default)]
    pub loaded_instructions: Vec<String>,
}

pub const DEFAULT_TITLE_PREFIX: &str = "New session - ";

impl Session {
    pub fn new(project: &str, directory: &Path, parent: Option<String>) -> Self {
        let now = crate::util::now_ms();
        let prefix = if parent.is_some() { "Child session - " } else { DEFAULT_TITLE_PREFIX };
        Session {
            id: crate::util::id_desc("ses"),
            project: project.to_string(),
            directory: directory.to_path_buf(),
            title: format!("{prefix}{}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S")),
            parent_id: parent,
            time_created: now,
            time_updated: now,
            agent: None,
            model: None,
            messages: vec![],
            todos: vec![],
            approved: vec![],
            loaded_instructions: vec![],
        }
    }
    pub fn has_default_title(&self) -> bool {
        self.title.starts_with(DEFAULT_TITLE_PREFIX) || self.title.starts_with("Child session - ")
    }
    pub fn usage_total(&self) -> (Usage, f64) {
        let mut u = Usage::default();
        let mut cost = 0.0;
        for m in &self.messages {
            u.add(&m.usage);
            cost += m.cost;
        }
        (u, cost)
    }
    /// The messages after the most recent compaction, the compaction message included.
    pub fn active_messages(&self) -> &[Message] {
        let idx = self
            .messages
            .iter()
            .rposition(|m| m.parts.iter().any(|p| matches!(p, Part::Compaction { .. })))
            .unwrap_or(0);
        &self.messages[idx..]
    }
    pub fn last_assistant(&self) -> Option<&Message> {
        self.messages.iter().rev().find(|m| m.role == Role::Assistant)
    }
}

/// Stable identifier for a project directory (its worktree root).
pub fn project_id(worktree: &Path) -> String {
    let s = crate::util::normalize(worktree).to_string_lossy().to_lowercase();
    // FNV-1a.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// The TypeScript client's project id for `directory`, sent as `x-opencode-project`. Inside git it is
/// the SHA-1 of the origin remote, else `.git/opencode`, else the root commit, else `global`; outside
/// git, the SHA-1 of the directory. Cached per directory.
pub fn upstream_project_id(directory: &Path) -> String {
    static CACHE: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, String>>> = std::sync::Mutex::new(None);
    if let Some(id) = CACHE.lock().unwrap().get_or_insert_with(Default::default).get(directory) {
        return id.clone();
    }
    let id = compute_upstream_project_id(directory);
    CACHE.lock().unwrap().get_or_insert_with(Default::default).insert(directory.to_path_buf(), id.clone());
    id
}

fn sha1_hex(s: &str) -> String {
    sha1_smol::Sha1::from(s).digest().to_string()
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).current_dir(dir).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `host/path` of a git remote URL, normalized the way upstream does it.
fn remote_key(url: &str) -> Option<String> {
    let url = url.trim();
    let join = |host: &str, path: &str| {
        let path = path.trim_start_matches('/');
        let path = path.strip_suffix(".git/").or_else(|| path.strip_suffix(".git")).unwrap_or(path).trim_end_matches('/');
        (!host.is_empty() && !path.is_empty()).then(|| format!("{}/{path}", host.to_lowercase()))
    };
    if let Ok(u) = url::Url::parse(url) {
        if u.scheme() == "file" {
            return None;
        }
        return join(u.host_str().unwrap_or(""), u.path());
    }
    let re = regex::Regex::new(r"^([^@/:]+@)?([^/:]+):(.+)$").unwrap();
    let c = re.captures(url)?;
    join(&c[2], &c[3])
}

fn compute_upstream_project_id(directory: &Path) -> String {
    let Some(git_dir) = directory.ancestors().map(|a| a.join(".git")).find(|p| p.exists()) else {
        return sha1_hex(&format!("directory:{}", directory.display()));
    };
    let root = git_dir.parent().unwrap_or(directory);
    let Some(out) = git(root, &["rev-parse", "--git-dir", "--git-common-dir", "--show-toplevel"]) else {
        return sha1_hex(&format!("directory:{}", directory.display()));
    };
    let mut lines = out.lines();
    let (_, common, top) = (lines.next(), lines.next().unwrap_or(""), lines.next().unwrap_or(""));
    let worktree = if top.is_empty() { root.to_path_buf() } else { root.join(top) };
    if let Some(key) = git(&worktree, &["remote", "get-url", "origin"]).as_deref().and_then(remote_key) {
        return sha1_hex(&format!("git-remote:{key}"));
    }
    if let Ok(saved) = std::fs::read_to_string(root.join(common).join("opencode")) {
        if !saved.trim().is_empty() {
            return saved.trim().to_string();
        }
    }
    let mut roots: Vec<String> = git(&worktree, &["rev-list", "--max-parents=0", "HEAD"])
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    roots.sort();
    roots.into_iter().next().unwrap_or_else(|| "global".into())
}

fn dir(project: &str) -> PathBuf {
    crate::paths::get().storage().join("session").join(project)
}

fn file(project: &str, id: &str) -> PathBuf {
    dir(project).join(format!("{id}.json"))
}

pub fn save(s: &Session) -> Result<()> {
    let d = dir(&s.project);
    std::fs::create_dir_all(&d)?;
    let tmp = d.join(format!("{}.json.tmp", s.id));
    std::fs::write(&tmp, serde_json::to_vec(s)?)?;
    std::fs::rename(&tmp, file(&s.project, &s.id))?;
    Ok(())
}

pub fn load(project: &str, id: &str) -> Result<Session> {
    let p = file(project, id);
    if p.is_file() {
        return Ok(serde_json::from_slice(&std::fs::read(p)?)?);
    }
    // Search every project.
    let root = crate::paths::get().storage().join("session");
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            let p = e.path().join(format!("{id}.json"));
            if p.is_file() {
                return Ok(serde_json::from_slice(&std::fs::read(p)?)?);
            }
        }
    }
    Err(anyhow!("session not found: {id}"))
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub id: String,
    pub title: String,
    pub parent_id: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
    pub directory: PathBuf,
}

/// Lightweight session listing, newest first.
pub fn list(project: &str) -> Vec<Summary> {
    #[derive(Deserialize)]
    struct Head {
        id: String,
        title: String,
        #[serde(default)]
        parent_id: Option<String>,
        time_created: i64,
        time_updated: i64,
        directory: PathBuf,
    }
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir(project)) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&p) {
                if let Ok(h) = serde_json::from_slice::<Head>(&bytes) {
                    out.push(Summary {
                        id: h.id,
                        title: h.title,
                        parent_id: h.parent_id,
                        time_created: h.time_created,
                        time_updated: h.time_updated,
                        directory: h.directory,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| b.time_updated.cmp(&a.time_updated));
    out
}

/// Deletes a session along with its subagent sessions.
pub fn delete(project: &str, id: &str) -> Result<usize> {
    let all = list(project);
    let mut removed = 0;
    let mut stack = vec![id.to_string()];
    while let Some(cur) = stack.pop() {
        for s in all.iter().filter(|s| s.parent_id.as_deref() == Some(cur.as_str())) {
            stack.push(s.id.clone());
        }
        let p = file(project, &cur);
        if p.is_file() {
            std::fs::remove_file(p)?;
            removed += 1;
        }
    }
    if removed == 0 {
        return Err(anyhow!("session not found: {id}"));
    }
    Ok(removed)
}

/// Forks a session: a copy under a new id.
pub fn fork(s: &Session) -> Session {
    let mut f = s.clone();
    f.id = crate::util::id_desc("ses");
    f.time_created = crate::util::now_ms();
    f.time_updated = f.time_created;
    f.title = format!("{} (fork)", s.title);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_ids() {
        // Outside git, upstream's id is the SHA-1 of `directory:<path>`.
        assert_eq!(
            compute_upstream_project_id(Path::new("/storage/emulated/0/Documents/app")),
            "bef14afd6eab0e7d83d93dc7f126b0e334691094"
        );
        assert_eq!(remote_key("git@github.com:Acme/App.git").as_deref(), Some("github.com/Acme/App"));
        assert_eq!(remote_key("https://user@GitHub.com/acme/app.git/").as_deref(), Some("github.com/acme/app"));
        assert_eq!(remote_key("file:///tmp/repo"), None);
        let id = crate::util::id_desc("ses");
        assert!(id.starts_with("ses_") && id.len() == 30, "{id}");
        assert!(crate::util::id_desc("ses") < id, "newer session ids sort first");
        assert!(crate::util::id("msg") < crate::util::id("msg"));
    }
}
