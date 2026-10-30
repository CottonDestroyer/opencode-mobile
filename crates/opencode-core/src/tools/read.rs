//! `read`: reads a file (text, image or PDF) or lists a directory.

use super::{arg_u64, req_str, Context, Output, Tool};
use crate::session::FilePart;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

pub struct Read;

const BINARY_EXT: &[&str] = &[
    "zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar", "exe", "dll", "so", "dylib", "bin", "o", "a", "lib", "class", "jar", "war",
    "wasm", "pyc", "pyo", "node", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "mp3", "mp4", "wav", "avi", "mov",
    "mkv", "flac", "ogg", "webm", "ttf", "otf", "woff", "woff2", "eot", "ico", "bmp", "tiff", "psd", "sqlite", "db", "pdb", "iso", "dmg",
];

fn image_mime(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "pdf" => Some("application/pdf"),
        _ => None,
    }
}

fn is_binary(path: &Path, bytes: &[u8]) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if BINARY_EXT.contains(&ext.to_lowercase().as_str()) {
            return true;
        }
    }
    let sample = &bytes[..bytes.len().min(4096)];
    if sample.is_empty() {
        return false;
    }
    if sample.contains(&0) {
        return true;
    }
    let odd = sample.iter().filter(|b| (**b < 9) || (**b > 13 && **b < 32)).count();
    odd * 100 / sample.len() > 30
}

fn suggestions(path: &Path, given: &str) -> String {
    let base = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new("."));
    let given_dir = Path::new(given).parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let l = n.to_lowercase();
            if l.contains(&base) || base.contains(&l) {
                out.push(given_dir.join(&n).to_string_lossy().into_owned());
            }
            if out.len() >= 3 {
                break;
            }
        }
    }
    if out.is_empty() {
        format!("File not found: {given}")
    } else {
        format!("File not found: {given}\nDid you mean one of these?\n{}", out.join("\n"))
    }
}

/// Resolves near-miss spellings of the file name (NFC, non-breaking spaces, curly quotes).
fn alternate(path: &Path) -> Option<PathBuf> {
    let norm = |s: &str| s.replace(['\u{00a0}', '\u{202f}'], " ").replace(['\u{2018}', '\u{2019}'], "'");
    let target = norm(&path.file_name()?.to_string_lossy());
    let dir = path.parent()?;
    let matches: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_file() && norm(&e.file_name().to_string_lossy()) == target)
        .map(|e| e.path())
        .collect();
    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    }
}

