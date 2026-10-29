//! Provider-neutral streaming interface over the supported wire protocols.

pub mod anthropic;
pub mod google;
pub mod openai_chat;
pub mod openai_responses;

use crate::models::{Model, Protocol, Provider};
use crate::session::{Message, ToolState, Usage};
use anyhow::{anyhow, Result};
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub provider: Provider,
    pub model: Model,
    pub variant: Option<String>,
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// Output token limit; 0 leaves it to the provider (upstream sends none for title requests).
    pub max_tokens: u64,
    pub session: SessionRef,
}

/// Identifies the conversation a request belongs to; sent as client headers.
#[derive(Debug, Clone, Default)]
pub struct SessionRef {
    pub id: String,
    pub parent_id: Option<String>,
    pub project: String,
}

impl SessionRef {
    pub fn of(s: &crate::session::Session) -> Self {
        SessionRef { id: s.id.clone(), parent_id: s.parent_id.clone(), project: crate::session::upstream_project_id(&s.directory) }
    }
    /// Subagent sessions share their root session's affinity key.
    pub fn affinity(&self) -> &str {
        self.parent_id.as_deref().unwrap_or(&self.id)
    }
}

/// Session and client identification headers sent with every model request, shaped like the
/// TypeScript client's.
pub fn client_headers(req: &Request) -> Vec<(String, String)> {
    let s = &req.session;
    let mut h = vec![
        ("User-Agent".to_string(), crate::user_agent()),
        ("x-opencode-client".into(), crate::client_name()),
        ("x-opencode-session-id".into(), s.id.clone()),
        ("x-opencode-session".into(), s.affinity().to_string()),
        ("x-session-affinity".into(), s.affinity().to_string()),
        ("X-Session-Id".into(), s.affinity().to_string()),
        ("x-opencode-project".into(), s.project.clone()),
    ];
    if let Some(p) = &s.parent_id {
        h.push(("x-opencode-parent-session-id".into(), p.clone()));
        h.push(("x-parent-session-id".into(), p.clone()));
    }
    // OpenTelemetry propagation headers, as added by the TypeScript client's instrumented fetch.
    let trace = trace_id();
    let span = random_hex(8);
    h.push(("b3".into(), format!("{trace}-{span}-1-{}", random_hex(8))));
    h.push(("traceparent".into(), format!("00-{trace}-{span}-01")));
    h
}

fn random_hex(bytes: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..bytes).map(|_| format!("{:02x}", rng.gen::<u8>())).collect()
}

/// One trace id per process, mirroring upstream's run-level trace.
fn trace_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| random_hex(16))
}

/// The output-limit field an OpenAI-compatible endpoint expects. Upstream defaults to
/// `max_completion_tokens` and keeps `max_tokens` for these providers.
pub fn max_tokens_field(provider_id: &str, base_url: &str) -> &'static str {
    let i = provider_id.to_lowercase();
    let a = base_url.to_lowercase();
    let legacy = matches!(
        i.as_str(),
        "deepseek" | "moonshotai" | "togetherai" | "zai" | "zai-coding-plan" | "zhipuai" | "zhipuai-coding-plan" | "nvidia" | "cerebras" | "chutes" | "cloudflare-ai-gateway" | "cloudflare-workers-ai"
    ) || [
        "deepseek.com", "api.moonshot.ai", "api.together.", "api.z.ai", "open.bigmodel.cn", "integrate.api.nvidia.com", "cerebras.ai", "llm.chutes.ai", "gateway.ai.cloudflare.com", "api.cloudflare.com",
    ]
    .iter()
    .any(|h| a.contains(h));
    if legacy { "max_tokens" } else { "max_completion_tokens" }
}

#[derive(Debug, Clone)]
pub enum Event {
    TextDelta(String),
    ReasoningDelta(String),
    /// The current reasoning block is complete.
    ReasoningEnd { signature: Option<String>, redacted: Option<String>, item_id: Option<String> },
    /// A tool call's arguments are streaming in; for display only.
    ToolInputStart { id: String, name: String },
    ToolCall { id: String, name: String, input: Value, meta: Option<Value> },
    Usage(Usage),
    Finish(String),
    Retry(String),
}

pub type Tx = UnboundedSender<Event>;

pub const OUTPUT_TOKEN_MAX: u64 = 32_000;

