//! `patch`: applies a `*** Begin Patch` envelope of add, update and delete operations.

use super::{req_str, Context, Output, Tool};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct Patch;

const DESCRIPTION: &str = include_str!("../../assets/prompts/patch.txt");

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub context: Option<String>,
    pub old: Vec<String>,
    pub new: Vec<String>,
    pub eof: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Add { path: String, contents: String },
    Delete { path: String },
    Update { path: String, move_to: Option<String>, chunks: Vec<Chunk> },
}

pub fn parse(text: &str) -> Result<Vec<Op>> {
    let text = text.replace("\r\n", "\n");
    let lines: Vec<&str> = text.trim().lines().collect();
    let start = lines.iter().position(|l| l.trim() == "*** Begin Patch").ok_or_else(|| anyhow!("The first line of the patch must be '*** Begin Patch'"))?;
    let end = lines.iter().rposition(|l| l.trim() == "*** End Patch").ok_or_else(|| anyhow!("The last line of the patch must be '*** End Patch'"))?;
    let body = &lines[start + 1..end];
    let mut ops = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        if let Some(p) = line.strip_prefix("*** Add File: ") {
            i += 1;
            let mut contents = Vec::new();
            while i < body.len() && !body[i].starts_with("*** ") {
                match body[i].strip_prefix('+') {
                    Some(rest) => contents.push(rest.to_string()),
                    None => bail!("Invalid Add File line (must start with '+'): {}", body[i]),
                }
                i += 1;
            }
            let mut c = contents.join("\n");
            if !contents.is_empty() {
                c.push('\n');
            }
            ops.push(Op::Add { path: p.trim().to_string(), contents: c });
        } else if let Some(p) = line.strip_prefix("*** Delete File: ") {
            ops.push(Op::Delete { path: p.trim().to_string() });
            i += 1;
        } else if let Some(p) = line.strip_prefix("*** Update File: ") {
            i += 1;
            let mut move_to = None;
            if i < body.len() {
                if let Some(m) = body[i].strip_prefix("*** Move to: ") {
                    move_to = Some(m.trim().to_string());
                    i += 1;
                }
            }
            let mut chunks = Vec::new();
            let mut cur: Option<Chunk> = None;
            while i < body.len() && !(body[i].starts_with("*** ") && !body[i].starts_with("*** End of File")) {
                let l = body[i];
                if l.starts_with("@@") {
                    if let Some(c) = cur.take() {
                        chunks.push(c);
                    }
                    let ctx = l.trim_start_matches('@').trim();
                    cur = Some(Chunk { context: if ctx.is_empty() { None } else { Some(ctx.to_string()) }, old: vec![], new: vec![], eof: false });
                } else if l.starts_with("*** End of File") {
                    if let Some(c) = cur.as_mut() {
                        c.eof = true;
                    }
                } else {
                    let c = cur.get_or_insert_with(|| Chunk { context: None, old: vec![], new: vec![], eof: false });
                    if let Some(r) = l.strip_prefix('-') {
                        c.old.push(r.to_string());
                    } else if let Some(r) = l.strip_prefix('+') {
                        c.new.push(r.to_string());
                    } else {
                        let r = l.strip_prefix(' ').unwrap_or(l);
                        c.old.push(r.to_string());
                        c.new.push(r.to_string());
                    }
                }
                i += 1;
            }
            if let Some(c) = cur.take() {
                chunks.push(c);
            }
            if chunks.is_empty() && move_to.is_none() {
                bail!("Update File section for {} has no changes", p.trim());
            }
            ops.push(Op::Update { path: p.trim().to_string(), move_to, chunks });
        } else if line.trim().is_empty() {
            i += 1;
        } else {
            bail!("Unexpected line in patch: {line}");
        }
    }
    Ok(ops)
}

