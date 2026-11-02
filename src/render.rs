//! Converts engine messages and parts into the Slint view models.

use crate::markdown;
use crate::{DiffLine, MdBlock, MessageView, PartView, TodoRow, ToolView};
use base64::Engine;
use opencode_core::session::{Message, Part, Role, ToolState};
use serde_json::Value;
use slint::{Image, ModelRc, StyledText, VecModel};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub struct Ctx<'a> {
    /// Part keys whose expanded state the user toggled; each one inverts the default.
    pub toggled: &'a HashSet<String>,
    pub expand_thinking: bool,
    /// Tool call id → (start, end), in epoch milliseconds.
    pub tool_times: &'a HashMap<String, (i64, Option<i64>)>,
    pub images: &'a mut HashMap<String, Image>,
    /// Project root, for displaying paths relative to it.
    pub base: Option<&'a Path>,
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// Shows `p` relative to the project root when inside it, else relative to `~` when under home.
fn rel(p: &str, base: Option<&Path>) -> String {
    if p.is_empty() {
        return String::new();
    }
    let path = Path::new(p);
    if let Some(b) = base.filter(|b| opencode_core::util::contains(b, path)) {
        return opencode_core::util::relative(b, path);
    }
    let home = &opencode_core::paths::get().home;
    if path.is_absolute() && opencode_core::util::contains(home, path) {
        return format!("~/{}", opencode_core::util::relative(home, path));
    }
    p.to_string()
}

fn path_of(input: &Value) -> String {
    let p = s(input, "path");
    if p.is_empty() { s(input, "filePath") } else { p }
}

/// Truncates to `max_lines` lines and `max_chars` characters, keeping the end rather than the start
/// when `tail` is set.
pub fn clip(text: &str, max_lines: usize, max_chars: usize, tail: bool) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = if lines.len() > max_lines {
        if tail {
            format!("… {} earlier lines\n{}", lines.len() - max_lines, lines[lines.len() - max_lines..].join("\n"))
        } else {
            format!("{}\n… {} more lines", lines[..max_lines].join("\n"), lines.len() - max_lines)
        }
    } else {
        text.trim_end().to_string()
    };
    if out.chars().count() > max_chars {
        let cut: String = out.chars().take(max_chars).collect();
        out = format!("{cut}\n… (truncated)");
    }
    out.replace('\t', "    ")
}

pub fn md_blocks(text: &str) -> ModelRc<MdBlock> {
    model(
        markdown::blocks(text)
            .into_iter()
            .map(|b| MdBlock {
                kind: b.kind,
                text: if b.kind == 2 || b.kind == 4 {
                    StyledText::default()
                } else {
                    StyledText::from_markdown(&b.md).unwrap_or_else(|_| StyledText::from_plain_text(&b.plain))
                },
                plain: b.plain.into(),
                level: b.level,
                bullet: b.bullet.into(),
                lang: b.lang.into(),
            })
            .collect(),
    )
}

pub fn tool_label(name: &str) -> &str {
    match name {
        "shell" => "Shell",
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "patch" => "Patch",
        "glob" => "Glob",
        "grep" => "Search",
        "webfetch" => "Fetch",
        "websearch" => "Web search",
        "todowrite" => "Todos",
        "subagent" => "Task",
        "skill" => "Skill",
        n => n,
    }
}

/// One-line summary of a tool call's arguments, matching the opencode TUI.
pub fn tool_summary(name: &str, input: &Value, base: Option<&Path>) -> String {
    match name {
        "shell" => {
            let c = s(input, "command");
            let mut out = c.lines().next().unwrap_or("").to_string();
            if c.lines().count() > 1 {
                out.push_str(" …");
            }
            if input.get("background").and_then(|b| b.as_bool()) == Some(true) {
                out.push_str("  (background)");
            }
            out
        }
        "read" | "write" | "edit" => rel(&path_of(input), base),
        "glob" => format!("{}{}", s(input, "pattern"), if s(input, "path").is_empty() { String::new() } else { format!(" in {}", s(input, "path")) }),
        "grep" => format!("\"{}\"{}", s(input, "pattern"), if s(input, "include").is_empty() { String::new() } else { format!(" ({})", s(input, "include")) }),
        "webfetch" => s(input, "url"),
        "websearch" => s(input, "query"),
        "subagent" => format!("@{} {}", s(input, "agent"), s(input, "description")),
        "skill" => s(input, "id"),
        "todowrite" => format!("{} todos", input.get("todos").and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0)),
        "patch" => {
            let t = s(input, "patchText");
            t.lines()
                .filter_map(|l| l.strip_prefix("*** Add File: ").or_else(|| l.strip_prefix("*** Update File: ")).or_else(|| l.strip_prefix("*** Delete File: ")))
                .collect::<Vec<_>>()
                .join(", ")
        }
        _ => {
            let raw = input.to_string();
            if raw == "{}" || raw == "null" { String::new() } else { raw.chars().take(100).collect() }
        }
    }
}

