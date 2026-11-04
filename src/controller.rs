//! The UI controller. All UI state lives on the Slint thread in a thread-local `Ctl`; background work
//! (engine turns, file IO, platform calls) runs on tokio and hands results back through `post`.

use crate::platform::{self, Dictation};
use crate::prefs::Prefs;
use crate::render::{self, fmt_tokens};
use crate::*;
use base64::Engine as _;
use opencode_core::app::{App, AppEvent, Handle, PromptOptions};
use opencode_core::auth::{self, Credential};
use opencode_core::permission::{self, Reply};
use opencode_core::session::{self, FilePart, Message, Part, Role, ToolState};
use opencode_core::{engine, mention, models};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

thread_local! {
    static CTL: RefCell<Option<Ctl>> = const { RefCell::new(None) };
}

/// Runs `f` against the controller. UI thread only; re-entrant calls are dropped.
pub fn with<R>(f: impl FnOnce(&mut Ctl) -> R) -> Option<R> {
    CTL.with(|c| {
        let mut b = c.try_borrow_mut().ok()?;
        b.as_mut().map(f)
    })
}

/// Runs `f` against the controller on the UI thread, from any thread.
pub fn post(f: impl FnOnce(&mut Ctl) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        with(f);
    });
}

type AppWaiter = Box<dyn FnOnce(&mut Ctl, Arc<App>)>;

struct Attachment {
    mime: String,
    data: String,
    name: String,
    image: Image,
}

struct Chat {
    app: Arc<App>,
    handle: Arc<Handle>,
    /// Current step (0..=3) while the scripted demo is showing.
    demo: Option<usize>,
    demo_perm: bool,
    agent: String,
    model: Option<String>,
    variant: Option<String>,
    attachments: Vec<Attachment>,
    /// Index of the user message being edited; it and everything after it are hidden.
    revert: Option<usize>,
    show: usize,
    error: Option<String>,
    retry: Option<String>,
    parent: Option<Box<Chat>>,
}

impl Chat {
    fn sid(&self) -> String {
        self.handle.id()
    }
}

const PAGE: usize = 40;
const AGENT_COLORS: [&str; 5] = ["#8b5cf6", "#3b82f6", "#10b981", "#f59e0b", "#ec4899"];

const BUILTIN_COMMANDS: [(&str, &str); 7] = [
    ("new", "Start a new session"),
    ("models", "Choose a different model"),
    ("agents", "Cycle to the next agent"),
    ("variant", "Cycle reasoning effort"),
    ("compact", "Summarize the conversation to save context"),
    ("export", "Export the conversation to markdown"),
    ("thinking", "Toggle expanded thinking blocks"),
];

pub struct Ctl {
    ui: AppWindow,
    rt: tokio::runtime::Handle,
    prefs: Prefs,
    dir: PathBuf,
    apps: HashMap<PathBuf, Arc<App>>,
    pending_apps: HashMap<PathBuf, Vec<AppWaiter>>,
    summaries: Vec<session::Summary>,
    session_dirs: HashMap<String, PathBuf>,
    busy: HashSet<String>,
    perms: Vec<(Arc<App>, permission::Request)>,
    status: HashMap<String, String>,
    tool_times: HashMap<String, (i64, Option<i64>)>,
    chat: Option<Chat>,
    render_scheduled: bool,
    messages: Rc<VecModel<MessageView>>,
    rendered: Vec<(String, u64)>,
    cache: HashMap<String, (u64, MessageView)>,
    toggled: HashSet<String>,
    images: HashMap<String, Image>,
    model_query: String,
    catalog_query: String,
    confirm: Option<Box<dyn FnOnce(&mut Ctl)>>,
    toast_seq: u64,
    dictation: Option<Dictation>,
    /// Whether the device can authenticate (biometrics or a screen lock).
    lock_available: bool,
    /// An unlock prompt is on screen; guards against stacking prompts.
    unlocking: bool,
    bottom_tick: i32,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn relative(ms: i64) -> String {
    let diff = now_ms() - ms;
    if diff < 60_000 {
        return "Just now".into();
    }
    if diff < 3_600_000 {
        return format!("{}m ago", diff / 60_000);
    }
    if diff < 86_400_000 {
        return format!("{}h ago", diff / 3_600_000);
    }
    if diff < 604_800_000 {
        return format!("{}d ago", diff / 86_400_000);
    }
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string()).unwrap_or_default()
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn short_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

/// Canonical form of a project directory, used as the `App` key (matches `App.directory`).
fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn expand(path: &str) -> PathBuf {
    PathBuf::from(opencode_core::util::expand_home(path.trim().trim_matches('"')))
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn parse_color(s: &str) -> Option<slint::Color> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(slint::Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// Re-encodes an image as JPEG, scaled to at most 1568 px (Anthropic's recommended maximum).
fn to_jpeg(img: image::DynamicImage) -> Result<(String, slint::SharedPixelBuffer<slint::Rgba8Pixel>), String> {
    let img = if img.width() > 1568 || img.height() > 1568 { img.resize(1568, 1568, image::imageops::FilterType::Lanczos3) } else { img };
    let mut jpeg = Vec::new();
    let rgb = image::DynamicImage::ImageRgb8(img.to_rgb8());
    rgb.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80)).map_err(|e| e.to_string())?;
    let thumb = img.thumbnail(160, 160).to_rgba8();
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(thumb.as_raw(), thumb.width(), thumb.height());
    Ok((base64::engine::general_purpose::STANDARD.encode(&jpeg), buf))
}

pub fn init(ui: &AppWindow, rt: tokio::runtime::Handle, start_dir: Option<PathBuf>) {
    let prefs = Prefs::load();
    let dir = start_dir
        .filter(|d| d.is_dir())
        .or_else(|| prefs.current_dir.clone().filter(|d| d.is_dir()))
        .or_else(dirs_home)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = canonical(&dir);
    let messages = Rc::new(VecModel::<MessageView>::default());
    let ctl = Ctl {
        ui: ui.clone_strong(),
        rt,
        prefs,
        dir,
        apps: HashMap::new(),
        pending_apps: HashMap::new(),
        summaries: vec![],
        session_dirs: HashMap::new(),
        busy: HashSet::new(),
        perms: vec![],
        status: HashMap::new(),
        tool_times: HashMap::new(),
        chat: None,
        render_scheduled: false,
        messages: messages.clone(),
        rendered: vec![],
        cache: HashMap::new(),
        toggled: HashSet::new(),
        images: HashMap::new(),
        model_query: String::new(),
        catalog_query: String::new(),
        confirm: None,
        toast_seq: 0,
        dictation: None,
        lock_available: false,
        unlocking: false,
        bottom_tick: 0,
    };
    let st = ui.global::<State>();
    st.set_messages(ModelRc::from(messages));
    st.set_version(format!("{} (engine {} · opencode {})", env!("CARGO_PKG_VERSION"), opencode_core::VERSION, opencode_core::UPSTREAM_VERSION).into());
    st.set_data_dir(opencode_core::paths::get().data.display().to_string().into());
    st.set_speech_available(platform::speech_available());
    CTL.with(|c| *c.borrow_mut() = Some(ctl));
    wire(ui);
    with(|c| c.start());
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `on!(actions, on_callback, |args| c => body)` registers a Slint callback that runs `body` against
/// the controller.
macro_rules! on {
    ($a:expr, $name:ident, |$($arg:ident),*| $c:ident => $body:expr) => {
        $a.$name(move |$($arg),*| {
            with(|$c: &mut Ctl| {
                $body;
            });
        })
    };
    ($a:expr, $name:ident, $c:ident => $body:expr) => {
        $a.$name(move || {
            with(|$c: &mut Ctl| {
                $body;
            });
        })
    };
}

fn wire(ui: &AppWindow) {
    let a = ui.global::<Actions>();
    on!(a, on_open_session, |id| c => c.open_session(id.to_string()));
    on!(a, on_new_session, c => c.new_session(None));
    on!(a, on_new_session_in, |p| c => c.new_session(Some(expand(&p))));
    on!(a, on_browse_new_session, c => c.browse_folder(true));
    on!(a, on_toggle_group, |d| c => c.toggle_group(d.to_string()));
    on!(a, on_rename_session, |id, t| c => c.rename_session(id.to_string(), t.to_string()));
    on!(a, on_delete_session, |id| c => c.delete_session(id.to_string()));
    on!(a, on_switch_dir, |p| c => c.switch_dir(expand(&p)));
    on!(a, on_browse_switch_dir, c => c.browse_folder(false));
    on!(a, on_refresh_sessions, c => c.refresh_sessions());
    on!(a, on_start_demo, c => c.start_demo());

    on!(a, on_back, c => c.back());
    on!(a, on_send, c => c.send());
    on!(a, on_abort, c => c.abort());
    on!(a, on_input_changed, |t| c => c.input_changed(&t));
    on!(a, on_slash_selected, |t| c => c.slash_selected(t.to_string()));
    on!(a, on_cycle_agent, |back| c => c.cycle_agent(back));
    on!(a, on_open_model_picker, c => c.open_model_picker());
    on!(a, on_model_search, |q| c => {
        c.model_query = q.to_lowercase();
        c.update_model_rows();
    });
    on!(a, on_pick_model, |v| c => c.pick_model(v.to_string()));
    on!(a, on_open_variant_picker, c => c.open_variant_picker());
    on!(a, on_pick_variant, |v| c => c.pick_variant(v.to_string()));
    on!(a, on_permission_reply, |id, r| c => c.permission_reply(id.to_string(), r));
    on!(a, on_toggle_expand, |k| c => c.toggle_expand(k.to_string()));
    on!(a, on_select_message, |id| c => c.select_message(id.to_string()));
    on!(a, on_edit_message, |id| c => c.edit_message(id.to_string()));
    on!(a, on_copy_text, |t| c => c.copy_text(t.to_string()));
    on!(a, on_undo_revert, c => c.undo_revert());
    on!(a, on_dismiss_error, c => {
        if let Some(ch) = &mut c.chat {
            ch.error = None;
        }
        c.update_chrome();
    });
    on!(a, on_load_older, c => c.load_more(30));
    on!(a, on_load_all, c => c.load_more(usize::MAX / 2));
    on!(a, on_attach_image, c => c.attach_image());
    on!(a, on_paste_clipboard, c => c.paste_clipboard());
    on!(a, on_remove_attachment, |i| c => {
        if let Some(ch) = &mut c.chat {
            if (i as usize) < ch.attachments.len() {
                ch.attachments.remove(i as usize);
            }
        }
        c.update_chrome();
    });
    on!(a, on_toggle_mic, c => c.toggle_mic());
    on!(a, on_open_child, |id| c => c.open_child(id.to_string()));
    on!(a, on_open_link, |u| c => c.open_url(u.to_string()));

    on!(a, on_open_add_key, c => c.open_add_key());
    on!(a, on_catalog_search, |q| c => {
        c.catalog_query = q.to_lowercase();
        c.update_catalog_rows();
    });
    on!(a, on_choose_key_provider, |id| c => c.choose_key_provider(id.to_string()));
    on!(a, on_save_key, |id, k| c => c.save_key(id.to_string(), k.to_string()));
    on!(a, on_remove_key, |id| c => c.remove_key(id.to_string()));

    on!(a, on_settings_changed, c => c.settings_changed());
    on!(a, on_lock_now, c => c.ui.global::<State>().set_locked(true));
    on!(a, on_unlock, c => c.unlock());
    on!(a, on_open_url, |u| c => c.open_url(u.to_string()));
    // The data directory is app-private, so no file manager can open it; copy its path instead.
    on!(a, on_open_data_dir, c => c.copy_text(opencode_core::paths::get().data.display().to_string()));
    on!(a, on_confirm_accepted, c => {
        if let Some(f) = c.confirm.take() {
            f(c);
        }
    });
}

impl Ctl {
    fn st(&self) -> State<'_> {
        self.ui.global::<State>()
    }

    fn start(&mut self) {
        let st = self.st();
        st.set_theme_pref(self.prefs.theme);
        self.ui.global::<Theme>().set_preference(self.prefs.theme);
        st.set_lock_enabled(self.prefs.lock_enabled);
        st.set_lock_send(self.prefs.lock_send);
        st.set_notify_finished(self.prefs.notify_finished);
        st.set_notify_permission(self.prefs.notify_permission);
        st.set_notify_error(self.prefs.notify_error);
        st.set_auto_expand_thinking(self.prefs.expand_thinking);
        st.set_locked(self.prefs.lock_enabled);
        self.update_project();
        self.refresh_sessions();
        self.ensure_app(self.dir.clone(), |c, _| c.refresh_providers());
        // Refresh the models.dev catalog in the background; failures just keep the cached copy.
        self.rt.spawn(async {
            let _ = models::refresh().await;
        });
        let want_notifications = self.prefs.notify_finished || self.prefs.notify_permission || self.prefs.notify_error;
        self.rt.spawn_blocking(move || {
            platform::setup();
            if want_notifications {
                platform::request_notification_permission();
            }
            let (ok, text) = platform::lock_status();
            post(move |c| {
                c.lock_available = ok;
                c.st().set_lock_available(ok);
                c.st().set_lock_status(text.into());
                if c.st().get_locked() {
                    if ok {
                        c.unlock();
                    } else {
                        // Verification is no longer possible; don't lock the user out.
                        c.st().set_locked(false);
                    }
                }
            });
        });
    }

    // Toasts and confirmations

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toast_seq += 1;
        let seq = self.toast_seq;
        self.st().set_toast(text.into().into());
        self.st().set_toast_error(error);
        slint::Timer::single_shot(Duration::from_millis(if error { 4500 } else { 2500 }), move || {
            with(|c| {
                if c.toast_seq == seq {
                    c.st().set_toast(SharedString::new());
                }
            });
        });
    }

