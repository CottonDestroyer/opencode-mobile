//! Provider and model catalog from models.dev (an embedded snapshot, refreshed into the cache
//! directory), merged with user configuration.

use crate::config::{Config, ProviderOptions};
use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::BufReader;
use std::sync::LazyLock;

static SNAPSHOT: &[u8] = include_bytes!("../assets/models.json.gz");
const MODELS_URL: &str = "https://models.dev/api.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Anthropic,
    OpenAIChat,
    OpenAIResponses,
    Google,
}

#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    pub provider_id: String,
    pub name: String,
    pub family: String,
    pub reasoning: bool,
    pub reasoning_options: Vec<Value>,
    pub tool_call: bool,
    pub attachment: bool,
    pub temperature: bool,
    pub context: u64,
    pub output: u64,
    pub cost_input: f64,
    pub cost_output: f64,
    pub cost_cache_read: f64,
    pub cost_cache_write: f64,
    pub input_modalities: Vec<String>,
    pub interleaved_field: Option<String>,
    pub release_date: String,
    pub status: Option<String>,
    pub npm: Option<String>,
    pub api: Option<String>,
    pub shape: Option<String>,
    pub options: Value,
    pub headers: BTreeMap<String, String>,
}

impl Model {
    pub fn full_id(&self) -> String {
        format!("{}/{}", self.provider_id, self.id)
    }
    /// Reasoning presets, selectable as `provider/model#variant`.
    pub fn variants(&self) -> Vec<String> {
        let mut out = Vec::new();
        for opt in &self.reasoning_options {
            match opt.get("type").and_then(|t| t.as_str()) {
                Some("effort") => {
                    for v in opt.get("values").and_then(|v| v.as_array()).into_iter().flatten() {
                        if let Some(s) = v.as_str() {
                            if !out.iter().any(|o| o == s) {
                                out.push(s.to_string());
                            }
                        }
                    }
                }
                Some("budget_tokens") => {
                    for s in ["low", "medium", "high"] {
                        if !out.iter().any(|o| o == s) {
                            out.push(s.to_string());
                        }
                    }
                }
                Some("toggle") => {
                    if !out.iter().any(|o| o == "off") {
                        out.push("off".into());
                    }
                }
                _ => {}
            }
        }
        out
    }
    pub fn has_reasoning_type(&self, ty: &str) -> bool {
        self.reasoning_options.iter().any(|o| o.get("type").and_then(|t| t.as_str()) == Some(ty))
    }
    pub fn supports_image(&self) -> bool {
        self.attachment || self.input_modalities.iter().any(|m| m == "image")
    }
    pub fn supports_pdf(&self) -> bool {
        self.input_modalities.iter().any(|m| m == "pdf")
    }
}

#[derive(Debug, Clone)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub npm: String,
    pub api: Option<String>,
    pub env: Vec<String>,
    pub key: Option<String>,
    pub options: ProviderOptions,
    pub models: BTreeMap<String, Model>,
}

impl Provider {
    pub fn protocol_for(&self, model: &Model) -> Result<Protocol> {
        let npm = model.npm.as_deref().unwrap_or(&self.npm);
        if model.shape.as_deref() == Some("responses") {
            return Ok(Protocol::OpenAIResponses);
        }
        if model.shape.as_deref() == Some("completions") {
            return Ok(Protocol::OpenAIChat);
        }
        Ok(match npm {
            "@ai-sdk/anthropic" => Protocol::Anthropic,
            "@ai-sdk/openai" => Protocol::OpenAIResponses,
            "@ai-sdk/google" => Protocol::Google,
            "@ai-sdk/amazon-bedrock" | "@ai-sdk/google-vertex" | "@ai-sdk/google-vertex/anthropic" | "@ai-sdk/azure"
                if self.base_url(model).is_none() =>
            {
                return Err(anyhow!(
                    "provider {} ({npm}) needs cloud SDK authentication, which is not supported; configure provider.{}.options.baseURL with an OpenAI-compatible endpoint instead",
                    self.id,
                    self.id
                ))
            }
            _ => Protocol::OpenAIChat,
        })
    }

