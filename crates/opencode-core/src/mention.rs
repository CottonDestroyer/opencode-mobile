//! File attachments and `@path` mentions.

use crate::app::App;
use crate::session::{FilePart, Part};
use anyhow::{anyhow, Result};
use base64::Engine;
use std::path::Path;

/// Builds the message parts for an attached file or directory.
pub fn attach(path: &Path, display: &str) -> Result<Vec<Part>> {
    if path.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(path)?
            .flatten()
            .map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                if e.path().is_dir() { format!("{n}/") } else { n }
            })
            .collect();
        names.sort();
        return Ok(vec![Part::Text { text: format!("\n\nAttached directory: {}\n\n{}", path.display(), names.join("\n")), synthetic: true }]);
    }
    let bytes = std::fs::read(path).map_err(|e| anyhow!("cannot read {display}: {e}"))?;
    let mime = mime_guess::from_path(path).first_or_octet_stream().essence_str().to_string();
    if mime.starts_with("image/") || mime == "application/pdf" {
        return Ok(vec![
            Part::Text { text: format!("Attached file: {}", path.display()), synthetic: true },
            Part::File(FilePart { mime, data: base64::engine::general_purpose::STANDARD.encode(&bytes), name: display.to_string() }),
        ]);
    }
    let text = String::from_utf8_lossy(&bytes);
    Ok(vec![Part::Text { text: format!("\n\nAttached file: {}\n\n{}", path.display(), text), synthetic: true }])
}

/// Expands `@path` mentions into attachments.
pub fn expand_mentions(app: &App, text: &str) -> Vec<Part> {
    let re = regex::Regex::new(r"(?:^|\s)@([^\s]+)").unwrap();
    let mut parts = Vec::new();
    for c in re.captures_iter(text) {
        let raw = c[1].trim_end_matches([',', '.', ')', ';', ':']);
        let p = crate::util::resolve(&app.directory, raw);
        if p.exists() {
            if let Ok(mut a) = attach(&p, raw) {
                parts.append(&mut a);
            }
        }
    }
    parts
}