    fn ask(&mut self, title: &str, text: &str, button: &str, f: impl FnOnce(&mut Ctl) + 'static) {
        self.confirm = Some(Box::new(f));
        let st = self.st();
        st.set_confirm_title(title.into());
        st.set_confirm_text(text.into());
        st.set_confirm_button(button.into());
        st.set_confirm_open(true);
    }

    fn open_url(&mut self, url: String) {
        if let Err(e) = platform::open_url(&url) {
            self.toast(format!("Couldn't open {url}: {e}"), true);
        }
    }

    // Engine apps (one per project directory)

    fn ensure_app(&mut self, dir: PathBuf, cont: impl FnOnce(&mut Ctl, Arc<App>) + 'static) {
        let key = canonical(&dir);
        if let Some(a) = self.apps.get(&key) {
            cont(self, a.clone());
            return;
        }
        let waiters = self.pending_apps.entry(key.clone()).or_default();
        let first = waiters.is_empty();
        waiters.push(Box::new(cont));
        if !first {
            return;
        }
        self.rt.spawn(async move {
            let res = App::init(key.clone()).await.map_err(|e| e.to_string());
            post(move |c| c.app_ready(key, res));
        });
    }

    fn app_ready(&mut self, key: PathBuf, res: Result<Arc<App>, String>) {
        let waiters = self.pending_apps.remove(&key).unwrap_or_default();
        match res {
            Ok(app) => {
                let mut rx = app.subscribe();
                self.rt.spawn(async move {
                    use tokio::sync::broadcast::error::RecvError;
                    loop {
                        match rx.recv().await {
                            Ok(ev) => {
                                let at = now_ms();
                                post(move |c| c.on_event(ev, at))
                            }
                            Err(RecvError::Lagged(_)) => post(|c| c.schedule_render()),
                            Err(RecvError::Closed) => break,
                        }
                    }
                });
                self.apps.insert(key, app.clone());
                for w in waiters {
                    w(self, app.clone());
                }
            }
            Err(e) => self.toast(format!("Couldn't open {}: {e}", key.display()), true),
        }
    }

    fn any_app(&self) -> Option<Arc<App>> {
        self.chat.as_ref().map(|c| c.app.clone()).or_else(|| self.apps.get(&self.dir).cloned()).or_else(|| self.apps.values().next().cloned())
    }

    // Engine events

