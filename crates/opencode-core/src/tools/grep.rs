//! `grep`: regex or literal content search across files, honoring .gitignore.

use super::{arg_bool, arg_str, arg_u64, req_str, Context, Output, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::BufRead;
use std::path::PathBuf;

pub struct Grep;

pub struct Hit {
    pub path: PathBuf,
    pub line: usize,
    pub text: String,
}

pub fn search(root: &PathBuf, re: &regex::Regex, include: Option<&str>, limit: usize) -> Result<Vec<Hit>> {
    let inc = match include {
        Some(i) => Some(super::glob::matcher(i)?),
        None => None,
    };
    let mut hits = Vec::new();
    let files: Box<dyn Iterator<Item = PathBuf>> = if root.is_file() {
        Box::new(std::iter::once(root.clone()))
    } else {
        Box::new(
            ignore::WalkBuilder::new(root)
                .require_git(false)
                .build()
                .flatten()
                .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                .map(|e| e.into_path()),
        )
    };
    for path in files {
        if let Some((m, by_name)) = &inc {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let ok = if *by_name { m.is_match(path.file_name().unwrap_or_default()) } else { m.is_match(rel) };
            if !ok {
                continue;
            }
        }
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.len() > 20 * 1024 * 1024 {
            continue;
        }
        let Ok(f) = std::fs::File::open(&path) else { continue };
        let mut reader = std::io::BufReader::new(f);
        let mut first = true;
        let mut buf = Vec::new();
        let mut n = 0;
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if first {
                first = false;
                if buf.iter().take(8192).any(|b| *b == 0) {
                    break;
                }
            }
            n += 1;
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim_end_matches(['\n', '\r']);
            if re.is_match(line) {
                hits.push(Hit { path: path.clone(), line: n, text: crate::util::truncate_chars(line, 2000) });
                if hits.len() >= limit {
                    return Ok(hits);
                }
            }
        }
    }
    Ok(hits)
}

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> String {
        "Search file contents using ripgrep's regular expression syntax or literal text matching. Use it to locate specific code, symbols, or text patterns, and narrow searches with `path` or `include`. Returns matching file paths, line numbers, and line previews.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "minLength": 1, "description": "Regular expression or literal text to match in file contents." },
                "path": { "type": "string", "description": "File or directory to search. Defaults to the current working directory." },
                "include": { "type": "string", "description": "Glob pattern to filter files (for example, \"*.js\" or \"*.{ts,tsx}\")" },
                "literal": { "type": "boolean", "description": "Treat `pattern` as exact text instead of a regular expression (default: false)." },
                "caseSensitive": { "type": "boolean", "description": "Use case-sensitive matching (default: true)." },
                "limit": { "type": "integer", "exclusiveMinimum": 0, "description": format!("Maximum number of matching lines to return (default: {})", super::glob::DEFAULT_LIMIT) }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let pattern = req_str(&input, "pattern")?.to_string();
        if pattern.is_empty() {
            return Err(anyhow!("Pattern must not be empty"));
        }
        let given = arg_str(&input, "path").unwrap_or(".").to_string();
        let root = ctx.resolve(&given);
        ctx.authorize_external(&root).await?;
        ctx.ask("grep", &[pattern.clone()], &["*".into()], format!("Grep {pattern}"), json!({ "path": given })).await?;
        if !root.exists() {
            return Err(anyhow!("Search path does not exist: {given}"));
        }
        let literal = arg_bool(&input, "literal").unwrap_or(false);
        let case = arg_bool(&input, "caseSensitive").unwrap_or(true);
        let src = if literal { regex::escape(&pattern) } else { pattern.clone() };
        let re = regex::RegexBuilder::new(&src).case_insensitive(!case).build().map_err(|e| anyhow!("Invalid regex pattern: {e}"))?;
        let limit = arg_u64(&input, "limit").map(|l| l as usize).unwrap_or(super::glob::DEFAULT_LIMIT).max(1);
        let include = arg_str(&input, "include").map(String::from);
        let task = tokio::task::spawn_blocking(move || search(&root, &re, include.as_deref(), limit + 1));
        let mut hits = tokio::time::timeout(std::time::Duration::from_millis(super::glob::TIMEOUT_MS), task)
            .await
            .map_err(|_| anyhow!("Search timed out after 30 seconds. Consider using a more specific path or pattern."))???;
        let truncated = hits.len() > limit;
        hits.truncate(limit);
        let mut lines = vec![if hits.is_empty() { "No matches found".to_string() } else { format!("Found {} matches", hits.len()) }];
        let mut last: Option<&PathBuf> = None;
        for h in &hits {
            if last != Some(&h.path) {
                if last.is_some() {
                    lines.push(String::new());
                }
                lines.push(format!("{}:", h.path.display()));
                last = Some(&h.path);
            }
            lines.push(format!("  Line {}: {}", h.line, h.text));
        }
        if truncated {
            lines.push(String::new());
            lines.push(format!("(Results are truncated: showing first {} results. Consider using a more specific path or pattern.)", hits.len()));
        }
        Ok(Output::text(lines.join("\n"), pattern).meta(json!({ "matches": hits.len(), "truncated": truncated })))
    }
}
