//! `edit`: exact find-and-replace, with fallbacks that tolerate typographic and whitespace
//! differences. Preserves the file's line endings and BOM.

use super::{arg_bool, req_str, Context, Output, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct Edit;

fn norm_char(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        other => other,
    }
}

fn norm(s: &str) -> String {
    s.chars().map(norm_char).collect()
}

/// Non-overlapping exact matches, as byte ranges.
pub fn find_all(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(i) = hay[start..].find(needle) {
        let s = start + i;
        out.push((s, s + needle.len()));
        start = s + needle.len();
    }
    out
}

/// Matches after normalizing typographic quotes, dashes and spaces, aligned to char boundaries.
fn find_normalized(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    let hc: Vec<(usize, char)> = hay.char_indices().map(|(i, c)| (i, norm_char(c))).collect();
    let nc: Vec<char> = needle.chars().map(norm_char).collect();
    if nc.is_empty() || nc.len() > hc.len() {
        return vec![];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + nc.len() <= hc.len() {
        if hc[i].1 == nc[0] && hc[i..i + nc.len()].iter().zip(&nc).all(|(a, b)| a.1 == *b) {
            let end = if i + nc.len() < hc.len() { hc[i + nc.len()].0 } else { hay.len() };
            out.push((hc[i].0, end));
            i += nc.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Line-sequence matches that ignore trailing whitespace (and typographic variants).
fn find_lines(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    let ends_nl = needle.ends_with('\n');
    let mut want: Vec<&str> = needle.split('\n').collect();
    if ends_nl {
        want.pop();
    }
    if want.is_empty() {
        return vec![];
    }
    struct Line<'a> {
        start: usize,
        end: usize,
        text: &'a str,
        content_end: usize,
        newline: bool,
    }
    let mut lines = Vec::new();
    let mut pos = 0;
    for seg in hay.split_inclusive('\n') {
        let newline = seg.ends_with('\n');
        let text = if newline { &seg[..seg.len() - 1] } else { seg };
        let content_end = pos + text.len() - if text.ends_with('\r') { 1 } else { 0 };
        lines.push(Line { start: pos, end: pos + seg.len(), text, content_end, newline });
        pos += seg.len();
    }
    let key = |s: &str| norm(s.trim_end());
    let want: Vec<String> = want.iter().map(|s| key(s)).collect();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for i in 0..lines.len() {
        if i + want.len() > lines.len() {
            break;
        }
        if !lines[i..i + want.len()].iter().zip(&want).all(|(l, w)| key(l.text) == *w) {
            continue;
        }
        let last = &lines[i + want.len() - 1];
        if ends_nl && !last.newline {
            continue;
        }
        let range = (lines[i].start, if ends_nl { last.end } else { last.content_end });
        if out.iter().any(|(s, e)| *e > range.0 && *s < range.1) {
            continue;
        }
        out.push(range);
    }
    out
}

fn dominant_ending(s: &str, fallback: &'static str) -> &'static str {
    let crlf = s.matches("\r\n").count();
    let lf = s.matches('\n').count() - crlf;
    if crlf == lf {
        fallback
    } else if crlf > lf {
        "\r\n"
    } else {
        "\n"
    }
}

pub struct Plan {
    pub matches: Vec<(usize, usize)>,
    replacement_lf: String,
    file_ending: &'static str,
}

impl Plan {
    pub fn replacement(&self, original: &str, range: (usize, usize)) -> String {
        let slice = &original[range.0..range.1];
        let ending = dominant_ending(slice, self.file_ending);
        if ending == "\n" {
            self.replacement_lf.clone()
        } else {
            self.replacement_lf.replace('\n', ending)
        }
    }
}

/// Finds `old` in `text`, which may use CRLF, returning byte ranges into `text`.
pub fn plan(text: &str, old: &str, new: &str) -> Plan {
    let file_ending = dominant_ending(text, "\n");
    let lf = text.replace("\r\n", "\n");
    // Positions, in LF coordinates, where a CRLF was collapsed.
    let mut crlf_at = Vec::new();
    let mut removed = 0;
    for (i, _) in text.match_indices("\r\n") {
        crlf_at.push(i - removed);
        removed += 1;
    }
    let to_orig = |p: usize| p + crlf_at.partition_point(|&c| c < p);
    let old_lf = old.replace("\r\n", "\n");
    let new_lf = new.replace("\r\n", "\n");
    let mut m = find_all(&lf, &old_lf);
    if m.is_empty() {
        m = find_normalized(&lf, &old_lf);
    }
    if m.is_empty() {
        m = find_lines(&lf, &old_lf);
    }
    Plan { matches: m.into_iter().map(|(s, e)| (to_orig(s), to_orig(e))).collect(), replacement_lf: new_lf, file_ending }
}

pub fn apply(text: &str, plan: &Plan, ranges: &[(usize, usize)]) -> String {
    let mut out = text.to_string();
    for r in ranges.iter().rev() {
        let rep = plan.replacement(text, *r);
        out.replace_range(r.0..r.1, &rep);
    }
    out
}

#[async_trait]
impl Tool for Edit {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> String {
        "Edit the contents of a file by finding and replacing exact text. When editing text from Read output, preserve the exact indentation (tabs or spaces) and omit the line-number prefix, such as `1: `. Never include the prefix in oldString or newString. Write line breaks as \\n; the file's existing CRLF or LF line endings are matched and preserved. To change line endings themselves, make oldString and newString differ only in \\r; that edit applies to the exact bytes. If there is no exact match, the edit retries while ignoring typographic quote, dash, and space variants, then trailing whitespace on each line. The edit fails if oldString is not found. By default, oldString must identify a UNIQUE location. Multiple matches FAIL unless replaceAll is true. Add more surrounding context to disambiguate, or set replaceAll to true to replace every occurrence. Use replaceAll when the change should apply to every occurrence, such as renaming a variable.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to edit" },
                "oldString": { "type": "string", "description": "Exact text to find and replace" },
                "newString": { "type": "string", "description": "Text to replace oldString with (must differ from oldString)" },
                "replaceAll": { "type": "boolean", "description": "Whether to replace every occurrence of oldString. When false, oldString must match exactly once. Defaults to false." }
            },
            "required": ["path", "oldString", "newString"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let given = req_str(&input, "path").or_else(|_| req_str(&input, "filePath"))?;
        let old = req_str(&input, "oldString")?;
        let new = req_str(&input, "newString")?;
        let replace_all = arg_bool(&input, "replaceAll").unwrap_or(false);
        if old == new {
            return Err(anyhow!("No changes to apply: oldString and newString are identical."));
        }
        if old.is_empty() {
            return Err(anyhow!("oldString must not be empty. Use write to create or overwrite a file."));
        }
        let path = ctx.resolve(given);
        ctx.authorize_external(&path).await?;
        if path.is_dir() {
            return Err(anyhow!("Path is a directory, not a file: {given}"));
        }
        let raw = std::fs::read_to_string(&path).map_err(|_| anyhow!("File not found: {given}"))?;
        let (text, bom) = crate::util::strip_bom(&raw);
        let only_cr = old.replace('\r', "") == new.replace('\r', "");
        let (count, result) = if only_cr {
            let m = find_all(text, old);
            let ranges: Vec<_> = if replace_all { m.clone() } else { m.iter().take(1).cloned().collect() };
            let mut out = text.to_string();
            for r in ranges.iter().rev() {
                out.replace_range(r.0..r.1, new);
            }
            (m.len(), out)
        } else {
            let p = plan(text, old, new);
            let ranges: Vec<_> = if replace_all { p.matches.clone() } else { p.matches.iter().take(1).cloned().collect() };
            (p.matches.len(), apply(text, &p, &ranges))
        };
        let resource = ctx.display(&path);
        let diff = crate::util::unified_diff(&resource, text, &result);
        ctx.ask("edit", &[resource.clone()], &["*".into()], format!("Edit {resource}"), json!({ "filepath": resource, "diff": diff })).await?;
        if count == 0 {
            return Err(anyhow!("Could not find oldString in {given}. It must match exactly, including whitespace and indentation."));
        }
        if count > 1 && !replace_all {
            return Err(anyhow!("Found {count} matches for oldString, but expected exactly one. Add more surrounding context to make oldString unique, or set replaceAll to true to replace every occurrence."));
        }
        if result == text {
            return Err(anyhow!(if only_cr {
                format!("No changes to apply: {given} already has those line endings.")
            } else {
                format!("No changes to apply: the edit leaves {given} unchanged. Line endings follow the file; to change them, write \\r explicitly in oldString or newString.")
            }));
        }
        let mut data = String::new();
        if bom {
            data.push('\u{feff}');
        }
        data.push_str(&result);
        std::fs::write(&path, data)?;
        let replacements = if replace_all { count } else { 1 };
        let (add, del) = crate::util::diff_stats(text, &result);
        Ok(Output::text(format!("Edited {resource} ({replacements} replacement{})", if replacements == 1 { "" } else { "s" }), resource.clone())
            .meta(json!({ "files": [{ "file": resource, "diff": diff, "additions": add, "deletions": del, "status": "modified" }], "replacements": replacements })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crlf_preserved() {
        let text = "a\r\nb\r\nc\r\n";
        let p = plan(text, "b\nc", "x\ny");
        assert_eq!(p.matches.len(), 1);
        assert_eq!(apply(text, &p, &p.matches), "a\r\nx\r\ny\r\n");
    }
    #[test]
    fn typographic_and_trailing() {
        let text = "say \u{201C}hi\u{201D}\nfoo   \nbar\n";
        let p = plan(text, "say \"hi\"", "say 'yo'");
        assert_eq!(apply(text, &p, &p.matches), "say 'yo'\nfoo   \nbar\n");
        let p = plan(text, "foo\nbar", "baz");
        assert_eq!(p.matches.len(), 1);
        assert_eq!(apply(text, &p, &p.matches), "say \u{201C}hi\u{201D}\nbaz\n");
    }
}
