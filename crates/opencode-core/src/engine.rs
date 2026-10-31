//! The agent loop: stream a model turn, run the tool calls it makes, and repeat until the model stops.
//! Also handles steering (messages queued mid-turn), step limits, recovery from truncated output,
//! automatic compaction and session titles.

use crate::agent::Agent;
use crate::app::{App, AppEvent, Handle, PromptOptions};
use crate::models::{Model, Provider};
use crate::provider::{self, Event, Request, ToolSpec};
use crate::session::{Message, Part, Role, ToolState, Usage};
use crate::tools::{self, Tool};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const REJECTED: &str = "The user rejected permission to use this tool. Stop and wait for the user's next instruction; do not retry.";
const COMPACT_THRESHOLD: f64 = 0.85;

pub const COMPACTION_TEMPLATE: &str = "You MUST use this format for your response (you may omit sections that aren't applicable). Do not include the <template> tags in your response.
<template>
## Objective
- [one or two brief sentences describing what the user is trying to accomplish]

## Requirements
- [constraints, preferences, requirements, and scope boundaries stated by the user, or \"(none)\"]

## Decisions
- [decisions already made and why, or \"(none)\"]

## Work State
Break the objective into smaller goals and report which are completed, which are being worked on, and which are blocked.
### Completed
- [goals that have been completed; otherwise \"(none)\"]

### Active
- [goals currently being worked on; otherwise \"(none)\"]

### Blocked
- [anything blocking progress, and why; otherwise \"(none)\"]

## Next Move
1. [ordered list of next actions, or \"(none)\"]

## Relevant Files
List the files and directories, other than the current working directory, that another agent would need to open to continue this work. Include at most 15, most important first. Do not list every file that was read or changed. Include paths outside the current working directory when relevant. If none, write \"(none)\".
- `[file or directory path]`: [brief reason it matters]

## Important Context
- [facts the next agent cannot continue without and cannot easily find on its own; or \"(none)\"]
</template>";

const COMPACTION_RULES: &str = "Rules:
- Keep each section concise. Use terse, single-line bullets, not prose paragraphs or nested lists.
- Prefer short references over detailed restatement. It is fine to leave out information the next agent can recover from the code or the files listed above.
- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers.
- Carry forward only user questions or requests that remain unanswered or require further action. Do not repeat ones that newer history has answered or resolved. Preserve exact wording when carrying one forward.
- Preserve consequential workflow state, including whether changes are uncommitted, committed, pushed, under review, or merged.
- Do not mention the summary process or that context was compacted.";

fn compaction_prompt(existing: bool) -> String {
    let common = [
        "Summarize only what the user and the assistant said and did. Leave out instructions and setup the assistant was given rather than told by the user: repository conventions, instruction files such as AGENTS.md, and environment details like the session ID. The next agent receives current versions of all of these separately.",
        COMPACTION_TEMPLATE,
        COMPACTION_RULES,
        "Do not continue the task or call tools.",
        "Return only the structured summary in the requested format. Do not include a preamble, explanation, or other commentary.",
    ];
    let mut parts: Vec<&str> = Vec::new();
    if existing {
        parts.push("Update the existing checkpoint in the conversation above into one consolidated summary.");
        parts.push("Newer history always takes precedence over the existing checkpoint. Preserve previous information unless newer history clearly contradicts, supersedes, resolves, or makes it stale. If something is no longer relevant to continuing the work, you may remove it.");
        parts.push("Incorporate newer requirements, decisions, progress, and context. Reconcile Work State and Next Move: move completed work out of Active, remove resolved blockers and answered questions, and preserve unresolved or pending work.");
        parts.push("Return only the updated Markdown sections. Do not reproduce the `<conversation-checkpoint>`, `<summary>`, or `<recent-context>` wrapper tags from the previous checkpoint.");
    } else {
        parts.push("You MUST summarize the conversation above into a structured summary that will be given to another agent to resume the work.");
    }
    parts.extend(common);
    parts.join("\n\n")
}

pub struct Selection {
    pub provider: Provider,
    pub model: Model,
    pub variant: Option<String>,
}

/// Resolves the model for a turn: the explicit option, then the agent's, the session's, the default.
pub fn select_model(app: &App, agent: &Agent, opts: &PromptOptions, session_model: Option<&str>) -> Result<Selection> {
    let reference = opts
        .model
        .clone()
        .or_else(|| agent.model.clone())
        .or_else(|| session_model.map(String::from))
        .or_else(|| app.default_model())
        .ok_or_else(|| anyhow!("No model available. Set an API key (e.g. ANTHROPIC_API_KEY or OPENAI_API_KEY), run `opencode auth login`, or set \"model\" in opencode.json."))?;
    let r = crate::models::resolve(&app.providers.read().unwrap(), &reference)?;
    let variant = opts.variant.clone().or(r.variant).or_else(|| agent.variant.clone());
    if r.provider.key.is_none() && !app.config.provider.contains_key(&r.provider.id) && r.provider.id != "opencode" {
        let env = r.provider.env.first().cloned().unwrap_or_else(|| format!("{}_API_KEY", r.provider.id.to_uppercase()));
        return Err(anyhow!("No credentials for provider {}. Set {env} or run `opencode auth login {}`.", r.provider.id, r.provider.id));
    }
    Ok(Selection { provider: r.provider, model: r.model, variant })
}

pub fn cost(model: &Model, u: &Usage) -> f64 {
    (u.input as f64 * model.cost_input + u.output as f64 * model.cost_output + u.cache_read as f64 * model.cost_cache_read + u.cache_write as f64 * model.cost_cache_write) / 1_000_000.0
}

/// Sends a user message. If the session is busy, the message is queued and folded in at the next
/// step (steering).
pub async fn prompt(app: Arc<App>, session_id: &str, msg: Message, opts: PromptOptions) -> Result<()> {
    let handle = app.session(session_id)?;
    if handle.busy.swap(true, Ordering::SeqCst) {
        handle.queue.lock().unwrap().push_back((msg, opts));
        app.emit(AppEvent::Updated { session: session_id.to_string() });
        return Ok(());
    }
    app.emit(AppEvent::Busy { session: session_id.to_string(), busy: true });
    let mut result = Ok(());
    let mut pending = Some((msg, opts));
    loop {
        if let Some((msg, opts)) = pending.take() {
            result = turn(app.clone(), handle.clone(), msg, opts).await;
            if let Err(e) = &result {
                app.emit(AppEvent::Error { session: session_id.to_string(), message: e.to_string() });
            }
        }
        let next = handle.queue.lock().unwrap().pop_front();
        match next {
            Some(n) => pending = Some(n),
            None => {
                handle.busy.store(false, Ordering::SeqCst);
                // A message may have been queued between the pop and the store.
                let late = handle.queue.lock().unwrap().pop_front();
                match late {
                    Some(n) if !handle.busy.swap(true, Ordering::SeqCst) => pending = Some(n),
                    Some(n) => {
                        handle.queue.lock().unwrap().push_front(n);
                        break;
                    }
                    None => break,
                }
            }
        }
    }
    app.emit(AppEvent::Busy { session: session_id.to_string(), busy: false });
    result
}

/// Delivers a background notification (a finished shell command or subagent) to a session.
pub async fn notify(app: Arc<App>, session_id: String, text: String) {
    let mut msg = Message::new(Role::User);
    msg.parts.push(Part::Text { text, synthetic: true });
    let opts = app
        .session(&session_id)
        .ok()
        .map(|h| h.with(|s| PromptOptions { agent: s.agent.clone(), model: s.model.clone(), variant: None }))
        .unwrap_or_default();
    let _ = prompt(app, &session_id, msg, opts).await;
}

async fn turn(app: Arc<App>, handle: Arc<Handle>, mut msg: Message, opts: PromptOptions) -> Result<()> {
    let agent_id = opts.agent.clone().or_else(|| handle.with(|s| s.agent.clone())).unwrap_or_else(|| app.default_agent());
    let agent = app.agent(&agent_id)?.clone();
    let session_model = handle.with(|s| s.model.clone());
    let sel = select_model(&app, &agent, &opts, session_model.as_deref())?;
    let full = sel.model.full_id();
    let model_ref = match &sel.variant {
        Some(v) => format!("{full}#{v}"),
        None => full.clone(),
    };
    msg.agent = Some(agent.id.clone());
    msg.model = Some(model_ref.clone());
    let first = handle.with(|s| {
        s.agent = Some(agent.id.clone());
        s.model = Some(full.clone());
        s.messages.push(msg);
        s.messages.iter().filter(|m| m.role == Role::User).count() == 1 && s.has_default_title()
    });
    handle.save();
    if handle.with(|s| s.parent_id.is_none()) {
        crate::state::push_recent_model(&model_ref);
    }
    app.emit(AppEvent::Updated { session: handle.id() });
    if first {
        let (a, h) = (app.clone(), handle.clone());
        tokio::spawn(async move {
            // Best effort: on failure the session keeps its default title.
            let _ = generate_title(&a, &h).await;
            a.emit(AppEvent::Title { session: h.id() });
        });
    }
    run_loop(app, handle, agent, sel).await
}

fn apply_event(msg: &mut Message, ev: &Event, reasoning_open: &mut bool) {
    match ev {
        Event::TextDelta(t) => match msg.parts.last_mut() {
            Some(Part::Text { text, synthetic: false }) => text.push_str(t),
            _ => msg.parts.push(Part::Text { text: t.clone(), synthetic: false }),
        },
        Event::ReasoningDelta(t) => {
            if *reasoning_open {
                if let Some(Part::Reasoning { text, .. }) = msg.parts.iter_mut().rev().find(|p| matches!(p, Part::Reasoning { .. })) {
                    text.push_str(t);
                    return;
                }
            }
            *reasoning_open = true;
            msg.parts.push(Part::Reasoning { text: t.clone(), signature: None, redacted: None, item_id: None });
        }
        Event::ReasoningEnd { signature: sig, redacted: red, item_id: iid } => {
            if *reasoning_open {
                *reasoning_open = false;
                if let Some(Part::Reasoning { signature, redacted, item_id, .. }) = msg.parts.iter_mut().rev().find(|p| matches!(p, Part::Reasoning { .. })) {
                    *signature = sig.clone();
                    *redacted = red.clone();
                    *item_id = iid.clone();
                }
            } else if sig.is_some() || red.is_some() {
                msg.parts.push(Part::Reasoning { text: String::new(), signature: sig.clone(), redacted: red.clone(), item_id: iid.clone() });
            }
        }
        Event::ToolInputStart { id, name } => {
            if !id.is_empty() && !msg.parts.iter().any(|p| matches!(p, Part::Tool { id: pid, .. } if pid == id)) {
                msg.parts.push(Part::Tool { id: id.clone(), name: name.clone(), input: Value::Object(Default::default()), state: ToolState::Pending, provider_meta: None });
            }
        }
        Event::ToolCall { id, name, input, meta } => {
            let existing = msg.parts.iter_mut().find(|p| matches!(p, Part::Tool { id: pid, .. } if pid == id));
            match existing {
                Some(Part::Tool { input: i, provider_meta, name: n, .. }) => {
                    *i = input.clone();
                    *provider_meta = meta.clone();
                    *n = name.clone();
                }
                _ => msg.parts.push(Part::Tool { id: id.clone(), name: name.clone(), input: input.clone(), state: ToolState::Pending, provider_meta: meta.clone() }),
            }
        }
        Event::Usage(u) => msg.usage = u.clone(),
        Event::Finish(f) => msg.finish = Some(f.clone()),
        Event::Retry(_) => {}
    }
}

struct StepResult {
    finish: String,
    usage: Usage,
    tool_calls: usize,
    rejected: bool,
    aborted: bool,
}

async fn run_loop(app: Arc<App>, handle: Arc<Handle>, agent: Agent, sel: Selection) -> Result<()> {
    let sid = handle.id();
    let registry = tools::registry(&app, &agent, &sel.model).await;
    let max_steps = agent.steps;
    let mut step = 0usize;
    let mut length_retries = 0;
    loop {
        // Steering: fold queued user messages into the conversation.
        let queued: Vec<(Message, PromptOptions)> = handle.queue.lock().unwrap().drain(..).collect();
        if !queued.is_empty() {
            handle.with(|s| s.messages.extend(queued.into_iter().map(|(m, _)| m)));
            handle.save();
        }
        step += 1;
        let last = max_steps.is_some_and(|m| step >= m);
        let active_tools: Vec<Arc<dyn Tool>> = if last { vec![] } else { registry.clone() };
        let res = step_once(&app, &handle, &agent, &sel, &active_tools, &registry, last).await?;
        app.emit(AppEvent::StepFinished { session: sid.clone() });
        if res.aborted || res.rejected {
            break;
        }
        // Compact automatically near the context limit, except right after a compaction, when there
        // is nothing new to summarize.
        let fresh_checkpoint = handle.with(|s| {
            let active = s.active_messages();
            active.first().is_some_and(|m| m.parts.iter().any(|p| matches!(p, Part::Compaction { .. }))) && active.len() <= 3
        });
        if app.config.compaction.auto
            && !fresh_checkpoint
            && sel.model.context > 0
            && res.usage.total_context() as f64 >= sel.model.context as f64 * COMPACT_THRESHOLD
        {
            let continuing = res.tool_calls > 0;
            if let Err(e) = compact(&app, &handle, &agent, &sel, &registry, continuing).await {
                app.emit(AppEvent::Error { session: sid.clone(), message: format!("compaction failed: {e}") });
                break;
            }
            if !continuing {
                break;
            }
            continue;
        }
        let has_queue = !handle.queue.lock().unwrap().is_empty();
        match res.finish.as_str() {
            "tool-calls" if res.tool_calls > 0 && !last => continue,
            "pause" => continue,
            "length" if res.tool_calls == 0 && length_retries < 2 => {
                length_retries += 1;
                let mut m = Message::new(Role::User);
                m.parts.push(Part::Text { text: crate::prompt::LENGTH.into(), synthetic: true });
                handle.with(|s| s.messages.push(m));
                continue;
            }
            _ if has_queue => continue,
            _ => break,
        }
    }
    handle.save();
    app.emit(AppEvent::Updated { session: sid });
    Ok(())
}

fn build_request(
    sel: &Selection,
    agent: &Agent,
    system: Vec<String>,
    messages: Vec<Message>,
    tools: Vec<ToolSpec>,
    session: &provider::SessionRef,
) -> Request {
    Request {
        provider: sel.provider.clone(),
        model: sel.model.clone(),
        variant: sel.variant.clone(),
        system,
        messages,
        tools,
        temperature: agent.temperature,
        top_p: agent.top_p,
        max_tokens: provider::max_output(&sel.model),
        session: session.clone(),
    }
}

/// One model call, plus execution of the tool calls it made.
async fn step_once(
    app: &Arc<App>,
    handle: &Arc<Handle>,
    agent: &Agent,
    sel: &Selection,
    active: &[Arc<dyn Tool>],
    registry: &[Arc<dyn Tool>],
    last_step: bool,
) -> Result<StepResult> {
    let sid = handle.id();
    let names: Vec<String> = active.iter().map(|t| t.name().to_string()).collect();
    let system = crate::prompt::system(app, agent, &sel.model, &names).await;
    let mut messages: Vec<Message> = handle.with(|s| s.active_messages().to_vec());
    if last_step {
        let mut m = Message::new(Role::User);
        m.parts.push(Part::Text { text: crate::prompt::MAX_STEPS.into(), synthetic: true });
        messages.push(m);
    }
    // Providers require tools to stay declared whenever the history contains tool calls.
    let specs: Vec<ToolSpec> = if active.is_empty() && messages.iter().any(|m| m.tool_calls().next().is_some()) && !last_step {
        registry.iter().map(|t| tools::spec(t.as_ref())).collect()
    } else {
        active.iter().map(|t| tools::spec(t.as_ref())).collect()
    };
    let req = build_request(sel, agent, system, messages, specs, &handle.with(|s| provider::SessionRef::of(s)));

    let mut assistant = Message::new(Role::Assistant);
    assistant.agent = Some(agent.id.clone());
    assistant.model = Some(sel.model.full_id());
    let msg_id = assistant.id.clone();
    handle.with(|s| s.messages.push(assistant));
    app.emit(AppEvent::Updated { session: sid.clone() });

    let cancel = CancellationToken::new();
    *handle.cancel.lock().unwrap() = Some(cancel.clone());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(provider::stream(req, tx));
    let mut reasoning_open = false;
    let mut aborted = false;
    let update = |f: &mut dyn FnMut(&mut Message)| {
        handle.with(|s| {
            if let Some(m) = s.messages.iter_mut().rev().find(|m| m.id == msg_id) {
                f(m);
            }
        })
    };
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                match &ev {
                    Event::TextDelta(d) => app.emit(AppEvent::TextDelta { session: sid.clone(), delta: d.clone() }),
                    Event::ReasoningDelta(d) => app.emit(AppEvent::ReasoningDelta { session: sid.clone(), delta: d.clone() }),
                    Event::Retry(m) => app.emit(AppEvent::Retry { session: sid.clone(), message: m.clone() }),
                    _ => app.emit(AppEvent::Updated { session: sid.clone() }),
                }
                update(&mut |m| apply_event(m, &ev, &mut reasoning_open));
            }
            _ = cancel.cancelled() => {
                task.abort();
                aborted = true;
                break;
            }
        }
    }
    let stream_result = if aborted { Ok(Ok(())) } else { task.await };
    let error = match stream_result {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(e) if e.is_cancelled() => None,
        Err(e) => Some(format!("provider task failed: {e}")),
    };

    let model = sel.model.clone();
    let (usage, finish) = handle.with(|s| {
        let m = s.messages.iter_mut().rev().find(|m| m.id == msg_id).unwrap();
        m.cost = cost(&model, &m.usage);
        m.completed = Some(crate::util::now_ms());
        if aborted {
            m.error = Some("The operation was aborted.".into());
            m.finish = Some("aborted".into());
        }
        if let Some(e) = &error {
            m.error = Some(e.clone());
            m.finish = Some("error".into());
        }
        (m.usage.clone(), m.finish.clone().unwrap_or_else(|| "unknown".into()))
    });
    if let Some(e) = &error {
        app.emit(AppEvent::Error { session: sid.clone(), message: e.clone() });
    }
    if finish == "refusal" {
        app.emit(AppEvent::Error { session: sid.clone(), message: "The model declined to continue (refusal). Try rephrasing or switching models.".into() });
    }

    // Execute the tool calls.
    let calls: Vec<(String, String, Value)> = handle.with(|s| {
        s.messages
            .iter()
            .rev()
            .find(|m| m.id == msg_id)
            .map(|m| {
                m.parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Tool { id, name, input, state: ToolState::Pending, .. } => Some((id.clone(), name.clone(), input.clone())),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    });
    let set_state = |call_id: &str, state: ToolState| {
        handle.with(|s| {
            if let Some(m) = s.messages.iter_mut().rev().find(|m| m.id == msg_id) {
                for p in m.parts.iter_mut() {
                    if let Part::Tool { id, state: st, .. } = p {
                        if id == call_id {
                            *st = state.clone();
                        }
                    }
                }
            }
        })
    };
    if aborted || error.is_some() {
        for (id, _, _) in &calls {
            set_state(id, ToolState::Error { error: "Tool execution aborted".into() });
        }
        handle.save();
        return Ok(StepResult { finish, usage, tool_calls: 0, rejected: false, aborted: true });
    }

    let n_calls = calls.len();
    let rejected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let futs = calls.into_iter().map(|(call_id, name, input)| {
        let app = app.clone();
        let handle = handle.clone();
        let agent = agent.clone();
        let model = sel.model.clone();
        let sid = sid.clone();
        let cancel = cancel.clone();
        let rejected = rejected.clone();
        let tool = registry.iter().find(|t| t.name() == name).cloned();
        let available: Vec<String> = active.iter().map(|t| t.name().to_string()).collect();
        let set_state = &set_state;
        async move {
            set_state(&call_id, ToolState::Running);
            app.emit(AppEvent::ToolStarted { session: sid.clone(), id: call_id.clone(), name: name.clone(), input: input.clone() });
            let result: Result<tools::Output> = match tool {
                _ if input.get("__invalid_json").is_some() => Err(anyhow!(
                    "Invalid JSON in tool arguments ({}). Retry the call with valid JSON arguments.",
                    input.get("__error").and_then(|e| e.as_str()).unwrap_or("parse error")
                )),
                None => Err(anyhow!("Unknown tool: {name}. Available tools: {}", available.join(", "))),
                Some(_) if !available.contains(&name) => Err(anyhow!("Tool {name} is not available right now.")),
                Some(t) => {
                    let ctx = tools::Context { app: app.clone(), handle: handle.clone(), agent, model, session_id: sid.clone(), call_id: call_id.clone(), cancel: cancel.clone() };
                    tokio::select! {
                        r = t.execute(input, &ctx) => r,
                        _ = cancel.cancelled() => Err(anyhow!("Tool execution aborted")),
                    }
                }
            };
            let state = match result {
                Ok(out) => {
                    let mut text = if matches!(name.as_str(), "read" | "shell") { out.output } else { tools::truncate_output(&out.output).0 };
                    if let Some(c) = out.context {
                        text.push_str("\n\n");
                        text.push_str(&c);
                    }
                    app.emit(AppEvent::ToolFinished { session: sid.clone(), id: call_id.clone(), name: name.clone(), title: out.title.clone(), output: text.clone(), error: None, metadata: out.metadata.clone() });
                    ToolState::Completed { output: text, title: out.title, metadata: out.metadata, attachments: out.attachments }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg == REJECTED {
                        rejected.store(true, Ordering::SeqCst);
                    }
                    app.emit(AppEvent::ToolFinished { session: sid.clone(), id: call_id.clone(), name: name.clone(), title: String::new(), output: String::new(), error: Some(msg.clone()), metadata: serde_json::Value::Null });
                    ToolState::Error { error: msg }
                }
            };
            set_state(&call_id, state);
        }
    });
    futures::future::join_all(futs).await;
    *handle.cancel.lock().unwrap() = None;
    let was_cancelled = cancel.is_cancelled();
    handle.save();
    app.emit(AppEvent::Updated { session: sid });
    Ok(StepResult { finish, usage, tool_calls: n_calls, rejected: rejected.load(Ordering::SeqCst), aborted: was_cancelled })
}

/// Collects a plain-text completion, without executing tools.
pub async fn complete(sel: &Selection, agent: &Agent, system: Vec<String>, messages: Vec<Message>, tools: Vec<ToolSpec>, session: &provider::SessionRef, cancel: Option<CancellationToken>) -> Result<(String, Usage)> {
    complete_request(build_request(sel, agent, system, messages, tools, session), cancel).await
}

/// Runs one request to completion and collects its text.
async fn complete_request(req: Request, cancel: Option<CancellationToken>) -> Result<(String, Usage)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(provider::stream(req, tx));
    let mut text = String::new();
    let mut usage = Usage::default();
    let cancel = cancel.unwrap_or_default();
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(Event::TextDelta(t)) => text.push_str(&t),
                Some(Event::Usage(u)) => usage = u,
                Some(_) => {}
                None => break,
            },
            _ = cancel.cancelled() => { task.abort(); return Err(anyhow!("aborted")); }
        }
    }
    task.await.map_err(|e| anyhow!("{e}"))??;
    Ok((text, usage))
}

/// Summarizes the active history into a compaction checkpoint.
pub async fn compact(app: &Arc<App>, handle: &Arc<Handle>, agent: &Agent, sel: &Selection, registry: &[Arc<dyn Tool>], continuing: bool) -> Result<()> {
    let sid = handle.id();
    let mut messages: Vec<Message> = handle.with(|s| s.active_messages().to_vec());
    if messages.is_empty() {
        return Ok(());
    }
    let existing = messages.first().is_some_and(|m| m.parts.iter().any(|p| matches!(p, Part::Compaction { .. })));
    messages.push(Message::user_text(compaction_prompt(existing)));
    let compaction = app.agent("compaction").cloned().unwrap_or_else(|_| agent.clone());
    let names: Vec<String> = registry.iter().map(|t| t.name().to_string()).collect();
    let system = crate::prompt::system(app, &compaction, &sel.model, &names).await;
    let specs = registry.iter().map(|t| tools::spec(t.as_ref())).collect();
    app.emit(AppEvent::Notice { message: "Compacting conversation…".into() });
    let cancel = CancellationToken::new();
    *handle.cancel.lock().unwrap() = Some(cancel.clone());
    let (summary, usage) = complete(sel, &compaction, system, messages, specs, &handle.with(|s| provider::SessionRef::of(s)), Some(cancel)).await?;
    *handle.cancel.lock().unwrap() = None;
    if summary.trim().is_empty() {
        return Err(anyhow!("the model returned an empty summary"));
    }
    let mut m = Message::new(Role::User);
    m.parts.push(Part::Compaction { summary: summary.trim().to_string() });
    if continuing {
        m.parts.push(Part::Text { text: "Continue with the next steps of the work above, if there are any.".into(), synthetic: true });
    }
    m.usage = usage.clone();
    m.cost = cost(&sel.model, &usage);
    handle.with(|s| s.messages.push(m));
    handle.save();
    app.emit(AppEvent::Updated { session: sid });
    Ok(())
}

/// Compacts on request (e.g. `/compact`).
pub async fn compact_session(app: Arc<App>, session_id: &str) -> Result<()> {
    let handle = app.session(session_id)?;
    if handle.busy.swap(true, Ordering::SeqCst) {
        return Err(anyhow!("session is busy"));
    }
    app.emit(AppEvent::Busy { session: session_id.into(), busy: true });
    let res = async {
        let agent = app.agent(&handle.with(|s| s.agent.clone()).unwrap_or_else(|| app.default_agent()))?.clone();
        let sel = select_model(&app, &agent, &PromptOptions::default(), handle.with(|s| s.model.clone()).as_deref())?;
        let registry = tools::registry(&app, &agent, &sel.model).await;
        compact(&app, &handle, &agent, &sel, &registry, false).await
    }
    .await;
    handle.busy.store(false, Ordering::SeqCst);
    app.emit(AppEvent::Busy { session: session_id.into(), busy: false });
    res
}

pub async fn generate_title(app: &Arc<App>, handle: &Arc<Handle>) -> Result<()> {
    let first = handle.with(|s| s.messages.iter().find(|m| m.role == Role::User).map(|m| m.text())).unwrap_or_default();
    if first.trim().is_empty() {
        return Ok(());
    }
    let agent = app.agent("title")?.clone();
    let main = handle.with(|s| s.model.clone()).ok_or_else(|| anyhow!("no model"))?;
    let provider_id = main.split('/').next().unwrap_or("").to_string();
    let providers = app.providers();
    let small = crate::models::small_model(&app.config, &providers, &provider_id).unwrap_or(main);
    let sel = select_model(app, &agent, &PromptOptions { model: Some(small), ..Default::default() }, None)?;
    let sel = Selection { variant: if sel.model.has_reasoning_type("toggle") { Some("off".into()) } else { Some("low".into()).filter(|v| sel.model.variants().contains(v)) }, ..sel };
    let input = format!("Here is the first message of the conversation:\n\n<message>\n{first}\n</message>\n\nReply with only the title.");
    let mut req = build_request(&sel, &agent, vec![crate::agent::TITLE_PROMPT.into()], vec![Message::user_text(input)], vec![], &handle.with(|s| provider::SessionRef::of(s)));
    req.max_tokens = 0;
    let (text, _) = complete_request(req, None).await?;
    let title = text
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim_matches(['"', '\'', '`'])
        .trim_end_matches('.')
        .trim_start_matches("Title:")
        .trim()
        .to_string();
    if title.is_empty() {
        return Ok(());
    }
    let title = crate::util::truncate_chars(&title, 100);
    handle.with(|s| s.title = title.clone());
    handle.save();
    Ok(())
}
