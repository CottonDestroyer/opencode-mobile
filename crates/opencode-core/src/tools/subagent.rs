//! `subagent`: runs a task in a child session with a specialized agent.

use super::{arg_bool, arg_str, req_str, Context, Output, Tool};
use crate::agent::Agent;
use crate::app::{App, PromptOptions};
use crate::permission::Effect;
use crate::session::Message;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct Subagent {
    pub agents: Vec<(String, Option<String>)>,
}

/// The subagents the calling agent may launch.
pub fn available(app: &App, caller: &Agent) -> Vec<(String, Option<String>)> {
    let mut rules = crate::permission::defaults();
    if let Some(p) = &app.config.permission {
        rules.extend(crate::permission::from_config(p));
    }
    rules.extend(caller.permissions.iter().cloned());
    let mut v: Vec<(String, Option<String>)> = app
        .agents
        .values()
        .filter(|a| a.is_subagent() && !a.hidden && a.mode != crate::agent::Mode::Primary)
        .filter(|a| crate::permission::evaluate(&rules, "subagent", &a.id) != Effect::Deny)
        .map(|a| (a.id.clone(), a.description.clone()))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn final_text(app: &App, session_id: &str) -> String {
    app.session(session_id)
        .ok()
        .and_then(|h| h.with(|s| s.last_assistant().map(|m| (m.text(), m.error.clone()))))
        .map(|(t, e)| if t.trim().is_empty() { e.map(|e| format!("Error: {e}")).unwrap_or_default() } else { t })
        .unwrap_or_default()
}

#[async_trait]
impl Tool for Subagent {
    fn name(&self) -> &str {
        "subagent"
    }
    fn description(&self) -> String {
        let mut lines = vec![
            "Spawns an agent in a child session to work on the specified task.".to_string(),
            "The output includes a sessionID you can pass back later to continue that specific conversation with the subagent.".into(),
            "New child sessions start with fresh context, so include all relevant context and instructions when you don't pass a sessionID.".into(),
            "Foreground (default) runs the subagent to completion and returns its final response.".into(),
            "Background mode (background=true) launches it asynchronously and returns immediately; you are notified when it finishes.".into(),
            "Use background only for independent work that can run while you continue elsewhere.".into(),
        ];
        if !self.agents.is_empty() {
            lines.push(String::new());
            lines.push("Available subagents:".into());
            for (id, d) in &self.agents {
                lines.push(format!("- {id}: {}", d.as_deref().unwrap_or("This subagent should only be called when explicitly requested.")));
            }
        }
        lines.join("\n")
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": { "type": "string", "description": "The type of specialized agent to use for this task. If the user asks for a subagent by a name that is not one of the available subagents, they most likely mean a model: pick a suitable agent and pass the name through the model parameter instead." },
                "description": { "type": "string", "description": "A short 3-5 word label for the task, displayed to the user" },
                "prompt": { "type": "string", "description": "The task for the subagent to perform" },
                "model": { "type": "string", "description": "NEVER set this unless the user explicitly asks for a particular model or variant. The value is written as \"providerID/modelID\", or \"providerID/modelID#variant\" to include a variant. Do not guess the ID: look the model up with the models tool, filtering to your own provider first." },
                "sessionID": { "type": "string", "pattern": "^ses", "description": "Continue a specific previous subagent conversation by passing its sessionID. Calls without a sessionID start a new conversation." },
                "background": { "type": "boolean", "description": "Run the subagent in the background and return immediately. You will be notified when it completes. DO NOT sleep, poll, or proactively check on its progress." }
            },
            "required": ["agent", "description", "prompt"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let agent_id = req_str(&input, "agent").or_else(|_| req_str(&input, "subagent_type"))?.to_string();
        let description = arg_str(&input, "description").unwrap_or("subagent task").to_string();
        let prompt = req_str(&input, "prompt")?.to_string();
        if !self.agents.iter().any(|(id, _)| *id == agent_id) {
            return Err(anyhow!(
                "Unknown subagent: {agent_id}. Available subagents: {}",
                self.agents.iter().map(|a| a.0.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        ctx.ask("subagent", &[agent_id.clone()], &[agent_id.clone()], format!("Launch {agent_id}: {description}"), json!({ "description": description })).await?;
        let app: Arc<App> = ctx.app.clone();
        let child = match arg_str(&input, "sessionID") {
            Some(id) => app.session(id)?,
            None => {
                let h = app.create_session(Some(ctx.session_id.clone()));
                h.with(|s| s.title = format!("{description} (@{agent_id} subagent)"));
                h.save();
                h
            }
        };
        let child_id = child.id();
        let model = arg_str(&input, "model").map(String::from).or_else(|| app.agents.get(&agent_id).and_then(|a| a.model.clone())).or_else(|| Some(ctx.model.full_id()));
        let opts = PromptOptions { agent: Some(agent_id.clone()), model, variant: None };
        let msg = Message::user_text(prompt);
        let title = format!("{description} (@{agent_id})");

        if arg_bool(&input, "background").unwrap_or(false) {
            let parent = ctx.session_id.clone();
            let cid = child_id.clone();
            tokio::spawn(async move {
                let res = crate::engine::prompt(app.clone(), &cid, msg, opts).await;
                let (state, text) = match res {
                    Ok(_) => ("completed", final_text(&app, &cid)),
                    Err(e) => ("error", e.to_string()),
                };
                let note = format!("<subagent sessionID=\"{cid}\" state=\"{state}\">\n{text}\n</subagent>");
                crate::engine::notify(app, parent, note).await;
            });
            let out = [
                format!("The subagent is working in the background (sessionID: {child_id}). You will be notified automatically when it finishes."),
                "DO NOT sleep, poll for progress, ask the subagent for status, or duplicate this subagent's work; avoid working with the same files or topics it is using.".into(),
                "Work on non-overlapping tasks, or briefly tell the user what you launched and end your response.".into(),
            ]
            .join("\n");
            return Ok(Output::text(out, title).meta(json!({ "sessionID": child_id, "status": "running" })));
        }

        let cancel = ctx.cancel.clone();
        let c2 = child.clone();
        let watcher = tokio::spawn(async move {
            cancel.cancelled().await;
            c2.abort();
        });
        let res = crate::engine::prompt(app.clone(), &child_id, msg, opts).await;
        watcher.abort();
        res?;
        let text = final_text(&app, &child_id);
        Ok(Output::text(format!("sessionID: {child_id}\n\n<result>\n{text}\n</result>"), title).meta(json!({ "sessionID": child_id, "status": "completed" })))
    }
}
