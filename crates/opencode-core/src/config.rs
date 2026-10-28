//! Loading and merging of `opencode.json` / `opencode.jsonc`.
//!
//! Sources, from lowest to highest precedence: the global config directory, `OPENCODE_CONFIG`,
//! project files from the worktree root down to the working directory (`opencode.json[c]` and
//! `.opencode/opencode.json[c]`), and finally `OPENCODE_CONFIG_CONTENT`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: Option<String>,
    pub small_model: Option<String>,
    pub default_agent: Option<String>,
    pub username: Option<String>,
    pub provider: BTreeMap<String, ProviderConfig>,
    pub disabled_providers: Vec<String>,
    pub enabled_providers: Option<Vec<String>>,
    pub agent: BTreeMap<String, AgentConfig>,
    pub command: BTreeMap<String, CommandConfig>,
    pub instructions: Vec<String>,
    pub permission: Option<Value>,
    pub mcp: BTreeMap<String, McpConfig>,
    pub tools: BTreeMap<String, bool>,
    pub experimental: Experimental,
    pub compaction: Compaction,
    pub skills: Vec<String>,
    pub websearch: Option<WebSearchConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Experimental {
    pub todo: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Compaction {
    pub auto: bool,
}
impl Default for Compaction {
    fn default() -> Self {
        Compaction { auto: true }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSearchConfig {
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub name: Option<String>,
    pub npm: Option<String>,
    pub api: Option<String>,
    pub env: Vec<String>,
    pub options: ProviderOptions,
    pub models: BTreeMap<String, Value>,
    pub whitelist: Option<Vec<String>>,
    pub blacklist: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderOptions {
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    #[serde(rename = "baseURL")]
    pub base_url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub timeout: Option<u64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub model: Option<String>,
    pub variant: Option<String>,
    pub prompt: Option<String>,
    pub description: Option<String>,
    pub mode: Option<String>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub steps: Option<usize>,
    #[serde(alias = "maxSteps")]
    pub max_steps: Option<usize>,
    pub tools: BTreeMap<String, bool>,
    pub permission: Option<Value>,
    pub disable: bool,
    pub hidden: bool,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandConfig {
    pub template: String,
    pub description: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub subtask: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub enabled: Option<bool>,
    pub timeout: Option<u64>,
}

/// Where configuration was found, plus the directories that may hold `agent/`, `command/` and
/// `skill/` folders.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub config: Config,
    pub files: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
}

/// Strips `//` and `/* */` comments and trailing commas from JSONC.
pub fn strip_jsonc(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_str = false;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    // Trailing commas.
    let re = regex::Regex::new(r",(\s*[}\]])").unwrap();
    re.replace_all(&out, "$1").into_owned()
}

/// Substitutes `{env:VAR}` and `{file:path}` tokens.
fn substitute(text: &str, dir: &Path) -> String {
    let env_re = regex::Regex::new(r"\{env:([^}]+)\}").unwrap();
    let text = env_re.replace_all(text, |c: &regex::Captures| std::env::var(&c[1]).unwrap_or_default());
    let file_re = regex::Regex::new(r"\{file:([^}]+)\}").unwrap();
    file_re
        .replace_all(&text, |c: &regex::Captures| {
            let p = crate::util::resolve(dir, c[1].trim());
            let content = std::fs::read_to_string(&p).unwrap_or_default();
            // Embedded as the body of a JSON string.
            let s = serde_json::to_string(content.trim_end()).unwrap_or_default();
            s[1..s.len() - 1].to_string()
        })
        .into_owned()
}

pub fn parse_file(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let text = substitute(&text, dir);
    let v: Value = serde_json::from_str(&strip_jsonc(&text)).with_context(|| format!("parsing {}", path.display()))?;
    Ok(v)
}

/// Deep-merges `b` into `a`. `instructions` arrays are concatenated; other arrays are replaced.
pub fn merge(a: &mut Value, b: Value) {
    match (a, b) {
        (Value::Object(am), Value::Object(bm)) => {
            for (k, v) in bm {
                if k == "instructions" {
                    if let (Some(Value::Array(existing)), Value::Array(new)) = (am.get_mut(&k), &v) {
                        for item in new {
                            if !existing.contains(item) {
                                existing.push(item.clone());
                            }
                        }
                        continue;
                    }
                }
                match am.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        am.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

/// Finds the git worktree root by walking up to the nearest `.git` entry.
pub fn find_worktree(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

pub fn load(directory: &Path, worktree: &Path) -> Result<Loaded> {
    let paths = crate::paths::get();
    let mut raw = Value::Object(Map::new());
    let mut files = Vec::new();
    let mut directories = vec![paths.config.clone()];

    let add = |raw: &mut Value, files: &mut Vec<PathBuf>, p: PathBuf| -> Result<()> {
        if p.is_file() {
            let v = parse_file(&p)?;
            merge(raw, v);
            files.push(p);
        }
        Ok(())
    };

    for name in ["config.json", "opencode.json", "opencode.jsonc"] {
        add(&mut raw, &mut files, paths.config.join(name))?;
    }
    if let Ok(p) = std::env::var("OPENCODE_CONFIG") {
        add(&mut raw, &mut files, PathBuf::from(p))?;
    }

    // Project files, from the worktree root down to the working directory.
    let mut chain = Vec::new();
    let mut cur = Some(directory);
    while let Some(dir) = cur {
        chain.push(dir.to_path_buf());
        if crate::util::normalize(dir) == crate::util::normalize(worktree) {
            break;
        }
        cur = dir.parent();
    }
    chain.reverse();
    for dir in &chain {
        for name in ["opencode.json", "opencode.jsonc"] {
            add(&mut raw, &mut files, dir.join(name))?;
        }
        let dot = dir.join(".opencode");
        if dot.is_dir() {
            directories.push(dot.clone());
            for name in ["opencode.json", "opencode.jsonc"] {
                add(&mut raw, &mut files, dot.join(name))?;
            }
        }
    }
    if let Ok(content) = std::env::var("OPENCODE_CONFIG_CONTENT") {
        let v: Value = serde_json::from_str(&strip_jsonc(&substitute(&content, directory)))
            .context("parsing OPENCODE_CONFIG_CONTENT")?;
        merge(&mut raw, v);
    }
    if let Ok(dir) = std::env::var("OPENCODE_CONFIG_DIR") {
        directories.push(PathBuf::from(dir));
    }

    // Map legacy fields onto their current names.
    if let Value::Object(m) = &mut raw {
        if let Some(mode) = m.remove("mode") {
            let agents = m.entry("agent").or_insert_with(|| Value::Object(Map::new()));
            let mut mode = mode;
            if let Value::Object(mm) = &mut mode {
                for v in mm.values_mut() {
                    if let Value::Object(o) = v {
                        o.entry("mode").or_insert(Value::String("primary".into()));
                    }
                }
            }
            merge(agents, mode);
        }
    }

    let config: Config = serde_json::from_value(raw.clone()).context("invalid configuration")?;
    Ok(Loaded { config, files, directories })
}

/// Parses `---` YAML-style frontmatter (flat `key: value` pairs, plus maps nested one level) from the
/// markdown files that define agents, commands and skills.
pub fn frontmatter(text: &str) -> (Map<String, Value>, String) {
    let text = text.trim_start_matches('\u{feff}');
    let mut map = Map::new();
    let Some(rest) = text.strip_prefix("---") else {
        return (map, text.to_string());
    };
    let rest = rest.trim_start_matches(['\r', '\n']);
    let Some(end) = rest.find("\n---") else {
        return (map, text.to_string());
    };
    let header = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\r', '\n', '-']).to_string();
    let mut current: Option<String> = None;
    for line in header.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let Some((k, v)) = line.trim().split_once(':') else { continue };
        let key = k.trim().trim_matches('"').to_string();
        let val = v.trim();
        if indented {
            if let Some(parent) = &current {
                let entry = map.entry(parent.clone()).or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(o) = entry {
                    o.insert(key, yaml_scalar(val));
                }
            }
            continue;
        }
        if val.is_empty() {
            current = Some(key.clone());
            map.insert(key, Value::Object(Map::new()));
        } else {
            current = None;
            map.insert(key, yaml_scalar(val));
        }
    }
    (map, body)
}

fn yaml_scalar(v: &str) -> Value {
    let v = v.trim();
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2) || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2) {
        return Value::String(v[1..v.len() - 1].to_string());
    }
    match v {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" | "~" => return Value::Null,
        _ => {}
    }
    if let Ok(n) = v.parse::<i64>() {
        return Value::from(n);
    }
    if let Ok(f) = v.parse::<f64>() {
        return Value::from(f);
    }
    if v.starts_with('[') && v.ends_with(']') {
        let items = v[1..v.len() - 1]
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(yaml_scalar)
            .collect();
        return Value::Array(items);
    }
    Value::String(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jsonc() {
        let t = "{ // c\n \"a\": \"http://x\", /* b */ \"b\": [1,2,],\n}";
        let v: Value = serde_json::from_str(&strip_jsonc(t)).unwrap();
        assert_eq!(v["a"], "http://x");
        assert_eq!(v["b"][1], 2);
    }
    #[test]
    fn fm() {
        let (m, body) = frontmatter("---\nname: x\ndescription: \"hello: world\"\ntools:\n  write: false\n---\nBody");
        assert_eq!(m["name"], "x");
        assert_eq!(m["description"], "hello: world");
        assert_eq!(m["tools"]["write"], false);
        assert_eq!(body, "Body");
    }
}
