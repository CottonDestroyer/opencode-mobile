//! `todowrite`: the session's working plan. Enabled with `experimental.todo`.

use super::{Context, Output, Tool};
use crate::session::Todo;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct TodoWrite;

const DESCRIPTION: &str = include_str!("../../assets/prompts/todowrite.txt");

pub fn summary(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "Todos cleared".into();
    }
    let count = |s: &str| todos.iter().filter(|t| t.status == s).count();
    let mut parts = vec![format!("{}/{} completed", count("completed"), todos.len())];
    for s in ["in_progress", "pending", "cancelled"] {
        let n = count(s);
        if n > 0 {
            parts.push(format!("{n} {}", s.replace('_', " ")));
        }
    }
    let active: Vec<String> = todos.iter().filter(|t| t.status == "in_progress").map(|t| serde_json::to_string(&t.content).unwrap_or_default()).collect();
    let mut out = format!("Todos updated: {}", parts.join(", "));
    if !active.is_empty() {
        out.push_str(&format!(". In progress: {}", active.join(", ")));
    }
    out
}

#[async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &str {
        "todowrite"
    }
    fn description(&self) -> String {
        DESCRIPTION.trim().to_string()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The updated todo list",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string", "description": "Brief description of the task" },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"], "description": "Current status of the task: pending, in_progress, completed, cancelled" }
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["todos"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let todos: Vec<Todo> = serde_json::from_value(input.get("todos").cloned().unwrap_or(Value::Array(vec![]))).map_err(|e| anyhow!("Invalid todos: {e}"))?;
        ctx.ask("todowrite", &["*".into()], &["*".into()], "Update todos".into(), Value::Null).await?;
        ctx.handle.with(|s| s.todos = todos.clone());
        ctx.handle.save();
        Ok(Output::text(summary(&todos), format!("{} todos", todos.len())).meta(json!({ "todos": todos })))
    }
}