/// AGENTS.md files between the read target and the project root that aren't loaded yet.
fn nested_instructions(ctx: &Context, target: &Path) -> Option<String> {
    if !crate::util::contains(&ctx.app.directory, target) {
        return None;
    }
    let start = if target.is_dir() { target.to_path_buf() } else { target.parent()?.to_path_buf() };
    let root = crate::util::normalize(&ctx.app.directory);
    let mut found = Vec::new();
    let mut cur = Some(start.as_path());
    while let Some(d) = cur {
        if crate::util::normalize(d) == root || !crate::util::contains(&root, d) {
            break;
        }
        let f = d.join("AGENTS.md");
        if f.is_file() && f != target {
            found.push(f);
        }
        cur = d.parent();
    }
    let fresh: Vec<PathBuf> = ctx.handle.with(|s| {
        let fresh: Vec<PathBuf> = found.into_iter().filter(|f| !s.loaded_instructions.contains(&f.display().to_string())).collect();
        for f in &fresh {
            s.loaded_instructions.push(f.display().to_string());
        }
        fresh
    });
    if fresh.is_empty() {
        return None;
    }
    let blocks: Vec<String> = fresh
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok().map(|c| format!("Instructions from: {}\n{}", f.display(), c)))
        .collect();
    Some(format!("<system-reminder>\n{}\n</system-reminder>", blocks.join("\n\n")))
}

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> String {
        "Read the contents of a file or directory. Supports text files, images, and PDFs. Images and PDFs are presented directly to the model. Each text line is prefixed by its 1-based line number as <line>: <content>. The prefix is for reference and is not part of the file content. Directory entries are returned one per line. Use offset and limit to read large files or directories in sections. Prefer one larger read over many small slices, and use grep to find specific content in large files.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File or directory to read" },
                "offset": { "type": "integer", "minimum": 0, "description": "The line or directory entry to start reading from (1-based)" },
                "limit": { "type": "integer", "minimum": 0, "description": "The maximum number of lines or directory entries to read (defaults to and capped at 2000)" }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let given = req_str(&input, "path").or_else(|_| req_str(&input, "filePath"))?.to_string();
        let offset = arg_u64(&input, "offset").unwrap_or(1).max(1) as usize;
        let limit = (arg_u64(&input, "limit").unwrap_or(DEFAULT_LIMIT as u64) as usize).clamp(1, DEFAULT_LIMIT);
        let mut path = ctx.resolve(&given);
        if !path.exists() {
            match alternate(&path) {
                Some(p) => path = p,
                None => return Err(anyhow!(suggestions(&path, &given))),
            }
        }
        ctx.authorize_external(&path).await?;
        ctx.ask("read", &[path.display().to_string()], &[], format!("Read {}", ctx.display(&path)), json!({ "filepath": path.display().to_string() })).await?;
        let context = nested_instructions(ctx, &path);

        if path.is_dir() {
            let mut entries: Vec<(bool, String)> = std::fs::read_dir(&path)?
                .flatten()
                .map(|e| {
                    let dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    let n = e.file_name().to_string_lossy().to_string();
                    (dir, if dir { format!("{n}/") } else { n })
                })
                .collect();
            entries.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
            let total = entries.len();
            let page: Vec<String> = entries.into_iter().skip(offset - 1).take(limit).map(|e| e.1).collect();
            let mut lines = vec![if page.is_empty() {
                format!("Read directory {given}, 0 entries")
            } else {
                format!("Read directory {given}, entries {offset}-{}", offset + page.len() - 1)
            }];
            let shown = page.len();
            lines.extend(page);
            let truncated = offset - 1 + shown < total;
            if truncated {
                lines.push(format!("[Output truncated. Continue reading with offset: {}]", offset + shown));
            }
            let mut out = Output::text(lines.join("\n"), ctx.display(&path)).meta(json!({ "truncated": truncated }));
            out.context = context;
            return Ok(out);
        }

        let bytes = std::fs::read(&path)?;
        if let Some(mime) = image_mime(&path) {
            let is_pdf = mime == "application/pdf";
            if (is_pdf && !ctx.model.supports_pdf() && !ctx.model.supports_image()) || (!is_pdf && !ctx.model.supports_image()) {
                return Err(anyhow!("The current model ({}) cannot read {} files", ctx.model.full_id(), if is_pdf { "PDF" } else { "image" }));
            }
            let mut out = Output::text(if is_pdf { "PDF read successfully" } else { "Image read successfully" }, ctx.display(&path));
            out.attachments.push(FilePart {
                mime: mime.into(),
                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                name: given.clone(),
            });
            out.metadata = json!({ "truncated": false });
            return Ok(out);
        }
        if is_binary(&path, &bytes) {
            return Err(anyhow!("Cannot read binary file: {given}"));
        }
        let text = String::from_utf8_lossy(&bytes);
        let (text, _) = crate::util::strip_bom(&text);
        let text = text.replace("\r\n", "\n");
        let all: Vec<&str> = if text.is_empty() { vec![] } else { text.strip_suffix('\n').unwrap_or(&text).split('\n').collect() };
        if offset > all.len() && !all.is_empty() {
            return Err(anyhow!("Offset {offset} is out of range for this file ({} lines)", all.len()));
        }
        let mut lines = Vec::new();
        let mut bytes_used = 0;
        let mut truncated = false;
        for (i, l) in all.iter().enumerate().skip(offset - 1).take(limit) {
            let l = if l.chars().count() > MAX_LINE { format!("{}... (line truncated to {MAX_LINE} chars)", l.chars().take(MAX_LINE).collect::<String>()) } else { l.to_string() };
            let row = format!("{}: {l}", i + 1);
            if bytes_used + row.len() > MAX_BYTES && !lines.is_empty() {
                truncated = true;
                break;
            }
            bytes_used += row.len() + 1;
            lines.push(row);
        }
        let shown = lines.len();
        if offset - 1 + shown < all.len() {
            truncated = true;
        }
        let mut out_lines = vec![if shown == 0 {
            format!("Read file {given}, 0 lines")
        } else {
            format!("Read file {given}, lines {offset}-{}", offset + shown - 1)
        }];
        out_lines.extend(lines);
        if truncated {
            out_lines.push(format!("[Output truncated. Continue reading with offset: {}]", offset + shown));
        }
        let mut out = Output::text(out_lines.join("\n"), ctx.display(&path)).meta(json!({ "truncated": truncated, "lines": all.len() }));
        out.context = context;
        Ok(out)
    }
}
