//! OpenAI-compatible Chat Completions (`POST /chat/completions`, streaming), used by most third-party
//! providers: OpenRouter, Groq, DeepSeek, xAI, local servers and so on.

use super::{Event, Request, Sse, Tx};
use crate::session::{Part, Role, ToolState, Usage};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn data_url(f: &crate::session::FilePart) -> String {
    format!("data:{};base64,{}", f.mime, f.data)
}

fn convert(req: &Request) -> Vec<Value> {
    let mut out = Vec::new();
    let system = req.system.iter().filter(|s| !s.trim().is_empty()).cloned().collect::<Vec<_>>().join("\n\n");
    if !system.is_empty() {
        out.push(json!({ "role": "system", "content": system }));
    }
    let interleaved = req.model.interleaved_field.clone();
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut parts = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text { text, .. } if !text.is_empty() => parts.push(json!({ "type": "text", "text": text })),
                        Part::File(f) if f.mime.starts_with("image/") => {
                            parts.push(json!({ "type": "image_url", "image_url": { "url": data_url(f) } }))
                        }
                        Part::File(f) if f.mime == "application/pdf" => parts.push(json!({
                            "type": "file", "file": { "filename": f.name, "file_data": data_url(f) }
                        })),
                        Part::Compaction { summary } => parts.push(json!({ "type": "text", "text": super::compaction_text(summary) })),
                        _ => {}
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                let only_text = parts.iter().all(|p| p["type"] == "text");
                let content = if only_text {
                    Value::String(parts.iter().map(|p| p["text"].as_str().unwrap_or("")).collect::<Vec<_>>().join("\n\n"))
                } else {
                    Value::Array(parts)
                };
                out.push(json!({ "role": "user", "content": content }));
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut reasoning = String::new();
                let mut calls = Vec::new();
                let mut results = Vec::new();
                let mut images = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text { text: t, .. } => text.push_str(t),
                        Part::Reasoning { text: t, .. } => reasoning.push_str(t),
                        Part::Tool { id, name, input, state, .. } => {
                            let args = if input.get("__invalid_json").is_some() { "{}".to_string() } else { input.to_string() };
                            calls.push(json!({ "id": id, "type": "function", "function": { "name": name, "arguments": args } }));
                            let (t, _) = super::tool_result_text(state);
                            results.push(json!({ "role": "tool", "tool_call_id": id, "content": t }));
                            if let ToolState::Completed { attachments, .. } = state {
                                for a in attachments.iter().filter(|a| a.mime.starts_with("image/")) {
                                    images.push(json!({ "type": "image_url", "image_url": { "url": data_url(a) } }));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut msg = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { Value::String(text) } });
                if !calls.is_empty() {
                    msg["tool_calls"] = Value::Array(calls);
                }
                if let Some(field) = &interleaved {
                    if !reasoning.is_empty() {
                        msg[field.as_str()] = Value::String(reasoning);
                    }
                }
                out.push(msg);
                out.extend(results);
                if !images.is_empty() {
                    let mut content = vec![json!({ "type": "text", "text": "Attached images from the tool results above:" })];
                    content.extend(images);
                    out.push(json!({ "role": "user", "content": content }));
                }
            }
        }
    }
    out
}

/// Endpoints that accept `store`; upstream sends them `store: false`.
fn supports_store(provider_id: &str, base_url: &str) -> bool {
    let i = provider_id.to_lowercase();
    let a = base_url.to_lowercase();
    i == "openai"
        || i == "azure"
        || ["api.openai.com", ".openai.azure.com", ".cognitiveservices.azure.com", ".services.ai.azure.com"].iter().any(|h| a.contains(h))
}

