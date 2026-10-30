//! `webfetch`: fetches a URL and returns it as markdown, text or HTML.

use super::{arg_str, arg_u64, req_str, Context, Output, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::Engine;
use futures::StreamExt;
use serde_json::{json, Value};

const MAX_BYTES: usize = 5 * 1024 * 1024;
const DEFAULT_TIMEOUT: u64 = 30;
const MAX_TIMEOUT: u64 = 120;
const USER_AGENT: &str = "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; OpenCode-User/1.0; +https://opencode.ai";

pub struct WebFetch;

pub fn html_to_markdown(html: &str) -> String {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "meta", "link", "noscript", "iframe", "object", "embed", "svg"])
        .build()
        .convert(html)
        .unwrap_or_else(|_| html.to_string())
}

pub fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 120)
}

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "webfetch"
    }
    fn description(&self) -> String {
        "Fetch content from an HTTP or HTTPS URL and return it as text, markdown, or HTML. Markdown is the default.\n\nUse a more targeted tool when one is available. This tool is read-only. Large text results may be replaced with a preview while the complete output is retained in managed storage.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The HTTP or HTTPS URL to fetch content from" },
                "format": { "type": "string", "enum": ["text", "markdown", "html"], "description": "The format to return the content in. Defaults to markdown." },
                "timeout": { "type": "number", "exclusiveMinimum": 0, "maximum": MAX_TIMEOUT, "description": format!("Optional timeout in seconds (maximum: {MAX_TIMEOUT})") }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let url = req_str(&input, "url")?.to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(anyhow!("URL must start with http:// or https://"));
        }
        let format = arg_str(&input, "format").unwrap_or("markdown").to_string();
        let timeout = arg_u64(&input, "timeout").unwrap_or(DEFAULT_TIMEOUT).clamp(1, MAX_TIMEOUT);
        ctx.ask("webfetch", &[url.clone()], &["*".into()], format!("Fetch {url}"), json!({ "url": url, "format": format })).await?;
        let accept = match format.as_str() {
            "markdown" => "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1",
            "text" => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
            _ => "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1",
        };
        let fetch = |ua: &'static str| {
            crate::http::send(
                crate::http::client()
                    .get(&url)
                    .header("User-Agent", ua)
                    .header("Accept", accept)
                    .header("Accept-Language", "en-US,en;q=0.9")
                    .timeout(std::time::Duration::from_secs(timeout)),
            )
        };
        let fetched = async {
            let resp = fetch(USER_AGENT).await?;
            // Upstream retries a Cloudflare challenge with a plain user agent.
            if resp.status().as_u16() == 403 && resp.headers().get("cf-mitigated").is_some_and(|v| v == "challenge") {
                return fetch("opencode").await;
            }
            Ok(resp)
        };
        let resp = tokio::select! {
            r = fetched => r.map_err(|e: wreq::Error| anyhow!("Request failed: {e}"))?,
            _ = ctx.cancel.cancelled() => return Err(anyhow!("Request cancelled")),
        };
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("Request failed with status code: {}", status.as_u16()));
        }
        let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let mime = ctype.split(';').next().unwrap_or("").trim().to_lowercase();
        if let Some(len) = resp.content_length() {
            if len as usize > MAX_BYTES {
                return Err(anyhow!("Response too large (exceeds 5MB limit)"));
            }
        }
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| anyhow!("Failed to read response: {e}"))?;
            body.extend_from_slice(&chunk);
            if body.len() > MAX_BYTES {
                return Err(anyhow!("Response too large (exceeds 5MB limit)"));
            }
        }
        let title = format!("{url} ({mime})");
        if mime.starts_with("image/") && mime != "image/svg+xml" {
            let mut out = Output::text("Image fetched successfully", title);
            out.attachments.push(crate::session::FilePart { mime: mime.clone(), data: base64::engine::general_purpose::STANDARD.encode(&body), name: url.clone() });
            return Ok(out);
        }
        let text = String::from_utf8_lossy(&body).to_string();
        let is_html = mime.contains("html") || (mime.is_empty() && text.trim_start().to_lowercase().starts_with("<!doctype html"));
        let content = match (format.as_str(), is_html) {
            ("markdown", true) => html_to_markdown(&text),
            ("text", true) => html_to_text(&text),
            _ => text,
        };
        Ok(Output::text(content, title).meta(json!({ "url": url, "contentType": ctype, "format": format })))
    }
}