fn diff_from_unified(diff: &str, out: &mut Vec<DiffLine>) {
    for l in diff.lines() {
        if l.starts_with("---") || l.starts_with("+++") || l.starts_with("Index:") || l.starts_with("====") {
            continue;
        }
        let (kind, text) = if let Some(t) = l.strip_prefix('+') {
            (1, t)
        } else if let Some(t) = l.strip_prefix('-') {
            (2, t)
        } else if l.starts_with("@@") {
            (3, l)
        } else {
            (0, l.strip_prefix(' ').unwrap_or(l))
        };
        out.push(DiffLine { kind, text: text.replace('\t', "    ").into() });
    }
}

fn diff_from_strings(before: &str, after: &str, out: &mut Vec<DiffLine>) {
    let diff = similar::TextDiff::from_lines(before, after);
    for change in diff.iter_all_changes() {
        let kind = match change.tag() {
            similar::ChangeTag::Insert => 1,
            similar::ChangeTag::Delete => 2,
            similar::ChangeTag::Equal => 0,
        };
        out.push(DiffLine { kind, text: change.value().trim_end_matches('\n').replace('\t', "    ").into() });
    }
}

const MAX_DIFF: usize = 400;

fn cap_diff(mut v: Vec<DiffLine>) -> Vec<DiffLine> {
    if v.len() > MAX_DIFF {
        let more = v.len() - MAX_DIFF;
        v.truncate(MAX_DIFF);
        v.push(DiffLine { kind: 3, text: format!("… {more} more lines").into() });
    }
    v
}

fn elapsed(times: Option<&(i64, Option<i64>)>) -> String {
    match times {
        Some((start, Some(end))) => {
            let ms = end - start;
            if ms < 1000 { format!("{ms}ms") } else { format!("{:.1}s", ms as f64 / 1000.0) }
        }
        _ => String::new(),
    }
}