    fn on_event(&mut self, ev: AppEvent, at: i64) {
        let chat_sid = self.chat.as_ref().map(|c| c.sid());
        let is_chat = |s: &str| chat_sid.as_deref() == Some(s);
        match ev {
            AppEvent::Busy { session, busy } => {
                if busy {
                    self.busy.insert(session.clone());
                    self.status.insert(session.clone(), "Working…".into());
                } else {
                    self.busy.remove(&session);
                    self.status.remove(&session);
                    if let Some(ch) = &mut self.chat {
                        if ch.handle.id() == session {
                            ch.retry = None;
                        }
                    }
                    if self.prefs.notify_finished && self.should_notify(&session) {
                        if let Some(title) = self.root_title(&session) {
                            platform::notify("Agent finished", &title);
                        }
                    }
                    if !self.st().get_in_chat() {
                        self.refresh_sessions();
                    }
                }
                if !self.st().get_in_chat() {
                    self.rebuild_session_rows();
                }
                if is_chat(&session) {
                    self.schedule_render();
                }
            }
            AppEvent::TextDelta { session, .. } => {
                self.status.insert(session.clone(), "Writing…".into());
                if is_chat(&session) {
                    self.schedule_render();
                }
            }
            AppEvent::ReasoningDelta { session, .. } => {
                self.status.insert(session.clone(), "Thinking…".into());
                if is_chat(&session) {
                    self.schedule_render();
                }
            }
            AppEvent::ToolStarted { session, id, name, input } => {
                self.tool_times.insert(id, (at, None));
                let base = self.chat.as_ref().map(|c| c.app.directory.clone());
                let summary = render::tool_summary(&name, &input, base.as_deref());
                let label = format!("{} {}", render::tool_label(&name), summary);
                self.status.insert(session.clone(), format!("{}…", label.trim()));
                if is_chat(&session) {
                    self.schedule_render();
                }
            }
            AppEvent::ToolFinished { session, id, .. } => {
                if let Some(t) = self.tool_times.get_mut(&id) {
                    t.1 = Some(at);
                }
                self.status.insert(session.clone(), "Working…".into());
                if is_chat(&session) {
                    self.schedule_render();
                }
            }
            AppEvent::Updated { session } | AppEvent::StepFinished { session } => {
                if is_chat(&session) {
                    self.schedule_render();
                }
            }
            AppEvent::Error { session, message } => {
                if is_chat(&session) {
                    if let Some(ch) = &mut self.chat {
                        ch.error = Some(message.clone());
                    }
                    self.schedule_render();
                }
                if self.prefs.notify_error && self.should_notify(&session) {
                    platform::notify("Session error", &message);
                }
            }
            AppEvent::Retry { session, message } => {
                self.status.insert(session.clone(), format!("Retrying: {message}"));
                if is_chat(&session) {
                    if let Some(ch) = &mut self.chat {
                        ch.retry = Some(message);
                    }
                    self.schedule_render();
                }
            }
            AppEvent::Permission(req) => {
                let app = self.apps.values().find(|a| a.session(&req.session_id).is_ok()).cloned();
                if let Some(app) = app {
                    if self.prefs.notify_permission && self.should_notify(&req.session_id) {
                        platform::notify("Permission required", &req.title);
                    }
                    self.perms.push((app, req));
                    self.schedule_render();
                }
            }
            AppEvent::PermissionResolved { id } => {
                self.perms.retain(|(_, p)| p.id != id);
                self.schedule_render();
            }
            AppEvent::Title { session } => {
                if is_chat(&session) {
                    self.schedule_render();
                }
                if !self.st().get_in_chat() {
                    self.refresh_sessions();
                }
            }
            AppEvent::Notice { message } => self.toast(message, false),
        }
    }

    fn should_notify(&self, session: &str) -> bool {
        let viewing = self.chat.as_ref().is_some_and(|c| c.sid() == session);
        !(viewing && platform::is_foreground())
    }

    /// Title of the root session for `sid`; `None` for subagent sessions.
    fn root_title(&self, sid: &str) -> Option<String> {
        for app in self.apps.values() {
            if let Ok(h) = app.session(sid) {
                return h.with(|s| if s.parent_id.is_none() { Some(s.title.clone()) } else { None });
            }
        }
        None
    }

    /// Whether `sid` is the open chat's session or one of its subagent descendants.
    fn belongs_to_chat(&self, app: &Arc<App>, sid: &str) -> bool {
        let Some(chat) = &self.chat else { return false };
        let target = chat.sid();
        let mut cur = sid.to_string();
        for _ in 0..8 {
            if cur == target {
                return true;
            }
            match app.session(&cur).ok().and_then(|h| h.with(|s| s.parent_id.clone())) {
                Some(p) => cur = p,
                None => return false,
            }
        }
        false
    }

    // Session list

    fn update_project(&self) {
        let st = self.st();
        st.set_project_name(short_name(&self.dir).into());
        st.set_project_path(self.dir.display().to_string().into());
    }

