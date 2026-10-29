//! Anthropic Messages API (`POST /v1/messages`, streaming).

use super::{Event, Request, Sse, Tx};
use crate::models::Model;
use crate::session::{Part, Role, Usage};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn legacy_thinking(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    !id.contains("claude")
        || id.contains("claude-3")
        || id.contains("-4-5")
        || id.contains("-4-1")
        || id.contains("-4-0")
        || id.contains("sonnet-4-2")
        || id.contains("opus-4-2")
}

/// Returns `(thinking, effort, needs_interleaved_beta)`.
fn reasoning(model: &Model, variant: Option<&str>, max_tokens: u64) -> (Option<Value>, Option<String>, bool) {
    if !model.reasoning {
        return (None, None, false);
    }
    let effort_values: Vec<String> = model
        .reasoning_options
        .iter()
        .filter(|o| o.get("type").and_then(|t| t.as_str()) == Some("effort"))
        .flat_map(|o| o.get("values").and_then(|v| v.as_array()).cloned().unwrap_or_default())
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let effort = variant.filter(|v| effort_values.iter().any(|e| e == v)).map(String::from);
    if variant == Some("off") && model.has_reasoning_type("toggle") {
        return (Some(json!({ "type": "disabled" })), None, false);
    }
    if legacy_thinking(model) {
        let Some(v) = variant else { return (None, None, false) };
        let budget = match v {
            "low" | "minimal" => 4_000,
            "medium" => 12_000,
            _ => 24_000,
        }
        .min(max_tokens.saturating_sub(1))
        .max(1024);
        let effort = if model.has_reasoning_type("effort") { effort } else { None };
        return (Some(json!({ "type": "enabled", "budget_tokens": budget })), effort, true);
    }
    (Some(json!({ "type": "adaptive", "display": "summarized" })), effort, false)
}

fn sanitize_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

fn file_block(f: &crate::session::FilePart) -> Option<Value> {
    if f.mime.starts_with("image/") {
        Some(json!({ "type": "image", "source": { "type": "base64", "media_type": f.mime, "data": f.data } }))
    } else if f.mime == "application/pdf" {
        Some(json!({ "type": "document", "source": { "type": "base64", "media_type": f.mime, "data": f.data } }))
    } else {
        None
    }
}

fn convert(req: &Request) -> Vec<Value> {
    let mut msgs: Vec<(String, Vec<Value>)> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut content = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text { text, .. } if !text.is_empty() => content.push(json!({ "type": "text", "text": text })),
                        Part::File(f) => {
                            if let Some(b) = file_block(f) {
                                content.push(b);
                            }
                        }
                        Part::Compaction { summary } => content.push(json!({ "type": "text", "text": super::compaction_text(summary) })),
                        _ => {}
                    }
                }
                msgs.push(("user".into(), content));
            }
            Role::Assistant => {
                let mut content = Vec::new();
                let mut results = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Reasoning { text, signature: Some(sig), .. } => {
                            content.push(json!({ "type": "thinking", "thinking": text, "signature": sig }))
                        }
                        Part::Reasoning { redacted: Some(data), signature: None, .. } => {
                            content.push(json!({ "type": "redacted_thinking", "data": data }))
                        }
                        Part::Text { text, .. } if !text.trim().is_empty() => content.push(json!({ "type": "text", "text": text })),
                        Part::Tool { id, name, input, state, .. } => {
                            let input = if input.is_object() && input.get("__invalid_json").is_none() { input.clone() } else { json!({}) };
                            let id = sanitize_id(id);
                            content.push(json!({ "type": "tool_use", "id": id, "name": name, "input": input }));
                            let (text, is_error) = super::tool_result_text(state);
                            let mut blocks = vec![json!({ "type": "text", "text": text })];
                            if let crate::session::ToolState::Completed { attachments, .. } = state {
                                for a in attachments {
                                    if let Some(b) = file_block(a) {
                                        blocks.push(b);
                                    }
                                }
                            }
                            let mut r = json!({ "type": "tool_result", "tool_use_id": id, "content": blocks });
                            if is_error {
                                r["is_error"] = json!(true);
                            }
                            results.push(r);
                        }
                        _ => {}
                    }
                }
                msgs.push(("assistant".into(), content));
                if !results.is_empty() {
                    msgs.push(("user".into(), results));
                }
            }
        }
    }
    let mut msgs = super::merge_consecutive(msgs);
    // The Messages API requires the conversation to start with a user turn.
    if msgs.first().map(|m| m.0.as_str()) == Some("assistant") {
        msgs.insert(0, ("user".into(), vec![json!({ "type": "text", "text": "(continue)" })]));
    }
    // Prompt caching: mark the last block of each of the final two messages.
    let n = msgs.len();
    for (i, (_, content)) in msgs.iter_mut().enumerate() {
        if i + 2 >= n {
            if let Some(b) = content.iter_mut().rev().find(|b| {
                let t = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                t != "thinking" && t != "redacted_thinking"
            }) {
                b["cache_control"] = json!({ "type": "ephemeral" });
            }
        }
    }
    msgs.into_iter().map(|(role, content)| json!({ "role": role, "content": content })).collect()
}