pub fn tool_view(id: &str, name: &str, input: &Value, state: &ToolState, expanded: bool, times: Option<&(i64, Option<i64>)>, base: Option<&Path>) -> ToolView {
    let summary = tool_summary(name, input, base);
    let title = if summary.is_empty() { tool_label(name).to_string() } else { format!("{} {}", tool_label(name), summary) };
    let status = match state {
        ToolState::Pending => 0,
        ToolState::Running => 1,
        ToolState::Completed { .. } => 2,
        ToolState::Error { .. } => 3,
    };
    let (output, metadata) = match state {
        ToolState::Completed { output, metadata, .. } => (output.as_str(), metadata.clone()),
        _ => ("", Value::Null),
    };
    let error = match state {
        ToolState::Error { error } => clip(error, 12, 2000, false),
        _ => String::new(),
    };
    let mut v = ToolView {
        id: id.into(),
        name: name.into(),
        title: title.into(),
        elapsed: elapsed(times).into(),
        status,
        expanded,
        error: error.into(),
        ..Default::default()
    };
    if !expanded {
        return v;
    }
    let mut diffs = Vec::new();
    let file_diffs = |diffs: &mut Vec<DiffLine>, with_headers: bool| {
        for f in metadata.get("files").and_then(|f| f.as_array()).into_iter().flatten() {
            if with_headers {
                let add = f.get("additions").and_then(|x| x.as_u64()).unwrap_or(0);
                let del = f.get("deletions").and_then(|x| x.as_u64()).unwrap_or(0);
                diffs.push(DiffLine { kind: 3, text: format!("{}  +{add} -{del}", s(f, "file")).into() });
            }
            diff_from_unified(&s(f, "diff"), diffs);
        }
    };
    match name {
        "shell" => {
            v.command = s(input, "command").into();
            v.output = clip(output, 80, 8000, true).into();
        }
        "read" => {
            let mut p = rel(&path_of(input), base);
            let off = input.get("offset").and_then(|x| x.as_u64());
            let lim = input.get("limit").and_then(|x| x.as_u64());
            if off.is_some() || lim.is_some() {
                p.push_str(&format!(" ({}..{})", off.unwrap_or(0), lim.map(|l| l.to_string()).unwrap_or("end".into())));
            }
            v.file = p.into();
        }
        "write" => {
            v.file = rel(&path_of(input), base).into();
            file_diffs(&mut diffs, false);
            if diffs.is_empty() {
                v.output = clip(&s(input, "content"), 40, 6000, false).into();
            }
        }
        "edit" => {
            v.file = rel(&path_of(input), base).into();
            file_diffs(&mut diffs, false);
            if diffs.is_empty() {
                diff_from_strings(&s(input, "oldString"), &s(input, "newString"), &mut diffs);
            }
        }
        "patch" => {
            file_diffs(&mut diffs, true);
            if diffs.is_empty() {
                v.output = clip(&s(input, "patchText"), 60, 8000, false).into();
            }
        }
        "glob" | "grep" => {
            let pattern = s(input, "pattern");
            let path = s(input, "path");
            v.meta = if path.is_empty() { format!("Pattern: {pattern}") } else { format!("Pattern: {pattern} in {path}") }.into();
            v.output = clip(output, 30, 4000, false).into();
        }
        "webfetch" => {
            v.file = s(input, "url").into();
        }
        "websearch" => {
            v.meta = format!("Query: {}", s(input, "query")).into();
            v.output = clip(output, 30, 4000, false).into();
        }
        "subagent" => {
            v.meta = format!("@{} · {}", s(input, "agent"), s(input, "description")).into();
            v.output = clip(&s(input, "prompt"), 20, 3000, false).into();
            v.child_session = s(&metadata, "sessionID").into();
        }
        "todowrite" => {
            let todos: Vec<TodoRow> = input
                .get("todos")
                .and_then(|t| t.as_array())
                .into_iter()
                .flatten()
                .map(|t| TodoRow {
                    text: s(t, "content").into(),
                    status: match s(t, "status").as_str() {
                        "in_progress" => 1,
                        "completed" => 2,
                        "cancelled" => 3,
                        _ => 0,
                    },
                })
                .collect();
            v.todos = model(todos);
        }
        _ => {
            v.output = if output.is_empty() {
                clip(&serde_json::to_string_pretty(input).unwrap_or_default(), 30, 4000, false)
            } else {
                clip(output, 30, 4000, false)
            }
            .into();
        }
    }
    v.diffs = model(cap_diff(diffs));
    v
}

fn decode_image(key: &str, mime: &str, data: &str, cache: &mut HashMap<String, Image>) -> Option<Image> {
    if let Some(img) = cache.get(key) {
        return Some(img.clone());
    }
    if !mime.starts_with("image/") {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.thumbnail(400, 400).to_rgba8();
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(img.as_raw(), img.width(), img.height());
    let out = Image::from_rgba8(buf);
    cache.insert(key.to_string(), out.clone());
    Some(out)
}

pub fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.0}K", n as f64 / 1000.0)
    } else if n >= 1000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

