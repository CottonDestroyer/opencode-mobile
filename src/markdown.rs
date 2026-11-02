//! Markdown to UI blocks.
//!
//! Slint's `StyledText` accepts inline markdown only (emphasis, code, links) and rejects headings,
//! tables and raw HTML, all of which model output is full of (`Vec<T>` alone parses as HTML). So we
//! parse with pulldown-cmark, split the document into blocks that the UI lays out itself, and
//! re-emit each block's inline content as escaped markdown that `StyledText` always accepts.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Block {
    /// 0 paragraph, 1 heading, 2 code block, 3 block quote, 4 rule, 5 list item.
    pub kind: i32,
    /// Inline markdown, safe to pass to `StyledText::from_markdown`.
    pub md: String,
    /// Plain text: the code for code blocks, otherwise the text used for copying and fallbacks.
    pub plain: String,
    pub level: i32,
    pub bullet: String,
    pub lang: String,
}

/// Backslash-escapes every ASCII punctuation character, which CommonMark always permits.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn code_span(s: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in s.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(longest + 1);
    let pad = if s.starts_with('`') || s.ends_with('`') || s.starts_with(' ') && s.ends_with(' ') && !s.trim().is_empty() { " " } else { "" };
    if s.trim().is_empty() {
        return escape(s);
    }
    format!("{fence}{pad}{s}{pad}{fence}")
}

fn link_dest(url: &str) -> String {
    url.replace(' ', "%20").replace('(', "%28").replace(')', "%29").replace('<', "%3C").replace('>', "%3E")
}

struct Builder {
    out: Vec<Block>,
    md: String,
    plain: String,
    quote: usize,
    lists: Vec<Option<u64>>,
    bullet: Option<String>,
    heading: Option<i32>,
    code: Option<(String, String)>,
    table: Option<(Vec<Vec<String>>, Vec<String>, String)>,
    links: Vec<String>,
}

impl Builder {
    fn flush(&mut self) {
        let md = std::mem::take(&mut self.md);
        let plain = std::mem::take(&mut self.plain);
        if let Some(level) = self.heading {
            if !plain.trim().is_empty() {
                self.out.push(Block { kind: 1, md: format!("**{}**", escape(plain.trim())), plain, level, ..Default::default() });
            }
            return;
        }
        if plain.trim().is_empty() && md.trim().is_empty() {
            return;
        }
        if !self.lists.is_empty() {
            let bullet = self.bullet.take().unwrap_or_default();
            self.out.push(Block { kind: 5, md, plain, level: self.lists.len() as i32 - 1, bullet, ..Default::default() });
            return;
        }
        let kind = if self.quote > 0 { 3 } else { 0 };
        self.out.push(Block { kind, md, plain, ..Default::default() });
    }

    fn text(&mut self, t: &str) {
        if let Some((_, buf)) = &mut self.code {
            buf.push_str(t);
            return;
        }
        if let Some((_, _, cell)) = &mut self.table {
            cell.push_str(t);
            return;
        }
        self.md.push_str(&escape(t));
        self.plain.push_str(t);
    }

    fn table_text(&mut self, t: &str) -> bool {
        if let Some((_, _, cell)) = &mut self.table {
            cell.push_str(t);
            return true;
        }
        false
    }
}