    pub fn base_url(&self, model: &Model) -> Option<String> {
        let url = self
            .options
            .base_url
            .clone()
            .or_else(|| model.api.clone())
            .or_else(|| self.api.clone())
            .or_else(|| default_base_url(model.npm.as_deref().unwrap_or(&self.npm)).map(String::from))?;
        // Expand the `${ENV}` placeholders the catalog uses.
        static RE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\$\{([A-Z0-9_]+)\}").unwrap());
        Some(RE.replace_all(&url, |c: &regex::Captures| std::env::var(&c[1]).unwrap_or_default()).trim_end_matches('/').to_string())
    }
}

fn default_base_url(npm: &str) -> Option<&'static str> {
    Some(match npm {
        "@ai-sdk/anthropic" => "https://api.anthropic.com/v1",
        "@ai-sdk/openai" => "https://api.openai.com/v1",
        "@ai-sdk/google" => "https://generativelanguage.googleapis.com/v1beta",
        "@ai-sdk/groq" => "https://api.groq.com/openai/v1",
        "@ai-sdk/xai" => "https://api.x.ai/v1",
        "@ai-sdk/mistral" => "https://api.mistral.ai/v1",
        "@ai-sdk/cerebras" => "https://api.cerebras.ai/v1",
        "@ai-sdk/togetherai" => "https://api.together.xyz/v1",
        "@ai-sdk/deepinfra" => "https://api.deepinfra.com/v1/openai",
        "@ai-sdk/perplexity" => "https://api.perplexity.ai",
        "@ai-sdk/cohere" => "https://api.cohere.ai/compatibility/v1",
        "@openrouter/ai-sdk-provider" => "https://openrouter.ai/api/v1",
        "@ai-sdk/gateway" => "https://ai-gateway.vercel.sh/v1",
        _ => return None,
    })
}

/// A models.dev catalog entry with its chat models already parsed. The catalog is streamed: each
/// model's JSON is dropped as soon as it's parsed, so the whole catalog (several MB of JSON, many
/// times that as a `Value` tree) is never held at once.
#[derive(Default, Deserialize)]
#[serde(default)]
struct CatalogProvider {
    name: Option<Value>,
    npm: Option<Value>,
    api: Option<Value>,
    env: Option<Value>,
    models: ChatModels,
}

/// A catalog provider's chat models, keyed by catalog id, with `provider_id` left empty.
#[derive(Default)]
struct ChatModels(BTreeMap<String, Model>);

impl<'de> Deserialize<'de> for ChatModels {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ChatModels;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map of models")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<ChatModels, A::Error> {
                let mut out = BTreeMap::new();
                while let Some((id, v)) = map.next_entry::<String, Value>()? {
                    if is_chat_model(&v) {
                        let m = parse_model("", &id, &v);
                        out.insert(id, m);
                    }
                }
                Ok(ChatModels(out))
            }
        }
        d.deserialize_map(Visitor)
    }
}

/// The models.dev catalog: the cached copy if it parses, otherwise the embedded snapshot.
fn catalog() -> BTreeMap<String, CatalogProvider> {
    if let Ok(f) = std::fs::File::open(crate::paths::get().models_cache()) {
        if let Ok(c) = serde_json::from_reader(BufReader::new(f)) {
            return c;
        }
    }
    serde_json::from_reader(BufReader::new(flate2::read::GzDecoder::new(SNAPSHOT))).unwrap_or_default()
}