pub fn message_view(m: &Message, ctx: &mut Ctx) -> MessageView {
    let user = m.role == Role::User;
    let mut parts = Vec::new();
    let all_synthetic = user && m.parts.iter().all(|p| matches!(p, Part::Text { synthetic: true, .. }));
    for (i, p) in m.parts.iter().enumerate() {
        let key = format!("{}:{i}", m.id);
        match p {
            Part::Text { text, synthetic } => {
                if *synthetic {
                    if all_synthetic {
                        let first: String = text.trim().chars().take(300).collect();
                        parts.push(PartView { kind: 5, key: key.into(), text: first.into(), ..Default::default() });
                    }
                    continue;
                }
                if text.trim().is_empty() {
                    continue;
                }
                if user {
                    parts.push(PartView { kind: 4, key: key.into(), text: text.as_str().into(), ..Default::default() });
                } else {
                    parts.push(PartView { kind: 0, key: key.into(), blocks: md_blocks(text), ..Default::default() });
                }
            }
            Part::Reasoning { text, .. } => {
                if text.trim().is_empty() {
                    continue;
                }
                let expanded = ctx.expand_thinking != ctx.toggled.contains(&key);
                let blocks = if expanded { md_blocks(text) } else { ModelRc::default() };
                parts.push(PartView { kind: 1, key: key.into(), blocks, expanded, ..Default::default() });
            }
            Part::Tool { id, name, input, state, .. } => {
                let expanded = ctx.toggled.contains(&key);
                let tool = tool_view(id, name, input, state, expanded, ctx.tool_times.get(id), ctx.base);
                parts.push(PartView { kind: 2, key: key.into(), tool, expanded, ..Default::default() });
            }
            Part::File(f) => match decode_image(&key, &f.mime, &f.data, ctx.images) {
                Some(image) => parts.push(PartView { kind: 3, key: key.into(), image, label: f.name.as_str().into(), ..Default::default() }),
                None => parts.push(PartView { kind: 5, key: key.into(), text: format!("Attached {} ({})", if f.name.is_empty() { "file" } else { &f.name }, f.mime).into(), ..Default::default() }),
            },
            Part::Compaction { .. } => {
                parts.push(PartView { kind: 5, key: key.into(), text: "Earlier conversation was compacted into a summary to save context.".into(), ..Default::default() });
            }
        }
    }
    let model_tag = if user { String::new() } else { m.model.as_deref().map(|r| r.split('#').next().unwrap_or(r).rsplit('/').next().unwrap_or(r).to_string()).unwrap_or_default() };
    let tokens = m.usage.input + m.usage.output + m.usage.cache_read + m.usage.cache_write;
    let footer = if !user && m.completed.is_some() && tokens > 0 {
        if m.cost > 0.0 { format!("{} tokens · ${:.4}", fmt_tokens(tokens), m.cost) } else { format!("{} tokens", fmt_tokens(tokens)) }
    } else {
        String::new()
    };
    MessageView {
        id: m.id.as_str().into(),
        user,
        model: model_tag.into(),
        parts: model(parts),
        error: m.error.as_deref().map(|e| clip(e, 12, 2000, false)).unwrap_or_default().into(),
        footer: footer.into(),
    }
}

/// Cheap fingerprint that changes whenever streamed text or tool state does; lets the controller
/// skip re-rendering unchanged messages.
pub fn message_key(m: &Message) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    m.parts.len().hash(&mut h);
    m.error.hash(&mut h);
    m.completed.hash(&mut h);
    for p in &m.parts {
        match p {
            Part::Text { text, synthetic } => (0u8, text.len(), *synthetic).hash(&mut h),
            Part::Reasoning { text, .. } => (1u8, text.len()).hash(&mut h),
            Part::Tool { input, state, .. } => {
                let st = match state {
                    ToolState::Pending => (0u8, 0usize),
                    ToolState::Running => (1, 0),
                    ToolState::Completed { output, .. } => (2, output.len()),
                    ToolState::Error { error } => (3, error.len()),
                };
                (2u8, input.to_string().len(), st).hash(&mut h)
            }
            Part::File(f) => (3u8, f.data.len()).hash(&mut h),
            Part::Compaction { summary } => (4u8, summary.len()).hash(&mut h),
        }
    }
    h.finish()
}

/// The whole message as plain text, for the "Select text" sheet.
pub fn message_plain(m: &Message) -> String {
    let mut out = Vec::new();
    for p in &m.parts {
        match p {
            Part::Text { text, synthetic } => {
                if !*synthetic || m.parts.iter().all(|p| matches!(p, Part::Text { synthetic: true, .. })) {
                    out.push(text.clone());
                }
            }
            Part::Reasoning { text, .. } if !text.trim().is_empty() => out.push(format!("Thinking:\n{text}")),
            Part::Tool { name, input, state, .. } => {
                let mut t = format!("Tool: {} {}", tool_label(name), tool_summary(name, input, None));
                match state {
                    ToolState::Completed { output, .. } if !output.is_empty() => t.push_str(&format!("\nResult:\n{}", clip(output, 200, 20000, false))),
                    ToolState::Error { error } => t.push_str(&format!("\nError: {error}")),
                    _ => {}
                }
                out.push(t);
            }
            Part::File(f) => out.push(format!("[File: {}]", f.name)),
            _ => {}
        }
    }
    if let Some(e) = &m.error {
        out.push(format!("Error: {e}"));
    }
    out.join("\n\n")
}