fn format_table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let mut out = String::new();
    for (ri, r) in rows.iter().enumerate() {
        let cells: Vec<String> = (0..cols)
            .map(|i| {
                let c = r.get(i).map(|s| s.as_str()).unwrap_or("");
                format!("{c}{}", " ".repeat(widths[i] - c.chars().count()))
            })
            .collect();
        out.push_str(cells.join(" │ ").trim_end());
        out.push('\n');
        if ri == 0 && rows.len() > 1 {
            let sep: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            out.push_str(&sep.join("─┼─"));
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

pub fn blocks(src: &str) -> Vec<Block> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut b = Builder { out: vec![], md: String::new(), plain: String::new(), quote: 0, lists: vec![], bullet: None, heading: None, code: None, table: None, links: vec![] };
    for ev in Parser::new_ext(src, opts) {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    b.flush();
                    b.heading = Some(match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        _ => 4,
                    });
                }
                Tag::BlockQuote(_) => {
                    b.flush();
                    b.quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    b.flush();
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => l.split_whitespace().next().unwrap_or("").to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    b.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    b.flush();
                    b.lists.push(start);
                }
                Tag::Item => {
                    b.flush();
                    let bullet = match b.lists.last_mut() {
                        Some(Some(n)) => {
                            let s = format!("{n}.");
                            *n += 1;
                            s
                        }
                        _ => "•".to_string(),
                    };
                    b.bullet = Some(bullet);
                }
                Tag::Table(_) => {
                    b.flush();
                    b.table = Some((vec![], vec![], String::new()));
                }
                Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
                Tag::Emphasis => {
                    if b.table.is_none() {
                        b.md.push('*');
                    }
                }
                Tag::Strong => {
                    if b.table.is_none() {
                        b.md.push_str("**");
                    }
                }
                Tag::Strikethrough => {
                    if b.table.is_none() {
                        b.md.push_str("~~");
                    }
                }
                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                    if b.table.is_none() {
                        if b.links.is_empty() {
                            b.md.push('[');
                        }
                        b.links.push(dest_url.to_string());
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => b.flush(),
                TagEnd::Heading(_) => {
                    b.flush();
                    b.heading = None;
                }
                TagEnd::BlockQuote(_) => {
                    b.flush();
                    b.quote = b.quote.saturating_sub(1);
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, code)) = b.code.take() {
                        let code = code.trim_end_matches('\n').to_string();
                        b.out.push(Block { kind: 2, plain: code, lang, ..Default::default() });
                    }
                }
                TagEnd::List(_) => {
                    b.flush();
                    b.lists.pop();
                }
                TagEnd::Item => b.flush(),
                TagEnd::TableCell => {
                    if let Some((_, row, cell)) = &mut b.table {
                        row.push(std::mem::take(cell).trim().to_string());
                    }
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some((rows, row, _)) = &mut b.table {
                        rows.push(std::mem::take(row));
                    }
                }
                TagEnd::Table => {
                    if let Some((rows, _, _)) = b.table.take() {
                        b.out.push(Block { kind: 2, plain: format_table(&rows), lang: "table".into(), ..Default::default() });
                    }
                }
                TagEnd::Emphasis => {
                    if b.table.is_none() {
                        b.md.push('*');
                    }
                }
                TagEnd::Strong => {
                    if b.table.is_none() {
                        b.md.push_str("**");
                    }
                }
                TagEnd::Strikethrough => {
                    if b.table.is_none() {
                        b.md.push_str("~~");
                    }
                }
                TagEnd::Link | TagEnd::Image => {
                    if b.table.is_none() {
                        if let Some(dest) = b.links.pop() {
                            if b.links.is_empty() {
                                b.md.push_str(&format!("]({})", link_dest(&dest)));
                            }
                        }
                    }
                }
                _ => {}
            },
            Event::Text(t) => b.text(&t),
            Event::Code(t) => {
                if !b.table_text(&t) {
                    b.md.push_str(&code_span(&t));
                    b.plain.push_str(&t);
                }
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                if !b.table_text(&t) {
                    b.md.push_str(&code_span(&t));
                    b.plain.push_str(&t);
                }
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                if b.code.is_some() {
                    b.text(&t);
                } else {
                    b.text(t.trim_end_matches('\n'));
                }
            }
            Event::SoftBreak => {
                if !b.table_text(" ") {
                    b.md.push('\n');
                    b.plain.push('\n');
                }
            }
            Event::HardBreak => {
                if !b.table_text(" ") {
                    b.md.push_str("\\\n");
                    b.plain.push('\n');
                }
            }
            Event::Rule => {
                b.flush();
                b.out.push(Block { kind: 4, ..Default::default() });
            }
            Event::TaskListMarker(done) => b.text(if done { "☑ " } else { "☐ " }),
            Event::FootnoteReference(r) => b.text(&format!("[^{r}]")),
        }
    }
    // A fence left open is normal mid-stream; emit what has arrived so far.
    if let Some((lang, code)) = b.code.take() {
        b.out.push(Block { kind: 2, plain: code.trim_end_matches('\n').to_string(), lang, ..Default::default() });
    }
    b.flush();
    b.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted(src: &str) -> Vec<Block> {
        let out = blocks(src);
        for b in &out {
            if b.kind != 2 && b.kind != 4 {
                if let Err(e) = slint::StyledText::from_markdown(&b.md) {
                    panic!("StyledText rejected {:?} (from {src:?}): {e:?}", b.md);
                }
            }
        }
        out
    }

    #[test]
    fn splits_blocks() {
        let b = accepted("# Title\n\nSome **bold** and `code` with a [link](https://x.y/a_b).\n\n```rust\nfn main() {}\n```\n\n- one\n- two\n  1. nested\n\n> quoted\n\n---\n\n| a | b |\n|---|---|\n| 1 | 22 |\n");
        let kinds: Vec<i32> = b.iter().map(|b| b.kind).collect();
        assert_eq!(kinds, vec![1, 0, 2, 5, 5, 5, 3, 4, 2]);
        assert_eq!(b[0].plain, "Title");
        assert!(b[1].md.contains("**bold**") && b[1].md.contains("`code`") && b[1].md.contains("](https://x.y/a_b)"));
        assert_eq!(b[2].lang, "rust");
        assert_eq!(b[2].plain, "fn main() {}");
        assert_eq!((b[5].bullet.as_str(), b[5].level), ("1.", 1));
        assert!(b[8].plain.contains("1 │ 22"));
    }

    #[test]
    fn hostile_inline_content_is_escaped() {
        for src in [
            "Use Vec<T> and <div>html</div> here",
            "a < b > c & d | e # f",
            "<details><summary>x</summary>\n\nbody\n</details>",
            "`` a ` b `` and ```` ``` ````",
            "* [x] done\n* [ ] todo",
            "1) one\n2) two",
            r"escape \* and \_ and 50%",
            "**unclosed bold and _emph",
            "line one  \nline two\nline three",
            "[outer ![img](i.png) link](http://a.b/(x))",
            "Footnote[^1].\n\n[^1]: note",
            "<font color='red'>x</font> <u>u</u>",
        ] {
            accepted(src);
        }
    }

    #[test]
    fn streaming_unterminated_fence() {
        let b = accepted("Here:\n\n```py\nprint(1)\n");
        assert_eq!(b.last().unwrap().kind, 2);
        assert_eq!(b.last().unwrap().plain, "print(1)");
    }
}