fn norm(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let max = lines.len() - pattern.len();
    let order: Vec<usize> = if eof { (start..=max).rev().collect() } else { (start..=max).collect() };
    let cmps: [&dyn Fn(&str, &str) -> bool; 4] = [
        &|a, b| a == b,
        &|a, b| a.trim_end() == b.trim_end(),
        &|a, b| a.trim() == b.trim(),
        &|a, b| norm(a) == norm(b),
    ];
    for cmp in cmps {
        for &i in &order {
            if pattern.iter().enumerate().all(|(j, p)| cmp(&lines[i + j], p)) {
                return Some(i);
            }
        }
    }
    None
}

pub fn apply_chunks(path: &str, content: &str, chunks: &[Chunk]) -> Result<String> {
    let crlf = content.contains("\r\n");
    let normalized = content.replace("\r\n", "\n");
    let mut lines: Vec<String> = normalized.split('\n').map(String::from).collect();
    let trailing_newline = lines.last().is_some_and(|l| l.is_empty());
    if trailing_newline {
        lines.pop();
    }
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut cursor = 0;
    for c in chunks {
        if let Some(ctx) = &c.context {
            match seek(&lines, &[ctx.clone()], cursor, false) {
                Some(i) => cursor = i + 1,
                None => bail!("Failed to find context '{ctx}' in {path}"),
            }
        }
        if c.old.is_empty() {
            let at = if c.context.is_some() { cursor } else { lines.len() };
            replacements.push((at, 0, c.new.clone()));
            continue;
        }
        let mut old = c.old.clone();
        let mut new = c.new.clone();
        let mut found = seek(&lines, &old, cursor, c.eof);
        if found.is_none() && old.last().is_some_and(|l| l.is_empty()) {
            old.pop();
            if new.last().is_some_and(|l| l.is_empty()) {
                new.pop();
            }
            found = seek(&lines, &old, cursor, c.eof);
        }
        match found {
            Some(i) => {
                replacements.push((i, old.len(), new));
                cursor = i + old.len();
            }
            None => bail!("Failed to find expected lines in {path}:\n{}", c.old.join("\n")),
        }
    }
    replacements.sort_by_key(|r| r.0);
    for (start, len, new) in replacements.into_iter().rev() {
        lines.splice(start..start + len, new);
    }
    let mut out = lines.join("\n");
    if trailing_newline || !out.is_empty() {
        out.push('\n');
    }
    if crlf {
        out = out.replace('\n', "\r\n");
    }
    Ok(out)
}