pub fn max_output(model: &Model) -> u64 {
    let lim = if model.output == 0 { 8192 } else { model.output };
    lim.min(std::env::var("OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(OUTPUT_TOKEN_MAX))
}

pub async fn stream(req: Request, tx: Tx) -> Result<()> {
    match req.provider.protocol_for(&req.model)? {
        Protocol::Anthropic => anthropic::stream(req, tx).await,
        Protocol::OpenAIChat => openai_chat::stream(req, tx).await,
        Protocol::OpenAIResponses => openai_responses::stream(req, tx).await,
        Protocol::Google => google::stream(req, tx).await,
    }
}

/// POSTs with retries on rate limits, overload and server errors.
pub async fn post(
    tx: &Tx,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Result<wreq::Response> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let mut rb = crate::http::client().post(url).json(body);
        // `.json()` already sets the content type.
        for (k, v) in headers.iter().filter(|(k, _)| !k.eq_ignore_ascii_case("content-type")) {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let res = crate::http::send(rb).await;
        let (retryable, msg, wait) = match res {
            Ok(r) if r.status().is_success() => return Ok(r),
            Ok(r) => {
                let status = r.status();
                let retry_after = r
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());
                let text = r.text().await.unwrap_or_default();
                let msg = format!("{} {}", status.as_u16(), error_message(&text));
                let lower = text.to_lowercase();
                // Exhausted quota or billing is reported as 429 too, but retrying can't help.
                let quota = ["limit reached", "quota", "insufficient", "billing", "credit"].iter().any(|k| lower.contains(k));
                let retryable = (status.as_u16() == 429 && !quota) || status.as_u16() == 529 || status.is_server_error() || lower.contains("overloaded");
                (retryable, msg, retry_after)
            }
            Err(e) => (e.is_connect() || e.is_timeout(), format!("request failed: {e}"), None),
        };
        if !retryable || attempt >= 6 {
            return Err(anyhow!(msg));
        }
        let delay = wait.map(|s| s * 1000).unwrap_or(2000 * 2u64.pow(attempt - 1)).min(60_000);
        let _ = tx.send(Event::Retry(format!("{msg} — retrying in {}s (attempt {attempt})", delay / 1000)));
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
}

pub fn error_message(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        for ptr in ["/error/message", "/message", "/error", "/detail", "/0/error/message"] {
            if let Some(s) = v.pointer(ptr).and_then(|x| x.as_str()) {
                return s.to_string();
            }
        }
    }
    crate::util::truncate_chars(text.trim(), 500)
}

/// Server-sent events from a streaming response, yielded as `(event, data)`.
pub struct Sse {
    inner: futures::stream::BoxStream<'static, wreq::Result<Vec<u8>>>,
    buf: Vec<u8>,
}

impl Sse {
    pub fn new(resp: wreq::Response) -> Self {
        Sse { inner: resp.bytes_stream().map(|r| r.map(|b| b.to_vec())).boxed(), buf: Vec::new() }
    }

    pub async fn next(&mut self) -> Result<Option<(String, String)>> {
        loop {
            if let Some(ev) = self.take_event() {
                return Ok(Some(ev));
            }
            match self.inner.next().await {
                Some(Ok(chunk)) => self.buf.extend(chunk.into_iter().filter(|b| *b != b'\r')),
                Some(Err(e)) => return Err(anyhow!("stream error: {e}")),
                None => {
                    if !self.buf.iter().all(|b| b.is_ascii_whitespace()) {
                        self.buf.extend_from_slice(b"\n\n");
                        if let Some(ev) = self.take_event() {
                            return Ok(Some(ev));
                        }
                    }
                    return Ok(None);
                }
            }
        }
    }

    fn take_event(&mut self) -> Option<(String, String)> {
        loop {
            let idx = self.buf.windows(2).position(|w| w == b"\n\n")?;
            let raw: Vec<u8> = self.buf.drain(..idx + 2).collect();
            let block = String::from_utf8_lossy(&raw);
            let mut event = String::new();
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push(v.strip_prefix(' ').unwrap_or(v).to_string());
                }
            }
            if data.is_empty() && event.is_empty() {
                continue;
            }
            return Some((event, data.join("\n")));
        }
    }
}

/// Plain-text rendering of a tool result, as sent back to the model.
pub fn tool_result_text(state: &ToolState) -> (String, bool) {
    match state {
        ToolState::Completed { output, .. } => (if output.is_empty() { "(no output)".into() } else { output.clone() }, false),
        ToolState::Error { error } => (error.clone(), true),
        ToolState::Pending | ToolState::Running => ("Tool execution was interrupted".into(), true),
    }
}

pub fn compaction_text(summary: &str) -> String {
    format!(
        "<conversation-checkpoint>\nThe earlier conversation was compacted. This is the summary of it:\n<summary>\n{summary}\n</summary>\n</conversation-checkpoint>"
    )
}

/// Merges consecutive messages from the same role, in provider JSON form.
pub fn merge_consecutive(msgs: Vec<(String, Vec<Value>)>) -> Vec<(String, Vec<Value>)> {
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();
    for (role, content) in msgs {
        if content.is_empty() {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if last.0 == role {
                last.1.extend(content);
                continue;
            }
        }
        out.push((role, content));
    }
    out
}

/// Parses tool input JSON that may have been truncated.
pub fn parse_tool_input(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(raw).unwrap_or_else(|e| serde_json::json!({ "__invalid_json": raw, "__error": e.to_string() }))
}
