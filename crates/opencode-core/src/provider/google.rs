//! Google Gemini API (`models/{id}:streamGenerateContent?alt=sse`).

use super::{Event, Request, Sse, Tx};
use crate::session::{Part, Role, ToolState, Usage};
use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

/// Gemini accepts only an OpenAPI subset of JSON Schema.
fn clean_schema(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, val) in m {
                if matches!(k.as_str(), "additionalProperties" | "$schema" | "default" | "examples" | "$ref" | "$defs" | "const") {
                    continue;
                }
                out.insert(k.clone(), clean_schema(val));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(clean_schema).collect()),
        other => other.clone(),
    }
}

fn convert(req: &Request) -> Vec<Value> {
    let mut contents: Vec<(String, Vec<Value>)> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut parts = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text { text, .. } if !text.is_empty() => parts.push(json!({ "text": text })),
                        Part::File(f) if f.mime.starts_with("image/") || f.mime == "application/pdf" => {
                            parts.push(json!({ "inlineData": { "mimeType": f.mime, "data": f.data } }))
                        }
                        Part::Compaction { summary } => parts.push(json!({ "text": super::compaction_text(summary) })),
                        _ => {}
                    }
                }
                contents.push(("user".into(), parts));
            }
            Role::Assistant => {
                let mut parts = Vec::new();
                let mut results = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text { text, .. } if !text.is_empty() => parts.push(json!({ "text": text })),
                        Part::Tool { name, input, state, provider_meta, .. } => {
                            let args = if input.is_object() && input.get("__invalid_json").is_none() { input.clone() } else { json!({}) };
                            let mut call = json!({ "functionCall": { "name": name, "args": args } });
                            if let Some(sig) = provider_meta.as_ref().and_then(|m| m.get("thoughtSignature")) {
                                call["thoughtSignature"] = sig.clone();
                            }
                            parts.push(call);
                            let (t, is_err) = super::tool_result_text(state);
                            let mut response = Map::new();
                            response.insert(if is_err { "error" } else { "content" }.into(), Value::String(t));
                            results.push(json!({ "functionResponse": { "name": name, "response": response } }));
                            if let ToolState::Completed { attachments, .. } = state {
                                for a in attachments {
                                    results.push(json!({ "inlineData": { "mimeType": a.mime, "data": a.data } }));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                contents.push(("model".into(), parts));
                if !results.is_empty() {
                    contents.push(("user".into(), results));
                }
            }
        }
    }
    super::merge_consecutive(contents).into_iter().map(|(role, parts)| json!({ "role": role, "parts": parts })).collect()
}

pub async fn stream(req: Request, tx: Tx) -> Result<()> {
    let base = req.provider.base_url(&req.model).ok_or_else(|| anyhow!("no base URL for {}", req.provider.id))?;
    let url = format!("{base}/models/{}:streamGenerateContent?alt=sse", req.model.id);
    let mut generation = if req.max_tokens > 0 { json!({ "maxOutputTokens": req.max_tokens }) } else { json!({}) };
    if req.model.temperature {
        if let Some(t) = req.temperature {
            generation["temperature"] = json!(t);
        }
    }
    if req.model.reasoning {
        let mut tc = json!({ "includeThoughts": true });
        match req.variant.as_deref() {
            Some("off") => tc = json!({ "thinkingBudget": 0 }),
            Some(v) if req.model.id.contains("gemini-3") => tc["thinkingLevel"] = json!(v),
            Some("low") => tc["thinkingBudget"] = json!(2048),
            Some("medium") => tc["thinkingBudget"] = json!(8192),
            Some("high") => tc["thinkingBudget"] = json!(24576),
            _ => {}
        }
        generation["thinkingConfig"] = tc;
    }
    let mut body = json!({ "contents": convert(&req), "generationConfig": generation });
    let system = req.system.iter().filter(|s| !s.trim().is_empty()).map(|s| json!({ "text": s })).collect::<Vec<_>>();
    if !system.is_empty() {
        body["systemInstruction"] = json!({ "parts": system });
    }
    if !req.tools.is_empty() {
        let decls: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "parameters": clean_schema(&t.schema) }))
            .collect();
        body["tools"] = json!([{ "functionDeclarations": decls }]);
    }
    let mut headers: Vec<(String, String)> = vec![("content-type".into(), "application/json".into())];
    headers.extend(super::client_headers(&req));
    if let Some(k) = &req.provider.key {
        headers.push(("x-goog-api-key".into(), k.clone()));
    }
    for (k, v) in req.provider.options.headers.iter().chain(req.model.headers.iter()) {
        headers.push((k.clone(), v.clone()));
    }

    let resp = super::post(&tx, &url, &headers, &body).await?;
    let mut sse = Sse::new(resp);
    let mut usage = Usage::default();
    let mut finish = "unknown".to_string();
    let mut had_calls = false;
    let mut in_thought = false;

    while let Some((_, data)) = sse.next().await? {
        let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
        if let Some(err) = v.get("error") {
            return Err(anyhow!("{}", err.get("message").and_then(|m| m.as_str()).unwrap_or("gemini error")));
        }
        if let Some(u) = v.get("usageMetadata") {
            let prompt = u.get("promptTokenCount").and_then(|x| x.as_u64()).unwrap_or(0);
            let cached = u.get("cachedContentTokenCount").and_then(|x| x.as_u64()).unwrap_or(0);
            usage.input = prompt.saturating_sub(cached);
            usage.cache_read = cached;
            usage.reasoning = u.get("thoughtsTokenCount").and_then(|x| x.as_u64()).unwrap_or(0);
            usage.output = u.get("candidatesTokenCount").and_then(|x| x.as_u64()).unwrap_or(0) + usage.reasoning;
        }
        let Some(cand) = v.pointer("/candidates/0") else { continue };
        for p in cand.pointer("/content/parts").and_then(|x| x.as_array()).into_iter().flatten() {
            if let Some(fc) = p.get("functionCall") {
                if in_thought {
                    in_thought = false;
                    let _ = tx.send(Event::ReasoningEnd { signature: None, redacted: None, item_id: None });
                }
                had_calls = true;
                let name = fc.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let meta = p.get("thoughtSignature").map(|s| json!({ "thoughtSignature": s }));
                let _ = tx.send(Event::ToolCall {
                    id: fc.get("id").and_then(|x| x.as_str()).map(String::from).unwrap_or_else(|| crate::util::id("call")),
                    name,
                    input: fc.get("args").cloned().unwrap_or(json!({})),
                    meta,
                });
                continue;
            }
            if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                if p.get("thought").and_then(|x| x.as_bool()) == Some(true) {
                    in_thought = true;
                    let _ = tx.send(Event::ReasoningDelta(t.into()));
                } else {
                    if in_thought {
                        in_thought = false;
                        let _ = tx.send(Event::ReasoningEnd { signature: None, redacted: None, item_id: None });
                    }
                    let _ = tx.send(Event::TextDelta(t.into()));
                }
            }
        }
        if let Some(f) = cand.get("finishReason").and_then(|x| x.as_str()) {
            finish = match f {
                "MAX_TOKENS" => "length",
                "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "BLOCKLIST" => "refusal",
                _ if had_calls => "tool-calls",
                _ => "stop",
            }
            .into();
        }
    }
    if in_thought {
        let _ = tx.send(Event::ReasoningEnd { signature: None, redacted: None, item_id: None });
    }
    if had_calls && finish != "length" {
        finish = "tool-calls".into();
    }
    let _ = tx.send(Event::Usage(usage));
    let _ = tx.send(Event::Finish(finish));
    Ok(())
}