#[async_trait]
impl Tool for Patch {
    fn name(&self) -> &str {
        "patch"
    }
    fn description(&self) -> String {
        DESCRIPTION.trim().to_string()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "patchText": { "type": "string", "description": "The full patch text describing add, update, and delete operations" }
            },
            "required": ["patchText"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let text = req_str(&input, "patchText").or_else(|_| req_str(&input, "patch")).or_else(|_| req_str(&input, "input"))?;
        if text.trim().is_empty() {
            bail!("patchText is required");
        }
        let ops = parse(text).map_err(|e| anyhow!("patch verification failed: {e}"))?;
        if ops.is_empty() {
            bail!("patch rejected: empty patch");
        }
        // Prepare every change first, so that a failure leaves all files untouched.
        struct Prepared {
            kind: char,
            target: std::path::PathBuf,
            dest: Option<std::path::PathBuf>,
            before: String,
            after: Option<String>,
        }
        let mut prepared = Vec::new();
        for op in &ops {
            match op {
                Op::Add { path, contents } => {
                    let t = ctx.resolve(path);
                    ctx.authorize_external(&t).await?;
                    prepared.push(Prepared { kind: 'A', target: t, dest: None, before: String::new(), after: Some(contents.clone()) });
                }
                Op::Delete { path } => {
                    let t = ctx.resolve(path);
                    ctx.authorize_external(&t).await?;
                    let before = std::fs::read_to_string(&t).map_err(|_| anyhow!("patch verification failed: Failed to delete {path}: file does not exist"))?;
                    prepared.push(Prepared { kind: 'D', target: t, dest: None, before, after: None });
                }
                Op::Update { path, move_to, chunks } => {
                    let t = ctx.resolve(path);
                    ctx.authorize_external(&t).await?;
                    let prior = prepared.iter().rev().find(|p| p.target == t && p.after.is_some()).and_then(|p| p.after.clone());
                    let before = match prior {
                        Some(b) => b,
                        None => std::fs::read_to_string(&t).map_err(|_| anyhow!("patch verification failed: Failed to read file to update {}: file does not exist", t.display()))?,
                    };
                    let after = if chunks.is_empty() { before.clone() } else { apply_chunks(path, &before, chunks).map_err(|e| anyhow!("patch verification failed: {e}"))? };
                    let dest = match move_to {
                        Some(m) => {
                            let d = ctx.resolve(m);
                            ctx.authorize_external(&d).await?;
                            Some(d)
                        }
                        None => None,
                    };
                    prepared.push(Prepared { kind: 'M', target: t, dest, before, after: Some(after) });
                }
            }
        }
        let resources: Vec<String> = prepared.iter().flat_map(|p| std::iter::once(ctx.display(&p.target)).chain(p.dest.as_ref().map(|d| ctx.display(d)))).collect();
        let mut files = Vec::new();
        let mut diff_all = String::new();
        for p in &prepared {
            let name = ctx.display(p.dest.as_ref().unwrap_or(&p.target));
            let after = p.after.clone().unwrap_or_default();
            let diff = crate::util::unified_diff(&name, &p.before, &after);
            let (add, del) = crate::util::diff_stats(&p.before, &after);
            diff_all.push_str(&diff);
            files.push(json!({ "file": name, "diff": diff, "additions": add, "deletions": del, "status": match p.kind { 'A' => "added", 'D' => "deleted", _ => "modified" } }));
        }
        ctx.ask("edit", &resources, &["*".into()], format!("Apply patch to {}", resources.join(", ")), json!({ "filepath": resources.join(", "), "diff": diff_all })).await?;

        let mut applied = Vec::new();
        for p in &prepared {
            let res: Result<()> = (|| {
                match p.kind {
                    'D' => std::fs::remove_file(&p.target)?,
                    _ => {
                        let dest = p.dest.as_ref().unwrap_or(&p.target);
                        if let Some(dir) = dest.parent() {
                            std::fs::create_dir_all(dir)?;
                        }
                        std::fs::write(dest, p.after.as_deref().unwrap_or(""))?;
                        if p.dest.is_some() && p.dest.as_ref() != Some(&p.target) {
                            std::fs::remove_file(&p.target)?;
                        }
                    }
                }
                Ok(())
            })();
            let shown = ctx.display(p.dest.as_ref().unwrap_or(&p.target));
            if let Err(e) = res {
                let done = applied.iter().map(|(_, n): &(char, String)| n.clone()).collect::<Vec<_>>().join(", ");
                bail!("Failed to write {shown}: {e}{}", if done.is_empty() { String::new() } else { format!(". Completed before failure: {done}") });
            }
            applied.push((p.kind, shown));
        }
        let mut lines = vec!["Success. Updated the following files:".to_string()];
        lines.extend(applied.iter().map(|(k, n)| format!("{k} {n}")));
        Ok(Output::text(lines.join("\n"), format!("{} file(s)", applied.len())).meta(json!({ "files": files })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_and_apply() {
        let p = "*** Begin Patch\n*** Add File: hello.txt\n+Hello world\n*** Update File: src/app.py\n*** Move to: src/main.py\n@@ def greet():\n-print(\"Hi\")\n+print(\"Hello, world!\")\n*** Delete File: obsolete.txt\n*** End Patch";
        let ops = parse(p).unwrap();
        assert_eq!(ops.len(), 3);
        if let Op::Update { chunks, move_to, .. } = &ops[1] {
            assert_eq!(move_to.as_deref(), Some("src/main.py"));
            let out = apply_chunks("x", "def greet():\n    print(\"Hi\")\n", chunks).unwrap();
            assert_eq!(out, "def greet():\nprint(\"Hello, world!\")\n");
        } else {
            panic!()
        }
    }
}
