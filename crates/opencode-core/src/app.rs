//! Shared application context for one project directory: config, providers, agents, sessions, the
//! event bus the UI subscribes to, and interactive permission prompts.

use crate::agent::Agent;
use crate::config::{Config, Loaded};
use crate::models::Provider;
use crate::permission::{self, Effect, Reply};
use crate::session::{self, Message, Session};
use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub enum AppEvent {
    Updated { session: String },
    TextDelta { session: String, delta: String },
    ReasoningDelta { session: String, delta: String },
    ToolStarted { session: String, id: String, name: String, input: serde_json::Value },
    ToolFinished { session: String, id: String, name: String, title: String, output: String, error: Option<String>, metadata: serde_json::Value },
    StepFinished { session: String },
    Busy { session: String, busy: bool },
    Error { session: String, message: String },
    Retry { session: String, message: String },
    Permission(permission::Request),
    PermissionResolved { id: String },
    Title { session: String },
    Notice { message: String },
}

impl AppEvent {
    pub fn session(&self) -> Option<&str> {
        match self {
            AppEvent::Updated { session }
            | AppEvent::TextDelta { session, .. }
            | AppEvent::ReasoningDelta { session, .. }
            | AppEvent::ToolStarted { session, .. }
            | AppEvent::ToolFinished { session, .. }
            | AppEvent::StepFinished { session }
            | AppEvent::Busy { session, .. }
            | AppEvent::Error { session, .. }
            | AppEvent::Retry { session, .. }
            | AppEvent::Title { session, .. } => Some(session),
            AppEvent::Permission(r) => Some(&r.session_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskMode {
    /// Routes "ask" permission decisions to the UI.
    Interactive,
    AutoApprove,
    AutoReject,
}

#[derive(Debug, Clone, Default)]
pub struct PromptOptions {
    pub agent: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
}

pub struct Handle {
    pub session: Mutex<Session>,
    pub cancel: Mutex<Option<CancellationToken>>,
    pub busy: AtomicBool,
    pub queue: Mutex<VecDeque<(Message, PromptOptions)>>,
}

impl Handle {
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
    pub fn id(&self) -> String {
        self.session.lock().unwrap().id.clone()
    }
    pub fn with<R>(&self, f: impl FnOnce(&mut Session) -> R) -> R {
        let mut s = self.session.lock().unwrap();
        f(&mut s)
    }
    pub fn save(&self) {
        let mut s = self.session.lock().unwrap();
        s.time_updated = crate::util::now_ms();
        if let Err(e) = session::save(&s) {
            eprintln!("failed to save session: {e}");
        }
    }
    pub fn abort(&self) {
        if let Some(c) = self.cancel.lock().unwrap().as_ref() {
            c.cancel();
        }
        self.queue.lock().unwrap().clear();
    }
}

pub struct App {
    pub directory: PathBuf,
    pub worktree: PathBuf,
    pub is_git: bool,
    pub project: String,
    pub config: Config,
    pub config_files: Vec<PathBuf>,
    pub providers: RwLock<BTreeMap<String, Provider>>,
    pub agents: BTreeMap<String, Agent>,
    pub skills: Vec<crate::tools::skill::Skill>,
    pub commands: BTreeMap<String, crate::command::Command>,
    pub mcp: crate::mcp::Manager,
    pub events: broadcast::Sender<AppEvent>,
    pub ask_mode: Mutex<AskMode>,
    pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
    sessions: Mutex<HashMap<String, Arc<Handle>>>,
}

impl App {
    pub async fn init(directory: PathBuf) -> Result<Arc<App>> {
        Self::init_with_mcp(directory, BTreeMap::new()).await
    }

    /// Like [`App::init`], with additional MCP servers merged into the configured ones.
    pub async fn init_with_mcp(directory: PathBuf, extra_mcp: BTreeMap<String, crate::config::McpConfig>) -> Result<Arc<App>> {
        crate::paths::get().ensure();
        let directory = std::fs::canonicalize(&directory).unwrap_or(directory);
        let found = crate::config::find_worktree(&directory);
        let is_git = found.is_some();
        let worktree = found.unwrap_or_else(|| directory.clone());
        let Loaded { mut config, files, directories } = crate::config::load(&directory, &worktree)?;
        config.mcp.extend(extra_mcp);
        let providers = crate::models::providers(&config);
        let agents = crate::agent::load(&config, &directories);
        let skills = crate::tools::skill::discover(&config, &directories, &directory);
        let commands = crate::command::load(&config, &directories);
        let mcp = crate::mcp::Manager::new(config.mcp.clone(), directory.clone());
        let (events, _) = broadcast::channel(4096);
        let app = Arc::new(App {
            project: session::project_id(&worktree),
            directory,
            worktree,
            is_git,
            config,
            config_files: files,
            providers: RwLock::new(providers),
            agents,
            skills,
            commands,
            mcp,
            events,
            ask_mode: Mutex::new(AskMode::Interactive),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        });
        Ok(app)
    }

    pub fn emit(&self, ev: AppEvent) {
        let _ = self.events.send(ev);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.events.subscribe()
    }

    /// The provider registry, borrowed rather than copied (it can hold thousands of models). Don't
    /// hold the guard across anything that may rebuild the registry.
    pub fn providers(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Provider>> {
        self.providers.read().unwrap()
    }

    pub fn agent(&self, id: &str) -> Result<&Agent> {
        self.agents.get(id).ok_or_else(|| anyhow!("agent not found: {id}"))
    }

    pub fn default_agent(&self) -> String {
        crate::agent::default_agent(&self.config, &self.agents)
    }

    pub fn primary_agents(&self) -> Vec<&Agent> {
        let mut v: Vec<&Agent> = self.agents.values().filter(|a| a.is_primary() && !a.hidden).collect();
        v.sort_by_key(|a| (a.id != "build", a.id.clone()));
        v
    }

    pub fn default_model(&self) -> Option<String> {
        crate::models::default_model(&self.config, &self.providers.read().unwrap())
    }

    fn insert(&self, s: Session) -> Arc<Handle> {
        let id = s.id.clone();
        let h = Arc::new(Handle {
            session: Mutex::new(s),
            cancel: Mutex::new(None),
            busy: AtomicBool::new(false),
            queue: Mutex::new(VecDeque::new()),
        });
        self.sessions.lock().unwrap().insert(id, h.clone());
        h
    }

    pub fn create_session(&self, parent: Option<String>) -> Arc<Handle> {
        let h = self.new_session(parent);
        h.save();
        h
    }

    /// A session that stays in memory until its first message is saved.
    pub fn new_session(&self, parent: Option<String>) -> Arc<Handle> {
        self.insert(Session::new(&self.project, &self.directory, parent))
    }

    pub fn session(&self, id: &str) -> Result<Arc<Handle>> {
        if let Some(h) = self.sessions.lock().unwrap().get(id) {
            return Ok(h.clone());
        }
        let s = session::load(&self.project, id)?;
        Ok(self.insert(s))
    }

    pub fn forget_session(&self, id: &str) {
        self.sessions.lock().unwrap().remove(id);
    }

    /// Evaluates the agent's and session's permission rules for an action, prompting when they say "ask".
    pub async fn authorize(
        &self,
        handle: &Handle,
        agent: &Agent,
        tool_call_id: Option<&str>,
        action: &str,
        resources: &[String],
        save: &[String],
        title: String,
        metadata: serde_json::Value,
    ) -> Result<()> {
        let mut rules = permission::defaults();
        if let Some(p) = &self.config.permission {
            rules.extend(permission::from_config(p));
        }
        rules.extend(agent.permissions.iter().cloned());
        let approved: Vec<(String, String)> = handle.with(|s| s.approved.clone());
        rules.extend(approved.iter().map(|(a, r)| permission::rule(a, r, Effect::Allow)));

        let mut needs_ask = Vec::new();
        for r in resources {
            match permission::evaluate(&rules, action, r) {
                Effect::Allow => {}
                Effect::Deny => {
                    return Err(anyhow!(
                        "Permission denied: the {action} permission for {r} is denied by the configured rules. Do not retry; continue without it or ask the user."
                    ))
                }
                Effect::Ask => needs_ask.push(r.clone()),
            }
        }
        if needs_ask.is_empty() {
            return Ok(());
        }
        let mode = *self.ask_mode.lock().unwrap();
        match mode {
            AskMode::AutoApprove => return Ok(()),
            AskMode::AutoReject => {
                return Err(anyhow!(
                    "Permission rejected: {action} on {} requires approval, which is unavailable in this mode.",
                    needs_ask.join(", ")
                ))
            }
            AskMode::Interactive => {}
        }
        let session_id = handle.id();
        let req = permission::Request {
            id: crate::util::id("per"),
            session_id,
            tool_call_id: tool_call_id.map(String::from),
            action: action.to_string(),
            resources: needs_ask.clone(),
            title,
            metadata,
            save: if save.is_empty() { needs_ask.clone() } else { save.to_vec() },
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(req.id.clone(), tx);
        let save = req.save.clone();
        let id = req.id.clone();
        self.emit(AppEvent::Permission(req));
        let reply = rx.await.unwrap_or(Reply::Reject);
        self.emit(AppEvent::PermissionResolved { id });
        match reply {
            Reply::Once => Ok(()),
            Reply::Always => {
                handle.with(|s| {
                    for p in &save {
                        s.approved.push((action.to_string(), p.clone()));
                    }
                });
                handle.save();
                Ok(())
            }
            Reply::Reject => Err(anyhow!(crate::engine::REJECTED)),
        }
    }

    pub fn reply_permission(&self, id: &str, reply: Reply) {
        if let Some(tx) = self.pending.lock().unwrap().remove(id) {
            let _ = tx.send(reply);
        }
    }

    pub fn reject_all_pending(&self, session_id: &str) {
        let _ = session_id;
        let pending: Vec<_> = self.pending.lock().unwrap().drain().collect();
        for (_, tx) in pending {
            let _ = tx.send(Reply::Reject);
        }
    }
}
