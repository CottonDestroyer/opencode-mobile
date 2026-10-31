//! Skills: built-ins plus `SKILL.md` folders under `skill/` or `skills/` in the config directories,
//! and the `skill` tool that loads them.

use super::{req_str, Context, Output, Tool};
use crate::config::Config;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const OPENCODE_SKILL: &str = include_str!("../../assets/prompts/opencode.md");
const REPORT_SKILL: &str = include_str!("../../assets/prompts/report.md");

#[derive(Debug, Clone)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub content: String,
}

fn builtins() -> Vec<Skill> {
    vec![
        Skill {
            id: "opencode".into(),
            name: "OpenCode".into(),
            description: "Use this skill for any question about OpenCode itself, including how OpenCode works, using or configuring it, migrating from V1 to V2, troubleshooting it, developing plugins or integrations, using the OpenCode SDK, clients, server, or API, and contributing to the OpenCode codebase. Also use it for OpenCode agents, commands, skills, tools, permissions, MCP servers, providers, models, themes, keybinds, formatters, the CLI, TUI, desktop app, and web app.".into(),
            path: PathBuf::from("/builtin/opencode.md"),
            content: OPENCODE_SKILL.into(),
        },
        Skill {
            id: "report".into(),
            name: "Report".into(),
            description: "Use when the user wants to report an opencode issue or bug. Collect standard diagnostics, add user-specific reproduction context, and publish the issue with GitHub CLI.".into(),
            path: PathBuf::from("/builtin/report.md"),
            content: REPORT_SKILL.into(),
        },
    ]
}

fn scan(dir: &Path, out: &mut Vec<Skill>) {
    if !dir.is_dir() {
        return;
    }
    for e in ignore::WalkBuilder::new(dir).hidden(false).max_depth(Some(4)).build().flatten() {
        let p = e.path();
        if p.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(p) else { continue };
        let (fm, body) = crate::config::frontmatter(&text);
        let dir_name = p.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let name = fm.get("name").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| dir_name.clone());
        let id = fm.get("id").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| name.clone());
        let description = fm.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if out.iter().any(|s| s.id == id) {
            out.retain(|s| s.id != id);
        }
        out.push(Skill { id, name, description, path: p.to_path_buf(), content: body });
    }
}

pub fn discover(config: &Config, dirs: &[PathBuf], directory: &Path) -> Vec<Skill> {
    let mut out = builtins();
    for d in dirs {
        scan(&d.join("skill"), &mut out);
        scan(&d.join("skills"), &mut out);
    }
    for s in &config.skills {
        if s.starts_with("http://") || s.starts_with("https://") {
            continue;
        }
        scan(&crate::util::resolve(directory, s), &mut out);
    }
    out
}

pub fn render(skill: &Skill) -> String {
    let dir = skill.path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let mut files: Vec<String> = Vec::new();
    if skill.path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
        for e in ignore::WalkBuilder::new(&dir).hidden(false).build().flatten() {
            if e.file_type().is_some_and(|t| t.is_file()) && e.path().file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
                files.push(e.path().display().to_string());
            }
        }
        files.sort();
        files.truncate(10);
    }
    let mut lines = vec![
        format!("<skill_content name=\"{}\">", skill.name),
        format!("# Skill: {}", skill.name),
        String::new(),
        skill.content.trim().to_string(),
        String::new(),
        format!("Base directory for this skill: {}", dir.display()),
        "Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.".into(),
        "Note: file list is sampled.".into(),
        String::new(),
        "<skill_files>".into(),
    ];
    lines.extend(files.iter().map(|f| format!("<file>{f}</file>")));
    lines.push("</skill_files>".into());
    lines.push("</skill_content>".into());
    lines.join("\n")
}

pub struct SkillTool;

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> String {
        "Load a specialized skill's instructions and resources into the current conversation when the task at hand matches its description.\n\nThe skill ID must match an available skill or a skill explicitly referenced by the user.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "The ID of an available skill or a skill explicitly referenced by the user" } },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let id = req_str(&input, "id").or_else(|_| req_str(&input, "name"))?;
        let skill = ctx
            .app
            .skills
            .iter()
            .find(|s| s.id == id || s.name.eq_ignore_ascii_case(id))
            .ok_or_else(|| {
                let ids: Vec<&str> = ctx.app.skills.iter().map(|s| s.id.as_str()).collect();
                anyhow!("Unable to load skill {id}: not found. Available skills: {}", ids.join(", "))
            })?;
        ctx.ask("skill", &[skill.id.clone()], &[skill.id.clone()], format!("Load skill {}", skill.name), json!({ "name": skill.name })).await?;
        Ok(Output::text(render(skill), skill.name.clone()).meta(json!({ "name": skill.name, "directory": skill.path.parent().map(|p| p.display().to_string()) })))
    }
}