    fn refresh_sessions(&mut self) {
        self.st().set_sessions_loading(true);
        self.rt.spawn_blocking(|| {
            let root = opencode_core::paths::get().storage().join("session");
            let mut all = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&root) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        let project = e.file_name().to_string_lossy().to_string();
                        all.extend(session::list(&project).into_iter().filter(|s| s.parent_id.is_none()));
                    }
                }
            }
            post(move |c| {
                c.summaries = all;
                c.st().set_sessions_loading(false);
                c.rebuild_session_rows();
            });
        });
    }

    fn rebuild_session_rows(&mut self) {
        let mut groups: Vec<(PathBuf, Vec<&session::Summary>)> = Vec::new();
        for s in &self.summaries {
            let d = s.directory.clone();
            match groups.iter_mut().find(|(g, _)| *g == d) {
                Some((_, v)) => v.push(s),
                None => groups.push((d, vec![s])),
            }
        }
        for (_, v) in &mut groups {
            v.sort_by(|a, b| b.time_updated.cmp(&a.time_updated));
        }
        let cur = self.dir.clone();
        groups.sort_by_key(|(d, v)| (*d != cur, -v.first().map(|s| s.time_updated).unwrap_or(0)));
        let mut rows = Vec::new();
        self.session_dirs.clear();
        for (d, v) in &groups {
            let dir = d.display().to_string();
            let collapsed = self.prefs.collapsed.contains(&dir);
            rows.push(SessionRow { kind: 0, dir: dir.clone().into(), dir_short: short_name(d).into(), count: v.len() as i32, collapsed, ..Default::default() });
            for s in v {
                self.session_dirs.insert(s.id.clone(), d.clone());
                if collapsed {
                    continue;
                }
                rows.push(SessionRow {
                    kind: 1,
                    id: s.id.as_str().into(),
                    title: if s.title.is_empty() { "Untitled session".into() } else { s.title.as_str().into() },
                    meta: relative(s.time_updated).into(),
                    dir: dir.clone().into(),
                    dir_short: short_name(d).into(),
                    busy: self.busy.contains(&s.id),
                    ..Default::default()
                });
            }
        }
        self.st().set_session_rows(ModelRc::new(VecModel::from(rows)));
        // Recent projects: remembered ones first, then any other directory that has sessions.
        let mut recent: Vec<PathBuf> = self.prefs.recent_dirs.clone();
        for (d, _) in &groups {
            if !recent.contains(d) {
                recent.push(d.clone());
            }
        }
        let dirs: Vec<DirRow> = recent
            .into_iter()
            .filter(|d| *d != cur && d.is_dir())
            .take(10)
            .map(|d| DirRow { name: short_name(&d).into(), path: d.display().to_string().into(), current: false })
            .collect();
        self.st().set_recent_dirs(ModelRc::new(VecModel::from(dirs)));
    }

    fn toggle_group(&mut self, dir: String) {
        if self.prefs.collapsed.contains(&dir) {
            self.prefs.collapsed.retain(|d| *d != dir);
        } else {
            self.prefs.collapsed.push(dir);
        }
        self.prefs.save();
        self.rebuild_session_rows();
    }

    fn switch_dir(&mut self, dir: PathBuf) {
        if !dir.is_dir() {
            self.toast(format!("Not a folder: {}", dir.display()), true);
            return;
        }
        let dir = canonical(&dir);
        self.dir = dir.clone();
        self.prefs.current_dir = Some(dir.clone());
        self.prefs.push_recent(&dir);
        self.prefs.save();
        self.update_project();
        self.rebuild_session_rows();
        self.ensure_app(dir, |_, _| {});
    }

    fn browse_folder(&mut self, create: bool) {
        let start = self.dir.clone();
        self.rt.spawn(async move {
            match platform::pick_folder(&start).await {
                Ok(Some(p)) => post(move |c| {
                    if create {
                        c.new_session(Some(p));
                    } else {
                        c.switch_dir(p);
                    }
                }),
                Ok(None) => {}
                Err(e) => post(move |c| c.toast(e, true)),
            }
        });
    }

    fn new_session(&mut self, dir: Option<PathBuf>) {
        if let Some(d) = dir {
            if !d.is_dir() {
                self.toast(format!("Not a folder: {}", d.display()), true);
                return;
            }
            self.switch_dir(d);
        }
        let dir = self.dir.clone();
        self.prefs.push_recent(&dir);
        self.prefs.save();
        self.ensure_app(dir, |c, app| {
            let h = app.new_session(None);
            c.open_chat(app, h, None);
        });
    }

    fn open_session(&mut self, id: String) {
        let Some(dir) = self.session_dirs.get(&id).cloned() else { return };
        self.ensure_app(dir, move |c, app| match app.session(&id) {
            Ok(h) => c.open_chat(app, h, None),
            Err(e) => c.toast(format!("Couldn't open session: {e}"), true),
        });
    }

    fn rename_session(&mut self, id: String, title: String) {
        let title = title.trim().to_string();
        let Some(dir) = self.session_dirs.get(&id).cloned() else { return };
        self.ensure_app(dir, move |c, app| {
            if let Ok(h) = app.session(&id) {
                h.with(|s| s.title = title);
                h.save();
            }
            c.refresh_sessions();
        });
    }

    fn delete_session(&mut self, id: String) {
        let Some(dir) = self.session_dirs.get(&id).cloned() else { return };
        self.ensure_app(dir, move |c, app| {
            if let Ok(h) = app.session(&id) {
                h.abort();
            }
            app.forget_session(&id);
            if let Err(e) = session::delete(&app.project, &id) {
                c.toast(format!("Delete failed: {e}"), true);
            }
            c.refresh_sessions();
        });
    }

    // Chat lifecycle

    fn initial_selection(app: &App, handle: &Handle) -> (String, Option<String>, Option<String>) {
        let agent = handle.with(|s| s.agent.clone()).filter(|a| app.agents.contains_key(a)).unwrap_or_else(|| app.default_agent());
        let model_ref = handle
            .with(|s| s.model.clone())
            .or_else(|| app.agents.get(&agent).and_then(|a| a.model.clone()))
            .or_else(|| app.default_model());
        match model_ref {
            Some(r) => match r.split_once('#') {
                Some((m, v)) => (agent, Some(m.to_string()), Some(v.to_string())),
                None => (agent, Some(r), None),
            },
            None => (agent, None, None),
        }
    }

    fn open_chat(&mut self, app: Arc<App>, handle: Arc<Handle>, parent: Option<Chat>) {
        let (agent, model, variant) = Self::initial_selection(&app, &handle);
        self.chat = Some(Chat {
            app,
            handle,
            demo: None,
            demo_perm: false,
            agent,
            model,
            variant,
            attachments: vec![],
            revert: None,
            show: PAGE,
            error: None,
            retry: None,
            parent: parent.map(Box::new),
        });
        self.enter_chat();
    }

    fn enter_chat(&mut self) {
        self.rendered.clear();
        self.messages.set_vec(Vec::<MessageView>::new());
        let st = self.st();
        st.set_input(SharedString::new());
        st.set_slash_rows(ModelRc::default());
        st.set_show_info(false);
        st.set_in_chat(true);
        st.set_chat_demo(self.chat.as_ref().is_some_and(|c| c.demo.is_some()));
        self.render_chat();
        self.scroll_bottom();
        self.ui.invoke_focus_chat_input();
    }

    fn back(&mut self) {
        if let Some(d) = self.dictation.take() {
            d.stop();
        }
        let Some(mut chat) = self.chat.take() else { return };
        let empty = chat.handle.with(|s| s.messages.is_empty());
        if empty || chat.demo.is_some() {
            chat.app.forget_session(&chat.sid());
        }
        if let Some(parent) = chat.parent.take() {
            self.chat = Some(*parent);
            self.enter_chat();
            return;
        }
        self.cache.clear();
        self.st().set_in_chat(false);
        self.st().set_listening(false);
        self.refresh_sessions();
    }

    fn open_child(&mut self, id: String) {
        let Some(chat) = self.chat.take() else { return };
        let app = chat.app.clone();
        match app.session(&id) {
            Ok(h) => self.open_chat(app, h, Some(chat)),
            Err(e) => {
                self.chat = Some(chat);
                self.toast(format!("Couldn't open subagent session: {e}"), true);
            }
        }
    }

    fn scroll_bottom(&mut self) {
        self.bottom_tick += 1;
        self.st().set_scroll_to_bottom_tick(self.bottom_tick);
    }

    // Message rendering

    fn schedule_render(&mut self) {
        if self.render_scheduled || self.chat.is_none() {
            return;
        }
        self.render_scheduled = true;
        slint::Timer::single_shot(Duration::from_millis(40), || {
            with(|c| {
                c.render_scheduled = false;
                c.render_chat();
            });
        });
    }

    fn render_chat(&mut self) {
        let Some(chat) = &self.chat else { return };
        let handle = chat.handle.clone();
        let revert = chat.revert;
        let show = chat.show;
        let expand_thinking = self.prefs.expand_thinking;
        let base = chat.app.directory.clone();
        let (title, hidden, list) = {
            let s = handle.session.lock().unwrap();
            let visible = match revert {
                Some(r) => &s.messages[..r.min(s.messages.len())],
                None => &s.messages[..],
            };
            let start = visible.len().saturating_sub(show);
            let mut list: Vec<(String, u64, MessageView)> = Vec::new();
            for m in &visible[start..] {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                render::message_key(m).hash(&mut h);
                expand_thinking.hash(&mut h);
                let prefix = format!("{}:", m.id);
                let mut toggled: Vec<&String> = self.toggled.iter().filter(|k| k.starts_with(&prefix)).collect();
                toggled.sort();
                toggled.hash(&mut h);
                for p in &m.parts {
                    if let Part::Tool { id, .. } = p {
                        self.tool_times.get(id).hash(&mut h);
                    }
                }
                let key = h.finish();
                let view = match self.cache.get(&m.id) {
                    Some((k, v)) if *k == key => v.clone(),
                    _ => {
                        let mut ctx = render::Ctx { toggled: &self.toggled, expand_thinking, tool_times: &self.tool_times, images: &mut self.images, base: Some(&base) };
                        let v = render::message_view(m, &mut ctx);
                        self.cache.insert(m.id.clone(), (key, v.clone()));
                        v
                    }
                };
                // Skip the empty placeholder the engine inserts before a stream starts.
                if view.parts.row_count() == 0 && view.error.is_empty() && view.footer.is_empty() {
                    continue;
                }
                list.push((m.id.clone(), key, view));
            }
            (s.title.clone(), start, list)
        };
        let same_ids = list.len() == self.rendered.len() && list.iter().zip(&self.rendered).all(|(a, b)| a.0 == b.0);
        if same_ids {
            for (i, (id, key, view)) in list.into_iter().enumerate() {
                if self.rendered[i].1 != key {
                    self.messages.set_row_data(i, view);
                    self.rendered[i] = (id, key);
                }
            }
        } else if list.len() > self.rendered.len() && self.rendered.iter().zip(&list).all(|(a, b)| a.0 == b.0) {
            // Messages were only appended: patch changed rows in place and push the new ones.
            let n = self.rendered.len();
            for (i, (id, key, view)) in list.into_iter().enumerate() {
                if i < n {
                    if self.rendered[i].1 != key {
                        self.messages.set_row_data(i, view);
                        self.rendered[i] = (id, key);
                    }
                } else {
                    self.messages.push(view);
                    self.rendered.push((id, key));
                }
            }
        } else {
            self.rendered = list.iter().map(|(id, k, _)| (id.clone(), *k)).collect();
            self.messages.set_vec(list.into_iter().map(|(_, _, v)| v).collect::<Vec<_>>());
        }
        let st = self.st();
        st.set_chat_title(if self.chat.as_ref().is_some_and(|c| c.demo.is_some()) { "Demo: fix a bug".into() } else { title.into() });
        st.set_hidden_count(hidden as i32);
        self.update_chrome();
    }

    /// Everything around the message list: status line, banners, permissions, toolbar and stats.
    fn update_chrome(&mut self) {
        let Some(chat) = &self.chat else { return };
        let st = self.st();
        let sid = chat.sid();
        let busy = chat.handle.is_busy() || self.busy.contains(&sid);
        st.set_busy(busy || (chat.demo.is_some_and(|s| s < 3) && !chat.demo_perm));
        st.set_status_text(self.status.get(&sid).cloned().unwrap_or_else(|| if chat.demo.is_some() { "Working…".into() } else { String::new() }).into());
        st.set_error_text(chat.error.clone().unwrap_or_default().into());
        st.set_notice_text(chat.retry.as_ref().map(|r| format!("Retrying: {r}")).unwrap_or_default().into());
        st.set_revert_active(chat.revert.is_some());
        st.set_chat_dir(if chat.demo.is_some() { "demo".into() } else { short_name(&chat.app.directory).into() });

        let mut perms: Vec<PermissionView> = Vec::new();
        if chat.demo_perm {
            perms.push(PermissionView { id: crate::demo::PERMISSION_ID.into(), title: "Run: npm test -- LoginButton".into(), detail: "shell · npm test -- LoginButton".into() });
        }
        for (app, p) in &self.perms {
            if Arc::ptr_eq(app, &chat.app) && self.belongs_to_chat(app, &p.session_id) {
                perms.push(PermissionView { id: p.id.as_str().into(), title: p.title.as_str().into(), detail: format!("{} · {}", p.action, p.resources.join(", ")).into() });
            }
        }
        if !perms.is_empty() {
            st.set_status_text("Waiting for your approval…".into());
        }
        st.set_permissions(ModelRc::new(VecModel::from(perms)));

        let agent = chat.app.agents.get(&chat.agent);
        st.set_agent_name(agent.map(|a| a.name.clone()).unwrap_or_else(|| chat.agent.clone()).to_lowercase().into());
        let idx = chat.app.primary_agents().iter().position(|a| a.id == chat.agent).unwrap_or(0);
        let color = agent.and_then(|a| a.color.as_deref()).and_then(parse_color).unwrap_or_else(|| parse_color(AGENT_COLORS[idx % AGENT_COLORS.len()]).unwrap());
        st.set_agent_color(color);
        let resolved = chat.model.as_ref().and_then(|m| models::resolve(&chat.app.providers.read().unwrap(), m).ok());
        st.set_model_label(match (&chat.model, &resolved) {
            (_, Some(r)) => r.model.name.clone(),
            (Some(m), None) => m.rsplit('/').next().unwrap_or(m).to_string(),
            (None, _) => "No model".into(),
        }.into());
        let variants = resolved.as_ref().map(|r| r.model.variants()).unwrap_or_default();
        st.set_has_variants(!variants.is_empty());
        st.set_variant_label(chat.variant.as_deref().map(capitalize).unwrap_or_default().into());
        st.set_attachments(ModelRc::new(VecModel::from(
            chat.attachments.iter().map(|a| AttachmentView { image: a.image.clone(), name: a.name.as_str().into() }).collect::<Vec<_>>(),
        )));
        let stats = self.stats(resolved.as_ref().map(|r| r.model.context).unwrap_or(0));
        self.st().set_stats(stats);
    }

    fn stats(&self, context: u64) -> StatsView {
        let Some(chat) = &self.chat else { return StatsView::default() };
        chat.handle.with(|s| {
            let (u, cost) = s.usage_total();
            let last = s.last_assistant().map(|m| m.usage.total_context()).unwrap_or(0);
            let percent = if context > 0 && last > 0 { ((last as f64 / context as f64) * 100.0).round() as i32 } else { 0 };
            let mut files: HashSet<String> = HashSet::new();
            let (mut add, mut del) = (0u64, 0u64);
            for m in &s.messages {
                for p in &m.parts {
                    if let Part::Tool { state: ToolState::Completed { metadata, .. }, .. } = p {
                        for f in metadata.get("files").and_then(|f| f.as_array()).into_iter().flatten() {
                            files.insert(f.get("file").and_then(|x| x.as_str()).unwrap_or("").to_string());
                            add += f.get("additions").and_then(|x| x.as_u64()).unwrap_or(0);
                            del += f.get("deletions").and_then(|x| x.as_u64()).unwrap_or(0);
                        }
                    }
                }
            }
            let nz = |n: u64| if n > 0 { fmt_tokens(n) } else { String::new() };
            StatsView {
                total: if last > 0 { thousands(last).into() } else { SharedString::new() },
                percent,
                cost: if cost > 0.0 { format!("${cost:.4}").into() } else { SharedString::new() },
                input: fmt_tokens(u.input).into(),
                output: fmt_tokens(u.output).into(),
                reasoning: nz(u.reasoning).into(),
                cache_read: nz(u.cache_read).into(),
                cache_write: nz(u.cache_write).into(),
                created: relative(s.time_created).into(),
                updated: if s.time_updated != s.time_created { relative(s.time_updated).into() } else { SharedString::new() },
                messages: s.messages.len().to_string().into(),
                changes: if files.is_empty() { SharedString::new() } else { format!("{}f +{add} -{del}", files.len()).into() },
                model: chat.model.as_deref().map(|m| m.rsplit('/').next().unwrap_or(m).to_string()).unwrap_or_default().into(),
            }
        })
    }

    fn toggle_expand(&mut self, key: String) {
        if !self.toggled.remove(&key) {
            self.toggled.insert(key);
        }
        self.render_chat();
    }

    fn load_more(&mut self, n: usize) {
        if let Some(ch) = &mut self.chat {
            ch.show = ch.show.saturating_add(n);
        }
        self.render_chat();
    }

    // Composer

    fn input_changed(&mut self, text: &str) {
        let rows: Vec<SlashRow> = match text.strip_prefix('/') {
            Some(q) if !q.contains(char::is_whitespace) => {
                let q = q.to_lowercase();
                let mut rows: Vec<SlashRow> = Vec::new();
                if let Some(chat) = &self.chat {
                    for c in chat.app.commands.values() {
                        if c.name.to_lowercase().starts_with(&q) {
                            rows.push(SlashRow { trigger: c.name.as_str().into(), description: c.description.as_str().into(), custom: true });
                        }
                    }
                }
                for (t, d) in BUILTIN_COMMANDS {
                    if t.starts_with(&q) {
                        rows.push(SlashRow { trigger: t.into(), description: d.into(), custom: false });
                    }
                }
                rows
            }
            _ => vec![],
        };
        self.st().set_slash_rows(ModelRc::new(VecModel::from(rows)));
    }

    fn slash_selected(&mut self, trigger: String) {
        if BUILTIN_COMMANDS.iter().any(|(t, _)| *t == trigger) {
            self.st().set_input(SharedString::new());
            self.input_changed("");
            self.builtin(&trigger);
            return;
        }
        let text = format!("/{trigger} ");
        self.st().set_input(text.clone().into());
        self.input_changed(&text);
        self.ui.invoke_focus_chat_input();
    }

    fn builtin(&mut self, name: &str) {
        match name {
            "new" => {
                let dir = self.chat.as_ref().map(|c| c.app.directory.clone());
                self.back_to_root();
                self.new_session(dir);
            }
            "models" | "model" => self.open_model_picker(),
            "agents" | "agent" => self.cycle_agent(false),
            "variant" => self.cycle_variant(),
            "compact" => {
                let Some(chat) = &self.chat else { return };
                if chat.demo.is_some() {
                    return;
                }
                let (app, sid) = (chat.app.clone(), chat.sid());
                self.rt.spawn(async move {
                    if let Err(e) = engine::compact_session(app.clone(), &sid).await {
                        app.emit(AppEvent::Error { session: sid, message: e.to_string() });
                    }
                });
            }
            "export" => self.export(),
            "thinking" => {
                self.prefs.expand_thinking = !self.prefs.expand_thinking;
                self.prefs.save();
                self.st().set_auto_expand_thinking(self.prefs.expand_thinking);
                self.cache.clear();
                self.render_chat();
                self.toast(if self.prefs.expand_thinking { "Thinking blocks expanded" } else { "Thinking blocks collapsed" }, false);
            }
            _ => {}
        }
    }

    fn back_to_root(&mut self) {
        while self.chat.as_ref().is_some_and(|c| c.parent.is_some()) {
            self.back();
        }
        self.back();
    }

    fn export(&mut self) {
        let Some(chat) = &self.chat else { return };
        let s = chat.handle.session.lock().unwrap().clone();
        let mut md = format!("# {}\n\n", s.title);
        for m in &s.messages {
            match m.role {
                Role::User => md.push_str(&format!("## User\n\n{}\n\n", m.text())),
                Role::Assistant => {
                    md.push_str("## Assistant\n\n");
                    for p in &m.parts {
                        match p {
                            Part::Text { text, .. } => md.push_str(&format!("{text}\n\n")),
                            Part::Tool { name, input, .. } => md.push_str(&format!("**Tool: {name}** `{}`\n\n", render::tool_summary(name, input, None))),
                            _ => {}
                        }
                    }
                }
            }
        }
        let path = chat.app.directory.join(format!("session-{}.md", s.id));
        match std::fs::write(&path, md) {
            Ok(_) => self.toast(format!("Exported to {}", path.display()), false),
            Err(e) => self.toast(format!("Export failed: {e}"), true),
        }
    }

    fn cycle_agent(&mut self, back: bool) {
        let Some(chat) = &mut self.chat else { return };
        let agents: Vec<String> = chat.app.primary_agents().iter().map(|a| a.id.clone()).collect();
        if agents.is_empty() {
            return;
        }
        let i = agents.iter().position(|a| *a == chat.agent).unwrap_or(0);
        let n = agents.len();
        chat.agent = agents[if back { (i + n - 1) % n } else { (i + 1) % n }].clone();
        if let Some(m) = chat.app.agents.get(&chat.agent).and_then(|a| a.model.clone()) {
            chat.model = Some(m);
        }
        self.update_chrome();
    }

    fn cycle_variant(&mut self) {
        let Some(chat) = &mut self.chat else { return };
        let Some(model) = chat.model.as_ref().and_then(|m| models::resolve(&chat.app.providers.read().unwrap(), m).ok()) else { return };
        let vs = model.model.variants();
        if vs.is_empty() {
            return;
        }
        chat.variant = match &chat.variant {
            None => Some(vs[0].clone()),
            Some(v) => vs.iter().position(|x| x == v).and_then(|i| vs.get(i + 1)).cloned(),
        };
        let label = chat.variant.clone().unwrap_or_else(|| "auto".into());
        self.update_chrome();
        self.toast(format!("Reasoning effort: {label}"), false);
    }

    fn send(&mut self) {
        let text = self.st().get_input().trim_end().to_string();
        let Some(chat) = &self.chat else { return };
        if text.trim().is_empty() && chat.attachments.is_empty() {
            return;
        }
        if chat.demo.is_some() {
            self.toast("This is a demo. Start a real session to chat with the agent.", false);
            return;
        }
        if self.prefs.lock_send && self.lock_available {
            self.rt.spawn_blocking(move || {
                let ok = platform::verify("Verify to send this message");
                post(move |c| {
                    if ok {
                        c.do_send(text);
                    } else {
                        c.toast("Authentication required to send messages", true);
                    }
                });
            });
            return;
        }
        self.do_send(text);
    }

    fn do_send(&mut self, text: String) {
        // Slash commands take no attachments.
        if let Some(rest) = text.strip_prefix('/') {
            let no_files = self.chat.as_ref().is_some_and(|c| c.attachments.is_empty());
            let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if no_files && BUILTIN_COMMANDS.iter().any(|(t, _)| *t == name) || ["model", "agent", "clear"].contains(&name) {
                self.st().set_input(SharedString::new());
                self.input_changed("");
                self.builtin(if name == "clear" { "new" } else { name });
                return;
            }
            let cmd = self.chat.as_ref().and_then(|c| c.app.commands.get(name).cloned());
            if let (true, Some(cmd)) = (no_files, cmd) {
                let Some(chat) = &self.chat else { return };
                let body = opencode_core::command::render(&cmd, args.trim(), &chat.app.directory, &chat.app.worktree);
                let opts = PromptOptions {
                    agent: cmd.agent.clone().or_else(|| Some(chat.agent.clone())),
                    model: cmd.model.clone().or_else(|| chat.model.clone()),
                    variant: chat.variant.clone(),
                };
                self.dispatch(Message::user_text(body), opts);
                return;
            }
        }
        let Some(chat) = &mut self.chat else { return };
        if chat.model.is_none() {
            self.toast("No model available. Add a provider key in the Providers tab.", true);
            return;
        }
        let resolved = chat.model.as_ref().and_then(|m| models::resolve(&chat.app.providers.read().unwrap(), m).ok());
        if !chat.attachments.is_empty() && resolved.as_ref().is_some_and(|r| !r.model.supports_image()) {
            self.toast("The selected model doesn't accept images. Pick a vision model or remove the attachments.", true);
            return;
        }
        let mut msg = Message::user_text(text.clone());
        if text.trim().is_empty() {
            msg.parts.clear();
        }
        msg.parts.extend(mention::expand_mentions(&chat.app, &text));
        for a in chat.attachments.drain(..) {
            msg.parts.push(Part::File(FilePart { mime: a.mime, data: a.data, name: a.name }));
        }
        let opts = PromptOptions { agent: Some(chat.agent.clone()), model: chat.model.clone(), variant: chat.variant.clone() };
        self.dispatch(msg, opts);
    }

    fn dispatch(&mut self, msg: Message, opts: PromptOptions) {
        let Some(chat) = &mut self.chat else { return };
        if let Some(idx) = chat.revert.take() {
            if chat.handle.is_busy() {
                chat.handle.abort();
            }
            chat.handle.with(|s| s.messages.truncate(idx));
            chat.handle.save();
            self.cache.clear();
        }
        chat.error = None;
        let (app, sid) = (chat.app.clone(), chat.sid());
        self.busy.insert(sid.clone());
        self.status.insert(sid.clone(), "Working…".into());
        self.st().set_input(SharedString::new());
        self.input_changed("");
        self.rt.spawn(async move {
            let _ = engine::prompt(app, &sid, msg, opts).await;
        });
        self.render_chat();
        self.scroll_bottom();
    }

    fn abort(&mut self) {
        let Some(chat) = &self.chat else { return };
        chat.handle.abort();
        let sid = chat.sid();
        let ids: Vec<String> = self.perms.iter().filter(|(a, p)| Arc::ptr_eq(a, &chat.app) && self.belongs_to_chat(a, &p.session_id)).map(|(_, p)| p.id.clone()).collect();
        for id in ids {
            chat.app.reply_permission(&id, Reply::Reject);
        }
        self.status.remove(&sid);
        self.update_chrome();
    }

    // Model, agent and variant pickers

    fn open_model_picker(&mut self) {
        self.model_query.clear();
        self.update_model_rows();
        self.st().set_model_picker_open(true);
    }

    fn update_model_rows(&mut self) {
        let Some(chat) = &self.chat else { return };
        let providers = chat.app.providers();
        let mut avail = models::available(&chat.app.config, &providers);
        avail.sort_by_key(|id| (id != "opencode", providers.get(id).map(|p| p.name.clone()).unwrap_or_default()));
        let q = self.model_query.as_str();
        let matches = |name: &str, full: &str, pname: &str| q.is_empty() || name.to_lowercase().contains(q) || full.to_lowercase().contains(q) || pname.to_lowercase().contains(q);
        let current = chat.model.clone().unwrap_or_default();
        let mut rows = Vec::new();
        let mut recent_rows = Vec::new();
        for r in opencode_core::state::get().recent_models {
            let base = r.split('#').next().unwrap_or(&r).to_string();
            if recent_rows.iter().any(|x: &PickerRow| x.value == base.as_str()) {
                continue;
            }
            if let Ok(res) = models::resolve(&providers, &base) {
                if matches(&res.model.name, &base, &res.provider.name) {
                    recent_rows.push(PickerRow { kind: 1, value: base.as_str().into(), label: res.model.name.as_str().into(), detail: res.provider.name.as_str().into(), selected: base == current });
                }
            }
        }
        if !recent_rows.is_empty() {
            rows.push(PickerRow { kind: 0, label: "RECENT".into(), ..Default::default() });
            rows.extend(recent_rows.into_iter().take(5));
        }
        for id in &avail {
            let Some(p) = providers.get(id) else { continue };
            let mut ms: Vec<_> = p.models.values().filter(|m| m.tool_call).collect();
            ms.sort_by(|a, b| b.release_date.cmp(&a.release_date));
            let items: Vec<PickerRow> = ms
                .into_iter()
                .filter(|m| matches(&m.name, &m.full_id(), &p.name))
                .map(|m| {
                    let free = m.cost_input == 0.0 && m.cost_output == 0.0;
                    PickerRow {
                        kind: 1,
                        value: m.full_id().into(),
                        label: m.name.as_str().into(),
                        detail: format!("{}{}", m.id, if free { " · Free" } else { "" }).into(),
                        selected: m.full_id() == current,
                    }
                })
                .collect();
            if items.is_empty() {
                continue;
            }
            rows.push(PickerRow { kind: 0, label: p.name.to_uppercase().into(), ..Default::default() });
            rows.extend(items);
        }
        self.st().set_model_rows(ModelRc::new(VecModel::from(rows)));
    }

    fn pick_model(&mut self, value: String) {
        let Some(chat) = &mut self.chat else { return };
        chat.model = Some(value.clone());
        if let Ok(r) = models::resolve(&chat.app.providers.read().unwrap(), &value) {
            if chat.variant.as_ref().is_some_and(|v| !r.model.variants().contains(v)) {
                chat.variant = None;
            }
        }
        opencode_core::state::push_recent_model(&value);
        self.update_chrome();
    }

    fn open_variant_picker(&mut self) {
        let Some(chat) = &self.chat else { return };
        let Some(r) = chat.model.as_ref().and_then(|m| models::resolve(&chat.app.providers.read().unwrap(), m).ok()) else { return };
        let cur = chat.variant.clone().unwrap_or_default();
        let mut rows = vec![PickerRow { kind: 1, value: "".into(), label: "Auto".into(), detail: "Model default".into(), selected: cur.is_empty() }];
        for v in r.model.variants() {
            rows.push(PickerRow { kind: 1, value: v.as_str().into(), label: capitalize(&v).into(), detail: SharedString::new(), selected: v == cur });
        }
        self.st().set_variant_rows(ModelRc::new(VecModel::from(rows)));
        self.st().set_variant_picker_open(true);
    }

    fn pick_variant(&mut self, v: String) {
        if let Some(chat) = &mut self.chat {
            chat.variant = if v.is_empty() { None } else { Some(v) };
        }
        self.update_chrome();
    }

    // Permissions

    fn permission_reply(&mut self, id: String, reply: i32) {
        if id == crate::demo::PERMISSION_ID {
            if let Some(chat) = &mut self.chat {
                chat.demo_perm = false;
                chat.demo = Some(4);
                let m = crate::demo::outcome(reply != 2);
                chat.handle.with(|s| s.messages.push(m));
            }
            self.render_chat();
            self.scroll_bottom();
            return;
        }
        let r = match reply {
            0 => Reply::Once,
            1 => Reply::Always,
            _ => Reply::Reject,
        };
        if let Some(pos) = self.perms.iter().position(|(_, p)| p.id == id) {
            let (app, _) = self.perms.remove(pos);
            app.reply_permission(&id, r);
        }
        self.update_chrome();
    }

    // Message actions

    fn select_message(&mut self, id: String) {
        let Some(chat) = &self.chat else { return };
        let text = chat.handle.with(|s| s.messages.iter().find(|m| m.id == id).map(render::message_plain)).unwrap_or_default();
        self.st().set_select_text(text.into());
        self.st().set_select_open(true);
    }

    fn copy_text(&mut self, text: String) {
        match platform::copy_text(text) {
            Ok(_) => self.toast("Copied to clipboard", false),
            Err(e) => self.toast(format!("Copy failed: {e}"), true),
        }
    }

    fn edit_message(&mut self, id: String) {
        if self.chat.as_ref().is_some_and(|c| c.demo.is_some()) {
            return;
        }
        if !self.st().get_input().trim().is_empty() {
            self.ask("Replace draft?", "Editing this message will replace the text you're writing.", "Replace", move |c| c.do_edit(id));
            return;
        }
        self.do_edit(id);
    }

    fn do_edit(&mut self, id: String) {
        let Some(chat) = &mut self.chat else { return };
        if chat.handle.is_busy() {
            chat.handle.abort();
        }
        let found = chat.handle.with(|s| {
            let idx = s.messages.iter().position(|m| m.id == id)?;
            let m = &s.messages[idx];
            let text = m.parts.iter().filter_map(|p| match p {
                Part::Text { text, synthetic: false } => Some(text.clone()),
                _ => None,
            }).collect::<Vec<_>>().join("\n");
            let files: Vec<FilePart> = m.parts.iter().filter_map(|p| match p {
                Part::File(f) if f.mime.starts_with("image/") => Some(f.clone()),
                _ => None,
            }).collect();
            Some((idx, text, files))
        });
        let Some((idx, text, files)) = found else { return };
        chat.revert = Some(idx);
        chat.attachments = files
            .into_iter()
            .filter_map(|f| {
                let bytes = base64::engine::general_purpose::STANDARD.decode(&f.data).ok()?;
                let thumb = image::load_from_memory(&bytes).ok()?.thumbnail(160, 160).to_rgba8();
                let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(thumb.as_raw(), thumb.width(), thumb.height());
                Some(Attachment { mime: f.mime, data: f.data, name: f.name, image: Image::from_rgba8(buf) })
            })
            .collect();
        self.st().set_input(text.into());
        self.render_chat();
        self.scroll_bottom();
        self.ui.invoke_focus_chat_input();
    }

    fn undo_revert(&mut self) {
        if let Some(chat) = &mut self.chat {
            chat.revert = None;
            chat.attachments.clear();
        }
        self.st().set_input(SharedString::new());
        self.render_chat();
        self.scroll_bottom();
    }

    // Attachments

    fn add_attachment(&mut self, name: String, data: String, buf: slint::SharedPixelBuffer<slint::Rgba8Pixel>) {
        if let Some(chat) = &mut self.chat {
            chat.attachments.push(Attachment { mime: "image/jpeg".into(), data, name, image: Image::from_rgba8(buf) });
        }
        self.update_chrome();
    }

    fn attach_image(&mut self) {
        if self.chat.is_none() {
            return;
        }
        self.rt.spawn(async move {
            let files = match platform::pick_images().await {
                Ok(f) => f,
                Err(e) => return post(move |c| c.toast(e, true)),
            };
            for path in files.into_iter().take(10) {
                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "image.jpg".into());
                let res = std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| image::load_from_memory(&b).map_err(|e| e.to_string())).and_then(to_jpeg);
                post(move |c| match res {
                    Ok((data, buf)) => c.add_attachment(name, data, buf),
                    Err(e) => c.toast(format!("Couldn't attach {name}: {e}"), true),
                });
            }
        });
    }

    fn paste_clipboard(&mut self) {
        if self.chat.is_none() {
            return;
        }
        // Off the UI thread: an image on the clipboard is copied to a file first.
        self.rt.spawn_blocking(move || match platform::read_clipboard() {
            Err(e) => post(move |c| c.toast(e, true)),
            Ok(platform::Clip::ImageFile(path)) => {
                let res = std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| image::load_from_memory(&b).map_err(|e| e.to_string())).and_then(to_jpeg);
                post(move |c| match res {
                    Ok((data, buf)) => c.add_attachment("pasted-image.jpg".into(), data, buf),
                    Err(e) => c.toast(format!("Couldn't paste image: {e}"), true),
                });
            }
            Ok(platform::Clip::Text(t)) => post(move |c| {
                let cur = c.st().get_input().to_string();
                let next = format!("{cur}{t}");
                c.st().set_input(next.clone().into());
                c.input_changed(&next);
                c.ui.invoke_focus_chat_input();
            }),
            Ok(platform::Clip::Empty) => post(|c| c.toast("Clipboard is empty", false)),
        });
    }

    fn toggle_mic(&mut self) {
        if let Some(d) = self.dictation.take() {
            d.stop();
            self.st().set_listening(false);
            return;
        }
        if !platform::speech_available() {
            self.toast("Voice input isn't available on this platform", true);
            return;
        }
        self.st().set_listening(true);
        self.dictation = Some(Dictation::start(|res| {
            post(move |c| {
                c.dictation = None;
                c.st().set_listening(false);
                match res {
                    Ok(t) if !t.trim().is_empty() => {
                        let cur = c.st().get_input().to_string();
                        let next = if cur.is_empty() { t } else { format!("{cur} {t}") };
                        c.st().set_input(next.into());
                        c.ui.invoke_focus_chat_input();
                    }
                    Ok(_) => {}
                    Err(e) => c.toast(e, true),
                }
            });
        }));
    }

    // Demo

    fn start_demo(&mut self) {
        self.ensure_app(self.dir.clone(), |c, app| {
            let h = app.new_session(None);
            h.with(|s| s.messages = crate::demo::messages(0));
            c.chat = Some(Chat {
                app: app.clone(),
                handle: h,
                demo: Some(0),
                demo_perm: false,
                agent: app.default_agent(),
                model: Some("anthropic/claude-opus-4-6".into()),
                variant: None,
                attachments: vec![],
                revert: None,
                show: PAGE,
                error: None,
                retry: None,
                parent: None,
            });
            c.enter_chat();
            c.demo_tick();
        });
    }

    fn demo_tick(&mut self) {
        slint::Timer::single_shot(Duration::from_millis(1600), || {
            with(|c| {
                let Some(chat) = &mut c.chat else { return };
                let Some(step) = chat.demo else { return };
                if step >= 3 {
                    return;
                }
                let next = step + 1;
                chat.demo = Some(next);
                if next == 3 {
                    chat.demo_perm = true;
                } else {
                    chat.handle.with(|s| s.messages = crate::demo::messages(next));
                }
                c.render_chat();
                c.scroll_bottom();
                c.demo_tick();
            });
        });
    }

    // Providers

    fn refresh_providers(&mut self) {
        let Some(app) = self.any_app() else { return };
        let providers = app.providers();
        let saved: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(opencode_core::paths::get().auth_file())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let mut rows = Vec::new();
        for p in providers.values() {
            let Some(key) = &p.key else { continue };
            if p.id == "opencode" && key == "public" {
                continue;
            }
            let env = p.env.iter().find(|e| std::env::var(e).ok().is_some_and(|v| !v.is_empty() && v == *key));
            let (source, removable) = if saved.contains_key(&p.id) {
                ("Saved key".to_string(), true)
            } else if p.options.api_key.is_some() {
                ("opencode.json".to_string(), false)
            } else if let Some(e) = env {
                (format!("${e}"), false)
            } else {
                ("opencode app".to_string(), false)
            };
            rows.push(ProviderRow {
                id: p.id.as_str().into(),
                name: p.name.as_str().into(),
                source: source.into(),
                models: format!("{} models", p.models.len()).into(),
                removable,
                connected: true,
            });
        }
        let st = self.st();
        st.set_provider_rows(ModelRc::new(VecModel::from(rows)));
        match providers.get("opencode") {
            Some(p) if p.key.as_deref() == Some("public") => {
                st.set_zen_has_key(false);
                st.set_zen_status(format!("Anonymous free tier — {} free models, no account needed. Add an OpenCode key to unlock paid Zen models.", p.models.len()).into());
            }
            Some(p) => {
                st.set_zen_has_key(true);
                st.set_zen_status(format!("Using your OpenCode key — {} models available.", p.models.len()).into());
            }
            None => {
                st.set_zen_has_key(false);
                st.set_zen_status("OpenCode Zen is disabled in your config.".into());
            }
        }
    }

    fn reload_providers(&mut self) {
        for app in self.apps.values() {
            *app.providers.write().unwrap() = models::providers(&app.config);
        }
        self.refresh_providers();
        if let Some(chat) = &mut self.chat {
            if chat.model.is_none() {
                chat.model = chat.app.default_model().map(|m| m.split('#').next().unwrap_or(&m).to_string());
            }
        }
        self.update_chrome();
    }

    fn open_add_key(&mut self) {
        self.catalog_query.clear();
        self.st().set_add_key_provider(SharedString::new());
        self.update_catalog_rows();
        self.st().set_add_key_open(true);
    }

    fn update_catalog_rows(&mut self) {
        let Some(app) = self.any_app() else { return };
        let providers = app.providers();
        let mut ps: Vec<_> = providers.values().collect();
        ps.sort_by_key(|p| p.name.to_lowercase());
        let q = self.catalog_query.as_str();
        let rows: Vec<PickerRow> = ps
            .iter()
            .filter(|p| q.is_empty() || p.name.to_lowercase().contains(q) || p.id.contains(q))
            .map(|p| PickerRow {
                kind: 1,
                value: p.id.as_str().into(),
                label: p.name.as_str().into(),
                detail: if p.env.is_empty() { p.id.clone() } else { p.env.join(", ") }.into(),
                selected: p.key.as_deref().is_some_and(|k| k != "public"),
            })
            .collect();
        self.st().set_catalog_rows(ModelRc::new(VecModel::from(rows)));
    }

    fn choose_key_provider(&mut self, id: String) {
        let name = self.any_app().and_then(|a| a.providers().get(&id).map(|p| p.name.clone())).unwrap_or_else(|| id.clone());
        let st = self.st();
        st.set_add_key_provider(id.into());
        st.set_add_key_provider_name(name.into());
        st.set_add_key_open(true);
    }

    fn save_key(&mut self, id: String, key: String) {
        let key = key.trim().to_string();
        if key.is_empty() {
            return;
        }
        if let Err(e) = auth::set(&id, Credential::Api { key, metadata: None }) {
            self.toast(format!("Couldn't save key: {e}"), true);
            return;
        }
        let name = self.st().get_add_key_provider_name().to_string();
        self.st().set_add_key_open(false);
        self.reload_providers();
        self.toast(format!("Saved {name} key"), false);
    }

    fn remove_key(&mut self, id: String) {
        match auth::remove(&id) {
            Ok(_) => {
                self.reload_providers();
                self.toast("Key removed", false);
            }
            Err(e) => self.toast(format!("Couldn't remove key: {e}"), true),
        }
    }

    // Settings and app lock

    fn settings_changed(&mut self) {
        let lock_was = self.prefs.lock_enabled;
        let st = self.ui.global::<State>();
        let mut p = self.prefs.clone();
        p.theme = st.get_theme_pref();
        p.lock_enabled = st.get_lock_enabled();
        p.lock_send = st.get_lock_send() && p.lock_enabled;
        st.set_lock_send(p.lock_send);
        p.notify_finished = st.get_notify_finished();
        p.notify_permission = st.get_notify_permission();
        p.notify_error = st.get_notify_error();
        p.expand_thinking = st.get_auto_expand_thinking();
        if p.expand_thinking != self.prefs.expand_thinking {
            self.cache.clear();
        }
        let notify_was = self.prefs.notify_finished || self.prefs.notify_permission || self.prefs.notify_error;
        if (p.notify_finished || p.notify_permission || p.notify_error) && !notify_was {
            // The permission prompt can only be shown while we're in the foreground, i.e. now.
            self.rt.spawn_blocking(platform::request_notification_permission);
        }
        self.prefs = p;
        self.prefs.save();
        if self.prefs.lock_enabled && !lock_was {
            // Confirm the user can actually authenticate before the lock can engage.
            self.rt.spawn_blocking(move || {
                let ok = platform::verify("Confirm it's you to enable the app lock");
                post(move |c| {
                    if !ok {
                        c.prefs.lock_enabled = false;
                        c.prefs.lock_send = false;
                        c.prefs.save();
                        c.st().set_lock_enabled(false);
                        c.st().set_lock_send(false);
                        c.toast("App lock not enabled: verification failed", true);
                    }
                });
            });
        }
    }

    fn unlock(&mut self) {
        if self.unlocking {
            return;
        }
        self.unlocking = true;
        self.rt.spawn_blocking(move || {
            let ok = platform::verify("Unlock OpenCode");
            post(move |c| {
                c.unlocking = false;
                if ok {
                    c.st().set_locked(false);
                }
            });
        });
    }

    fn visibility_changed(&mut self, visible: bool) {
        if visible {
            // A keyboard dismissed while another activity covered ours leaves stale insets behind.
            platform::refresh_insets();
            if self.st().get_locked() && self.lock_available {
                self.unlock();
            }
        } else if self.prefs.lock_enabled && self.lock_available && !platform::picking() {
            // Lock on backgrounding, as opencode-mobile does, except while our own picker is up.
            self.st().set_locked(true);
        }
    }
}

/// Activity `Start`/`Stop` notification from the Android event loop.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn app_visible(visible: bool) {
    platform::set_visible(visible);
    post(move |c| c.visibility_changed(visible));
}
