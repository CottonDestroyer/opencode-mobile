//! OpenAI Responses API (`POST /responses`, streaming), used for OpenAI's own models.

use super::{Event, Request, Sse, Tx};
use crate::session::{Part, Role, ToolState, Usage};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

fn data_url(f: &crate::session::FilePart) -> String {
    format!("data:{};base64,{}", f.mime, f.data)
}

fn convert(req: &Request) -> Vec<Value> {
    let mut items = Vec::new();
    for s in req.system.iter().filter(|s| !s.trim().is_empty()) {
        items.push(json!({ "role": "developer", "content": s }));
    }
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut content = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text { text, .. } if !text.is_empty() => content.push(json!({ "type": "input_text", "text": text })),
                        Part::File(f) if f.mime.starts_with("image/") => {
                            content.push(json!({ "type": "input_image", "image_url": data_url(f) }))
                        }
                        Part::File(f) if f.mime == "application/pdf" => {
                            content.push(json!({ "type": "input_file", "filename": f.name, "file_data": data_url(f) }))
                        }
                        Part::Compaction { summary } => content.push(json!({ "type": "input_text", "text": super::compaction_text(summary) })),
                        _ => {}
                    }
                }
                if !content.is_empty() {
                    items.push(json!({ "role": "user", "content": content }));
                }
            }
            Role::Assistant => {
                let same_provider = m.model.as_deref().is_some_and(|x| x.starts_with(&format!("{}/", req.provider.id)));
                for p in &m.parts {
                    match p {
                        Part::Reasoning { text, redacted: Some(enc), .. } if same_provider => {
                            let summary: Vec<Value> = if text.is_empty() { vec![] } else { vec![json!({ "type": "summary_text", "text": text })] };
                            items.push(json!({ "type": "reasoning", "summary": summary, "encrypted_content": enc }));
                        }
                        Part::Text { text, .. } if !text.is_empty() => {
                            items.push(json!({ "role": "assistant", "content": [{ "type": "output_text", "text": text }] }))
                        }
                        Part::Tool { id, name, input, state, .. } => {
                            let args = if input.get("__invalid_json").is_some() { "{}".to_string() } else { input.to_string() };
                            items.push(json!({ "type": "function_call", "call_id": id, "name": name, "arguments": args }));
                            let (t, _) = super::tool_result_text(state);
                            items.push(json!({ "type": "function_call_output", "call_id": id, "output": t }));
                            if let ToolState::Completed { attachments, .. } = state {
                                let imgs: Vec<Value> = attachments
                                    .iter()
                                    .filter(|a| a.mime.starts_with("image/"))
                                    .map(|a| json!({ "type": "input_image", "image_url": data_url(a) }))
                                    .collect();
                                if !imgs.is_empty() {
                                    let mut content = vec![json!({ "type": "input_text", "text": format!("Attachments from the {name} result:") })];
                                    content.extend(imgs);
                                    items.push(json!({ "role": "user", "content": content }));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    items
}

pub async fn stream(req: Request, tx: Tx) -> Result<()> {
    let base = req.provider.base_url(&req.model).ok_or_else(|| anyhow!("no base URL for {}", req.provider.id))?;
    let url = format!("{base}/responses");
    let mut body = json!({
        "model": req.model.id,
        "input": convert(&req),
        "stream": true,
        "store": false,
        "prompt_cache_key": req.session.affinity(),
    });
    if req.max_tokens > 0 {
        body["max_output_tokens"] = json!(req.max_tokens);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| json!({ "type": "function", "name": t.name, "description": t.description, "parameters": t.schema, "strict": false }))
                .collect(),
        );
    }
    if req.model.reasoning {
        let mut r = json!({ "summary": "auto" });
        if let Some(v) = req.variant.as_deref().filter(|v| *v != "off") {
            r["effort"] = json!(v);
        }
        body["reasoning"] = r;
        body["include"] = json!(["reasoning.encrypted_content"]);
    } else if req.model.temperature {
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
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
    headers.push(("session_id".into(), req.session.affinity().to_string()));
    for (k, v) in req.provider.options.headers.iter().chain(req.model.headers.iter()) {
        headers.push((k.clone(), v.clone()));
    }

    let resp = super::post(&tx, &url, &headers, &body).await?;
    let mut sse = Sse::new(resp);
    let mut usage = Usage::default();
    let mut finish = "unknown".to_string();
    let mut had_calls = false;
    let mut summary_parts = 0usize;

    while let Some((event, data)) = sse.next().await? {
        if data.trim() == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or(&event).to_string();
        match ty.as_str() {
            "response.output_text.delta" => {
                if let Some(d) = v.get("delta").and_then(|x| x.as_str()) {
                    let _ = tx.send(Event::TextDelta(d.into()));
                }
            }
            "response.reasoning_summary_part.added" => {
                if summary_parts > 0 {
                    let _ = tx.send(Event::ReasoningDelta("\n\n".into()));
                }
                summary_parts += 1;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = v.get("delta").and_then(|x| x.as_str()) {
                    let _ = tx.send(Event::ReasoningDelta(d.into()));
                }
            }
            "response.output_item.added" => {
                let item = v.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                    let _ = tx.send(Event::ToolInputStart {
                        id: item.get("call_id").and_then(|x| x.as_str()).unwrap_or("").into(),
                        name: item.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                    });
                }
                if item.get("type").and_then(|t| t.as_str()) == Some("reasoning") {
                    summary_parts = 0;
                }
            }
            "response.output_item.done" => {
                let item = v.get("item").cloned().unwrap_or(Value::Null);
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("function_call") => {
                        had_calls = true;
                        let args = item.get("arguments").and_then(|x| x.as_str()).unwrap_or("");
                        let _ = tx.send(Event::ToolCall {
                            id: item.get("call_id").and_then(|x| x.as_str()).unwrap_or("").into(),
                            name: item.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                            input: super::parse_tool_input(args),
                            meta: None,
                        });
                    }
                    Some("reasoning") => {
                        let _ = tx.send(Event::ReasoningEnd {
                            signature: None,
                            redacted: item.get("encrypted_content").and_then(|x| x.as_str()).map(String::from),
                            item_id: item.get("id").and_then(|x| x.as_str()).map(String::from),
                        });
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                let r = v.get("response").cloned().unwrap_or(Value::Null);
                if let Some(u) = r.get("usage") {
                    let input = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    let cached = u.pointer("/input_tokens_details/cached_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    usage.input = input.saturating_sub(cached);
                    usage.cache_read = cached;
                    usage.output = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    usage.reasoning = u.pointer("/output_tokens_details/reasoning_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                }
                finish = if ty == "response.incomplete" {
                    match r.pointer("/incomplete_details/reason").and_then(|x| x.as_str()) {
                        Some("content_filter") => "refusal".into(),
                        _ => "length".into(),
                    }
                } else if had_calls {
                    "tool-calls".into()
                } else {
                    "stop".into()
                };
                break;
            }
            "response.failed" => {
                let msg = v.pointer("/response/error/message").and_then(|x| x.as_str()).unwrap_or("response failed");
                return Err(anyhow!("{msg}"));
            }
            "error" => {
                let msg = v.get("message").or_else(|| v.pointer("/error/message")).and_then(|x| x.as_str()).unwrap_or("unknown error");
                return Err(anyhow!("{msg}"));
            }
            _ => {}
        }
    }
    let _ = tx.send(Event::Usage(usage));
    let _ = tx.send(Event::Finish(finish));
    Ok(())
}