pub async fn stream(req: Request, tx: Tx) -> Result<()> {
    let base = req.provider.base_url(&req.model).ok_or_else(|| anyhow!("no base URL for {}", req.provider.id))?;
    let url = format!("{base}/messages");
    let first_party = base.contains("api.anthropic.com");
    let key = req.provider.key.clone().unwrap_or_default();
    let oauth = matches!(crate::auth::get(&req.provider.id), Some(crate::auth::Credential::Oauth { .. }));

    // The Messages API requires an output limit.
    let max_tokens = if req.max_tokens > 0 { req.max_tokens } else { super::max_output(&req.model) };
    let (thinking, effort, interleaved) = reasoning(&req.model, req.variant.as_deref(), max_tokens);
    let mut body = json!({
        "model": req.model.id,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": convert(&req),
    });
    let system: Vec<Value> = req
        .system
        .iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| json!({ "type": "text", "text": s }))
        .collect();
    if !system.is_empty() {
        let mut system = system;
        if let Some(last) = system.last_mut() {
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
        body["system"] = Value::Array(system);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    let mut v = json!({ "name": t.name, "description": t.description, "input_schema": t.schema });
                    if first_party {
                        v["eager_input_streaming"] = json!(true);
                    }
                    v
                })
                .collect(),
        );
    }
    if let Some(t) = thinking {
        body["thinking"] = t;
    }
    if let Some(e) = effort {
        body["output_config"] = json!({ "effort": e });
    }
    if !first_party {
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
    }
    if let Value::Object(extra) = &req.model.options {
        for (k, v) in extra {
            body[k] = v.clone();
        }
    }

    let mut headers: Vec<(String, String)> = vec![
        ("anthropic-version".into(), "2023-06-01".into()),
        ("content-type".into(), "application/json".into()),
    ];
    headers.extend(super::client_headers(&req));
    let mut betas: Vec<String> = Vec::new();
    if interleaved && !req.tools.is_empty() {
        betas.push("interleaved-thinking-2025-05-14".into());
    }
    if oauth {
        headers.push(("authorization".into(), format!("Bearer {key}")));
        betas.push("oauth-2025-04-20".into());
    } else {
        headers.push(("x-api-key".into(), key));
    }
    for (k, v) in req.provider.options.headers.iter().chain(req.model.headers.iter()) {
        if k.eq_ignore_ascii_case("anthropic-beta") {
            betas.extend(v.split(',').map(|s| s.trim().to_string()));
        } else {
            headers.push((k.clone(), v.clone()));
        }
    }
    if !betas.is_empty() {
        betas.dedup();
        headers.push(("anthropic-beta".into(), betas.join(",")));
    }

    let resp = super::post(&tx, &url, &headers, &body).await?;
    let mut sse = Sse::new(resp);

    enum Block {
        Text,
        Thinking { sig: String },
        Redacted(String),
        Tool { id: String, name: String, json: String },
        Other,
    }
    let mut blocks: BTreeMap<u64, Block> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut stop: Option<String> = None;

    while let Some((event, data)) = sse.next().await? {
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let v: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or(&event).to_string();
        match ty.as_str() {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    usage.input = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    usage.cache_read = u.get("cache_read_input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    usage.cache_write = u.get("cache_creation_input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    usage.output = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                }
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                let cb = v.get("content_block").cloned().unwrap_or(Value::Null);
                let block = match cb.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = cb.get("text").and_then(|t| t.as_str()).filter(|t| !t.is_empty()) {
                            let _ = tx.send(Event::TextDelta(t.into()));
                        }
                        Block::Text
                    }
                    Some("thinking") => Block::Thinking { sig: String::new() },
                    Some("redacted_thinking") => Block::Redacted(cb.get("data").and_then(|d| d.as_str()).unwrap_or("").into()),
                    Some("tool_use") => {
                        let id = cb.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let name = cb.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let _ = tx.send(Event::ToolInputStart { id: id.clone(), name: name.clone() });
                        Block::Tool { id, name, json: String::new() }
                    }
                    _ => Block::Other,
                };
                blocks.insert(idx, block);
            }
            "content_block_delta" => {
                let idx = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                let d = v.get("delta").cloned().unwrap_or(Value::Null);
                match (d.get("type").and_then(|t| t.as_str()), blocks.get_mut(&idx)) {
                    (Some("text_delta"), _) => {
                        if let Some(t) = d.get("text").and_then(|t| t.as_str()) {
                            let _ = tx.send(Event::TextDelta(t.into()));
                        }
                    }
                    (Some("thinking_delta"), _) => {
                        if let Some(t) = d.get("thinking").and_then(|t| t.as_str()) {
                            let _ = tx.send(Event::ReasoningDelta(t.into()));
                        }
                    }
                    (Some("signature_delta"), Some(Block::Thinking { sig })) => {
                        sig.push_str(d.get("signature").and_then(|s| s.as_str()).unwrap_or(""));
                    }
                    (Some("input_json_delta"), Some(Block::Tool { json, .. })) => {
                        json.push_str(d.get("partial_json").and_then(|s| s.as_str()).unwrap_or(""));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let idx = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                match blocks.remove(&idx) {
                    Some(Block::Thinking { sig }) => {
                        let _ = tx.send(Event::ReasoningEnd { signature: Some(sig).filter(|s| !s.is_empty()), redacted: None, item_id: None });
                    }
                    Some(Block::Redacted(data)) => {
                        let _ = tx.send(Event::ReasoningEnd { signature: None, redacted: Some(data), item_id: None });
                    }
                    Some(Block::Tool { id, name, json }) => {
                        let _ = tx.send(Event::ToolCall { id, name, input: super::parse_tool_input(&json), meta: None });
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(|x| x.as_str()) {
                    stop = Some(r.to_string());
                }
                if let Some(u) = v.get("usage") {
                    if let Some(o) = u.get("output_tokens").and_then(|x| x.as_u64()) {
                        usage.output = o;
                    }
                    if let Some(i) = u.get("input_tokens").and_then(|x| x.as_u64()) {
                        usage.input = i;
                    }
                    if let Some(i) = u.get("cache_read_input_tokens").and_then(|x| x.as_u64()) {
                        usage.cache_read = i;
                    }
                    if let Some(i) = u.get("cache_creation_input_tokens").and_then(|x| x.as_u64()) {
                        usage.cache_write = i;
                    }
                }
            }
            "message_stop" => break,
            "error" => {
                let msg = v.pointer("/error/message").and_then(|x| x.as_str()).unwrap_or("unknown error");
                return Err(anyhow!("{msg}"));
            }
            _ => {}
        }
    }
    let _ = tx.send(Event::Usage(usage));
    let finish = match stop.as_deref() {
        Some("tool_use") => "tool-calls",
        Some("max_tokens") => "length",
        Some("refusal") => "refusal",
        Some("pause_turn") => "pause",
        Some(_) => "stop",
        None => "unknown",
    };
    let _ = tx.send(Event::Finish(finish.into()));
    Ok(())
}