/// Endpoints that accept `strict` on tool definitions; upstream sends `strict: false`.
fn supports_strict(provider_id: &str, base_url: &str) -> bool {
    let i = provider_id.to_lowercase();
    let a = base_url.to_lowercase();
    let moonshot = i == "moonshotai" || i == "moonshotai-cn" || a.contains("api.moonshot.");
    let together = i == "togetherai" || i == "together" || a.contains("api.together.");
    let cloudflare = i == "cloudflare-ai-gateway" || a.contains("gateway.ai.cloudflare.com");
    let nvidia = i == "nvidia" || a.contains("integrate.api.nvidia.com");
    !moonshot && !together && !cloudflare && !nvidia
}

/// Z.ai GLM models after 4.5 stream tool calls only when `tool_stream` is set.
fn zai_tool_stream(provider_id: &str, base_url: &str, model_id: &str) -> bool {
    let i = provider_id.to_lowercase();
    let a = base_url.to_lowercase();
    let zai = matches!(i.as_str(), "zai" | "zai-coding-plan" | "zhipuai" | "zhipuai-coding-plan")
        || a.contains("api.z.ai")
        || a.contains("open.bigmodel.cn");
    zai && !matches!(model_id.to_lowercase().as_str(), "glm-4.5" | "glm-4.5-air" | "glm-4.5-flash" | "glm-4.5v")
}