/// Refreshes the cached catalog from models.dev.
pub async fn refresh() -> Result<()> {
    if std::env::var("OPENCODE_DISABLE_MODELS_FETCH").is_ok() {
        return Ok(());
    }
    let url = std::env::var("OPENCODE_MODELS_URL").unwrap_or_else(|_| MODELS_URL.into());
    let req = crate::http::client().get(url).header("User-Agent", crate::user_agent()).timeout(std::time::Duration::from_secs(10));
    let text = crate::http::send(req).await?.error_for_status()?.text().await?;
    // Only check that it's a JSON object; catalog() does the real parsing when it's next read.
    if serde_json::from_str::<BTreeMap<String, serde::de::IgnoredAny>>(&text).is_err() {
        return Err(anyhow!("unexpected models.dev payload"));
    }
    let path = crate::paths::get().models_cache();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

fn parse_model(provider_id: &str, id: &str, v: &Value) -> Model {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
    let b = |k: &str, d: bool| v.get(k).and_then(|x| x.as_bool()).unwrap_or(d);
    let limit = v.get("limit").cloned().unwrap_or(Value::Null);
    let cost = v.get("cost").cloned().unwrap_or(Value::Null);
    let num = |o: &Value, k: &str| o.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    let provider = v.get("provider").cloned().unwrap_or(Value::Null);
    Model {
        id: s("id").unwrap_or_else(|| id.to_string()),
        provider_id: provider_id.to_string(),
        name: s("name").unwrap_or_else(|| id.to_string()),
        family: s("family").unwrap_or_default(),
        reasoning: b("reasoning", false),
        reasoning_options: v.get("reasoning_options").and_then(|x| x.as_array()).cloned().unwrap_or_default(),
        tool_call: b("tool_call", true),
        attachment: b("attachment", false),
        temperature: b("temperature", true),
        context: num(&limit, "context") as u64,
        output: num(&limit, "output") as u64,
        cost_input: num(&cost, "input"),
        cost_output: num(&cost, "output"),
        cost_cache_read: num(&cost, "cache_read"),
        cost_cache_write: num(&cost, "cache_write"),
        input_modalities: v
            .pointer("/modalities/input")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect())
            .unwrap_or_else(|| vec!["text".into()]),
        interleaved_field: v.pointer("/interleaved/field").and_then(|x| x.as_str()).map(String::from),
        release_date: s("release_date").unwrap_or_default(),
        status: s("status"),
        npm: provider.get("npm").and_then(|x| x.as_str()).map(String::from),
        api: provider.get("api").and_then(|x| x.as_str()).map(String::from),
        shape: provider.get("shape").and_then(|x| x.as_str()).map(String::from),
        options: v.get("options").cloned().unwrap_or(Value::Null),
        headers: v
            .get("headers")
            .and_then(|h| serde_json::from_value(h.clone()).ok())
            .unwrap_or_default(),
    }
}

fn is_chat_model(v: &Value) -> bool {
    if v.get("type").and_then(|t| t.as_str()).is_some_and(|t| t != "chat" && t != "language") {
        return false;
    }
    let out = v.pointer("/modalities/output").and_then(|x| x.as_array());
    match out {
        Some(arr) => arr.iter().any(|m| m.as_str() == Some("text")),
        None => true,
    }
}

