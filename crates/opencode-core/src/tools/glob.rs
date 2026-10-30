//! `glob`: finds files by glob pattern, honoring .gitignore.

use super::{arg_bool, arg_str, arg_u64, req_str, Context, Output, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const DEFAULT_LIMIT: usize = 100;
pub const TIMEOUT_MS: u64 = 30_000;

pub struct Glob;

pub fn matcher(pattern: &str) -> Result<(globset::GlobMatcher, bool)> {
    let p = pattern.trim_start_matches("./");
    let by_name = !p.contains('/');
    let g = globset::GlobBuilder::new(p).literal_separator(!by_name).build().map_err(|e| anyhow!("Invalid glob pattern: {e}"))?;
    Ok((g.compile_matcher(), by_name))
}

pub fn walk(root: &PathBuf, pattern: &str, hidden: bool, limit: usize) -> Result<Vec<PathBuf>> {
    let (m, by_name) = matcher(pattern)?;
    let mut out: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let walker = ignore::WalkBuilder::new(root).hidden(!hidden).git_ignore(true).git_global(true).parents(true).require_git(false).build();
    for e in walker.flatten() {
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let p = e.path();
        if p.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        let rel = p.strip_prefix(root).unwrap_or(p);
        let ok = if by_name { m.is_match(rel.file_name().unwrap_or_default()) || m.is_match(rel) } else { m.is_match(rel) };
        if ok {
            let mtime = e.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::UNIX_EPOCH);
            out.push((mtime, p.to_path_buf()));
            if out.len() >= limit * 20 {
                break;
            }
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(out.into_iter().map(|(_, p)| p).take(limit).collect())
}

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> String {
        "Search file paths using a glob pattern (examples: \"**/*.ts\", \"src/**/*.tsx\").".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern to match files against" },
                "path": { "type": "string", "description": "Directory to search. Defaults to the current working directory." },
                "hidden": { "type": "boolean", "description": "Include hidden files and directories (default: false)." },
                "limit": { "type": "integer", "exclusiveMinimum": 0, "description": format!("Maximum number of matching files to return (default: {DEFAULT_LIMIT})") }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let pattern = req_str(&input, "pattern")?.to_string();
        let given = arg_str(&input, "path").filter(|p| *p != "undefined" && *p != "null").unwrap_or(".").to_string();
        let root = ctx.resolve(&given);
        ctx.authorize_external(&root).await?;
        ctx.ask("glob", &[pattern.clone()], &["*".into()], format!("Glob {pattern}"), json!({ "path": given })).await?;
        if !root.exists() {
            return Err(anyhow!("Search path does not exist: {given}"));
        }
        if !root.is_dir() {
            return Err(anyhow!("Search path is not a directory: {given}"));
        }
        let hidden = arg_bool(&input, "hidden").unwrap_or(false);
        let limit = arg_u64(&input, "limit").map(|l| l as usize).unwrap_or(DEFAULT_LIMIT).max(1);
        let p2 = pattern.clone();
        let task = tokio::task::spawn_blocking(move || walk(&root, &p2, hidden, limit + 1));
        let res = tokio::time::timeout(std::time::Duration::from_millis(TIMEOUT_MS), task)
            .await
            .map_err(|_| anyhow!("Search timed out after {} seconds. Consider using a more specific path or pattern.", TIMEOUT_MS / 1000))??;
        let mut files = res?;
        let truncated = files.len() > limit;
        files.truncate(limit);
        let mut lines: Vec<String> = if files.is_empty() { vec!["No files found".into()] } else { files.iter().map(|p| p.display().to_string()).collect() };
        if truncated {
            lines.push(String::new());
            lines.push(format!("(Results are truncated: showing first {} results. Consider using a more specific path or pattern.)", files.len()));
        }
        Ok(Output::text(lines.join("\n"), pattern).meta(json!({ "count": files.len(), "truncated": truncated })))
    }
}