pub async fn stream(req: Request, tx: Tx) -> Result<()> {
    let base = req
        .provider
        .base_url(&req.model)
        .ok_or_else(|| anyhow!("provider {} has no API URL; set provider.{}.options.baseURL", req.provider.id, req.provider.id))?;
    let url = if base.ends_with("/chat/completions") { base.clone() } else { format!("{base}/chat/completions") };

    // Field order follows the upstream request schema.
    let messages = convert(&req);
    let has_tool_calls = messages.iter().any(|m| m.get("tool_calls").is_some());
    let mut body = json!({ "model": req.model.id, "messages": messages });
    if !req.tools.is_empty() {
        let strict = supports_strict(&req.provider.id, &base);
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    let mut f = json!({ "name": t.name, "description": t.description, "parameters": t.schema });
                    if strict {
                        f["strict"] = json!(false);
                    }
                    json!({ "type": "function", "function": f })
                })
                .collect(),
        );
    } else if has_tool_calls {
        body["tools"] = json!([]);
    }
    body["stream"] = json!(true);
    body["stream_options"] = json!({ "include_usage": true });
    if supports_store(&req.provider.id, &base) {
        body["store"] = json!(false);
    }
    if let Some(v) = req.variant.as_deref() {
        if v != "off" && req.model.reasoning && req.provider.id != "openrouter" {
            body["reasoning_effort"] = json!(v);
        }
    }
    if !req.tools.is_empty() && zai_tool_stream(&req.provider.id, &base, &req.model.id) {
        body["tool_stream"] = json!(true);
    }
    if req.max_tokens > 0 {
        body[super::max_tokens_field(&req.provider.id, &base)] = json!(req.max_tokens);
    }
    if req.model.temperature {
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = req.top_p {
            body["top_p"] = json!(p);
        }
    }
    if let Some(v) = req.variant.as_deref() {
        if v != "off" && req.model.reasoning && req.provider.id == "openrouter" {
            body["reasoning"] = json!({ "effort": v });
        }
    }
    if req.provider.id == "openrouter" && req.variant.is_none() && req.model.reasoning {
        body["reasoning"] = json!({ "enabled": true });
    }
    for (k, v) in &req.provider.options.extra {
        if !matches!(k.as_str(), "apiKey" | "baseURL" | "headers" | "timeout") {
            body[k] = v.clone();
        }
    }
    if let Value::Object(extra) = &req.model.options {
        for (k, v) in extra {
            body[k] = v.clone();
        }
    }

    let mut headers: Vec<(String, String)> = vec![("content-type".into(), "application/json".into())];
    headers.extend(super::client_headers(&req));
    if let Some(k) = req.provider.key.as_deref().filter(|k| !k.is_empty()) {
        headers.push(("authorization".into(), format!("Bearer {k}")));
    }
    if req.provider.id == "openrouter" {
        headers.push(("HTTP-Referer".into(), "https://opencode.ai/".into()));
        headers.push(("X-Title".into(), "opencode".into()));
    }
    for (k, v) in req.provider.options.headers.iter().chain(req.model.headers.iter()) {
        headers.push((k.clone(), v.clone()));
    }

    let resp = super::post(&tx, &url, &headers, &body).await?;
    let mut sse = Sse::new(resp);

    struct Call {
        id: String,
        name: String,
        args: String,
    }
    let mut calls: BTreeMap<u64, Call> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut finish: Option<String> = None;
    let mut in_reasoning = false;

    while let Some((_, data)) = sse.next().await? {
        if data.trim() == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(|m| m.as_str()).map(String::from).unwrap_or_else(|| err.to_string());
            return Err(anyhow!("{msg}"));
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            let prompt = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
            let cached = u
                .pointer("/prompt_tokens_details/cached_tokens")
                .or_else(|| u.get("prompt_cache_hit_tokens"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            usage.input = prompt.saturating_sub(cached);
            usage.cache_read = cached;
            usage.output = u.get("completion_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
            usage.reasoning = u.pointer("/completion_tokens_details/reasoning_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
        }
        let Some(choice) = v.pointer("/choices/0") else { continue };
        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
        let reasoning = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .or_else(|| delta.get("reasoning_text"))
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty());
        if let Some(r) = reasoning {
            in_reasoning = true;
            let _ = tx.send(Event::ReasoningDelta(r.into()));
        }
        if let Some(t) = delta.get("content").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
            if in_reasoning {
                in_reasoning = false;
                let _ = tx.send(Event::ReasoningEnd { signature: None, redacted: None, item_id: None });
            }
            let _ = tx.send(Event::TextDelta(t.into()));
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|x| x.as_array()) {
            if in_reasoning {
                in_reasoning = false;
                let _ = tx.send(Event::ReasoningEnd { signature: None, redacted: None, item_id: None });
            }
            for (pos, tc) in tcs.iter().enumerate() {
                let idx = tc.get("index").and_then(|x| x.as_u64()).unwrap_or(pos as u64);
                let entry = calls.entry(idx).or_insert_with(|| Call { id: String::new(), name: String::new(), args: String::new() });
                if let Some(id) = tc.get("id").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
                    entry.id = id.to_string();
                }
                if let Some(n) = tc.pointer("/function/name").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
                    if entry.name.is_empty() {
                        let _ = tx.send(Event::ToolInputStart { id: entry.id.clone(), name: n.to_string() });
                    }
                    entry.name.push_str(if entry.name.is_empty() { n } else { "" });
                }
                if let Some(a) = tc.pointer("/function/arguments").and_then(|x| x.as_str()) {
                    entry.args.push_str(a);
                }
            }
        }
        if let Some(f) = choice.get("finish_reason").and_then(|x| x.as_str()) {
            finish = Some(f.to_string());
        }
    }
    if in_reasoning {
        let _ = tx.send(Event::ReasoningEnd { signature: None, redacted: None, item_id: None });
    }
    let had_calls = !calls.is_empty();
    for (_, c) in calls {
        let id = if c.id.is_empty() { crate::util::id("call") } else { c.id };
        let _ = tx.send(Event::ToolCall { id, name: c.name, input: super::parse_tool_input(&c.args), meta: None });
    }
    let _ = tx.send(Event::Usage(usage));
    let f = match finish.as_deref() {
        Some("tool_calls") | Some("function_call") => "tool-calls",
        Some("length") => "length",
        Some("content_filter") => "refusal",
        _ if had_calls => "tool-calls",
        Some(_) => "stop",
        None => "unknown",
    };
    let _ = tx.send(Event::Finish(f.into()));
    Ok(())
}