/// Builds the provider registry from the catalog, stored credentials and config.
pub fn providers(config: &Config) -> BTreeMap<String, Provider> {
    let mut catalog = catalog();
    let creds = crate::auth::all();
    let mut out = BTreeMap::new();

    let mut ids: Vec<String> = catalog.keys().cloned().collect();
    for id in config.provider.keys() {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }

    for id in ids {
        if config.disabled_providers.contains(&id) {
            continue;
        }
        if let Some(enabled) = &config.enabled_providers {
            if !enabled.contains(&id) {
                continue;
            }
        }
        let cat = catalog.remove(&id);
        let cfg = config.provider.get(&id);
        if cat.is_none() && cfg.is_none() {
            continue;
        }
        let cat = cat.unwrap_or_default();
        let cs = |v: &Option<Value>| v.as_ref().and_then(|x| x.as_str()).map(String::from);
        let mut models = cat.models.0;
        for m in models.values_mut() {
            m.provider_id = id.clone();
        }
        let mut env: Vec<String> = cat
            .env
            .as_ref()
            .and_then(|e| e.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let mut provider = Provider {
            id: id.clone(),
            name: cs(&cat.name).unwrap_or_else(|| id.clone()),
            npm: cs(&cat.npm).unwrap_or_else(|| "@ai-sdk/openai-compatible".into()),
            api: cs(&cat.api),
            env: vec![],
            key: None,
            options: ProviderOptions::default(),
            models,
        };
        if let Some(cfg) = cfg {
            if let Some(n) = &cfg.name {
                provider.name = n.clone();
            }
            if let Some(n) = &cfg.npm {
                provider.npm = n.clone();
            }
            if let Some(a) = &cfg.api {
                provider.api = Some(a.clone());
            }
            env.extend(cfg.env.iter().cloned());
            provider.options = cfg.options.clone();
            for (mid, mv) in &cfg.models {
                let base = provider
                    .models
                    .get(mid)
                    .map(|m| model_to_value(m))
                    .unwrap_or_else(|| serde_json::json!({ "id": mid, "name": mid, "tool_call": true, "attachment": false }));
                let mut merged = base;
                crate::config::merge(&mut merged, mv.clone());
                let mut m = parse_model(&id, mid, &merged);
                if let Some(api_id) = mv.get("id").and_then(|x| x.as_str()) {
                    m.id = api_id.to_string();
                }
                provider.models.insert(mid.clone(), m);
            }
            if let Some(wl) = &cfg.whitelist {
                provider.models.retain(|k, _| wl.contains(k));
            }
            if let Some(bl) = &cfg.blacklist {
                provider.models.retain(|k, _| !bl.contains(k));
            }
        }
        provider.env = env;
        provider.key = provider
            .options
            .api_key
            .clone()
            .or_else(|| creds.get(&id).map(|c| c.secret().to_string()))
            .or_else(|| provider.env.iter().find_map(|e| std::env::var(e).ok().filter(|v| !v.is_empty())));
        provider.models.retain(|_, m| m.status.as_deref() != Some("deprecated"));
        // OpenCode Zen without an account: anonymous access to the free models only.
        if id == "opencode" && provider.key.is_none() {
            provider.key = Some("public".into());
            provider.models.retain(|_, m| m.cost_input == 0.0);
        }
        if provider.models.is_empty() {
            continue;
        }
        // Only usable providers (see `available`) keep their models; the rest stay listed by name for
        // connecting one, and adding a key rebuilds the registry. Most of the catalog's thousands of
        // models belong to providers without a key.
        if provider.key.is_none() && cfg.is_none() {
            provider.models.clear();
        }
        out.insert(id, provider);
    }
    out
}

fn model_to_value(m: &Model) -> Value {
    serde_json::json!({
        "id": m.id, "name": m.name, "family": m.family, "reasoning": m.reasoning,
        "reasoning_options": m.reasoning_options, "tool_call": m.tool_call, "attachment": m.attachment,
        "temperature": m.temperature, "release_date": m.release_date,
        "limit": { "context": m.context, "output": m.output },
        "cost": { "input": m.cost_input, "output": m.cost_output, "cache_read": m.cost_cache_read, "cache_write": m.cost_cache_write },
        "modalities": { "input": m.input_modalities },
        "provider": { "npm": m.npm, "api": m.api, "shape": m.shape },
        "options": m.options, "headers": m.headers,
    })
}

/// Providers usable right now: those with credentials, or explicitly configured.
pub fn available(config: &Config, all: &BTreeMap<String, Provider>) -> Vec<String> {
    all.values()
        .filter(|p| p.key.is_some() || config.provider.contains_key(&p.id) || p.id == "opencode")
        .map(|p| p.id.clone())
        .collect()
}

/// Splits `provider/model#variant` into its parts.
pub fn parse_ref(s: &str) -> Option<(String, String, Option<String>)> {
    let (main, variant) = match s.split_once('#') {
        Some((m, v)) => (m, Some(v.to_string())),
        None => (s, None),
    };
    let (p, m) = main.split_once('/')?;
    Some((p.to_string(), m.to_string(), variant))
}

pub struct Resolved {
    pub provider: Provider,
    pub model: Model,
    pub variant: Option<String>,
}

pub fn resolve(all: &BTreeMap<String, Provider>, reference: &str) -> Result<Resolved> {
    let (pid, mid, variant) = parse_ref(reference).ok_or_else(|| anyhow!("model must be in the form provider/model, got {reference}"))?;
    let provider = all.get(&pid).ok_or_else(|| anyhow!("provider not found: {pid}"))?;
    if provider.models.is_empty() {
        return Err(anyhow!("provider {pid} is not connected: add an API key for it"));
    }
    let model = provider
        .models
        .get(&mid)
        .cloned()
        .or_else(|| provider.models.values().find(|m| m.id == mid).cloned())
        .ok_or_else(|| {
            let suggestions: Vec<_> = provider.models.keys().filter(|k| k.contains(&mid) || mid.contains(k.as_str())).take(5).cloned().collect();
            if suggestions.is_empty() {
                anyhow!("model not found: {pid}/{mid}")
            } else {
                anyhow!("model not found: {pid}/{mid}. Did you mean: {}", suggestions.join(", "))
            }
        })?;
    // Callers only need the provider's settings, so leave out its (possibly hundreds of) models.
    let provider = Provider {
        id: provider.id.clone(),
        name: provider.name.clone(),
        npm: provider.npm.clone(),
        api: provider.api.clone(),
        env: provider.env.clone(),
        key: provider.key.clone(),
        options: provider.options.clone(),
        models: BTreeMap::new(),
    };
    Ok(Resolved { provider, model, variant })
}

const FREE_ORDER: [&str; 4] = ["big-pickle", "deepseek-v4.1-flash-free", "glm-5.3-flash-free", "muse-spark-1.3-contributor-free"];

const PRIORITY: &[&str] = &["claude-opus-5-5", "claude-sonnet-5-5", "gpt-5", "gemini-3", "gemini-2.5-pro", "claude", "kimi", "glm", "qwen"];

/// Picks a default model among the available providers.
pub fn default_model(config: &Config, all: &BTreeMap<String, Provider>) -> Option<String> {
    if let Some(m) = &config.model {
        return Some(m.clone());
    }
    if let Some(recent) = crate::state::get().recent_models.first() {
        if resolve(all, recent).is_ok() {
            return Some(recent.clone());
        }
    }
    let avail = available(config, all);
    // With only OpenCode Zen's free models available, upstream prefers these, in this order.
    if avail.iter().all(|id| id == "opencode") {
        if let Some(p) = all.get("opencode").filter(|p| p.models.values().all(|m| m.cost_input == 0.0)) {
            for id in FREE_ORDER {
                if let Some(m) = p.models.get(id).filter(|m| m.tool_call) {
                    return Some(m.full_id());
                }
            }
        }
    }
    let order = ["anthropic", "openai", "google", "openrouter"];
    let mut ids: Vec<&String> = avail.iter().filter(|id| all[*id].key.is_some()).collect();
    ids.sort_by_key(|id| order.iter().position(|o| o == id).unwrap_or(order.len()));
    let pick = |p: &Provider| -> Option<String> {
        let mut models: Vec<&Model> = p.models.values().filter(|m| m.tool_call).collect();
        models.sort_by(|a, b| b.release_date.cmp(&a.release_date));
        for pref in PRIORITY {
            if let Some(m) = models.iter().find(|m| m.id.contains(pref)) {
                return Some(m.full_id());
            }
        }
        models.first().map(|m| m.full_id())
    };
    for id in ids {
        if let Some(m) = pick(&all[id]) {
            return Some(m);
        }
    }
    // Keyless fallback: the free models on the opencode provider.
    all.get("opencode").and_then(|p| {
        let mut free: Vec<&Model> = p.models.values().filter(|m| m.cost_input == 0.0 && m.tool_call).collect();
        free.sort_by(|a, b| b.release_date.cmp(&a.release_date));
        free.first().map(|m| m.full_id())
    })
}

/// A small, cheap model for titles and summaries.
pub fn small_model(config: &Config, all: &BTreeMap<String, Provider>, provider_id: &str) -> Option<String> {
    if let Some(m) = &config.small_model {
        return Some(m.clone());
    }
    // As upstream: the newest model of the first matching family, else the session's own model.
    let p = all.get(provider_id)?;
    for family in ["gpt-luna", "gemini-flash-lite", "gemini-flash", "claude-haiku"] {
        let mut ms: Vec<&Model> = p.models.values().filter(|m| m.family == family).collect();
        ms.sort_by(|a, b| b.release_date.cmp(&a.release_date));
        if let Some(m) = ms.first() {
            return Some(m.full_id());
        }
    }
    None
}
