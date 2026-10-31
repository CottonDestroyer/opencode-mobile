//! Permission rules: `(action, resource pattern, effect)` triples, evaluated last match wins.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
    Ask,
}

impl Effect {
    pub fn parse(s: &str) -> Option<Effect> {
        match s {
            "allow" => Some(Effect::Allow),
            "deny" => Some(Effect::Deny),
            "ask" => Some(Effect::Ask),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub action: String,
    pub resource: String,
    pub effect: Effect,
}

pub fn rule(action: &str, resource: &str, effect: Effect) -> Rule {
    Rule { action: action.into(), resource: resource.into(), effect }
}

pub fn defaults() -> Vec<Rule> {
    vec![
        rule("*", "*", Effect::Allow),
        rule("external_directory", "*", Effect::Ask),
        rule("read", "*.env", Effect::Ask),
        rule("read", "*.env.*", Effect::Ask),
        rule("read", "*.env.example", Effect::Allow),
    ]
}

/// Legacy (v1) tool names that may appear in permission configs.
fn canonical_action(a: &str) -> &str {
    match a {
        "bash" => "shell",
        "task" => "subagent",
        "list" => "read",
        "multiedit" | "write" | "apply_patch" | "patch" => "edit",
        other => other,
    }
}

/// Parses a `permission` config value: `"allow"`, or `{ action: effect | { pattern: effect } }`.
pub fn from_config(v: &Value) -> Vec<Rule> {
    let mut out = Vec::new();
    match v {
        Value::String(s) => {
            if let Some(e) = Effect::parse(s) {
                out.push(rule("*", "*", e));
            }
        }
        Value::Object(m) => {
            for (action, val) in m {
                let action = canonical_action(action);
                match val {
                    Value::String(s) => {
                        if let Some(e) = Effect::parse(s) {
                            out.push(rule(action, "*", e));
                        }
                    }
                    Value::Object(patterns) => {
                        for (pat, eff) in patterns {
                            if let Some(e) = eff.as_str().and_then(Effect::parse) {
                                out.push(rule(action, pat, e));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

pub fn evaluate(rules: &[Rule], action: &str, resource: &str) -> Effect {
    rules
        .iter()
        .rev()
        .find(|r| crate::util::wildcard(&r.action, action) && crate::util::wildcard(&r.resource, resource))
        .map(|r| r.effect)
        .unwrap_or(Effect::Allow)
}

/// Whether these rules disable the tool entirely, i.e. deny it for every resource.
pub fn disabled(rules: &[Rule], action: &str) -> bool {
    rules
        .iter()
        .rev()
        .find(|r| crate::util::wildcard(&r.action, action) && r.resource == "*")
        .map(|r| r.effect == Effect::Deny)
        .unwrap_or(false)
        && !rules.iter().any(|r| r.action == action && r.resource != "*" && r.effect != Effect::Deny)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Once,
    Always,
    Reject,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub id: String,
    pub session_id: String,
    /// The tool call that asked, when known.
    pub tool_call_id: Option<String>,
    pub action: String,
    pub resources: Vec<String>,
    pub title: String,
    pub metadata: Value,
    /// Patterns to remember when the user answers "always".
    pub save: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn last_match_wins() {
        let mut r = defaults();
        r.extend(from_config(&serde_json::json!({ "bash": { "*": "ask", "git *": "allow" }, "edit": "deny" })));
        assert_eq!(evaluate(&r, "shell", "git status"), Effect::Allow);
        assert_eq!(evaluate(&r, "shell", "rm -rf /"), Effect::Ask);
        assert_eq!(evaluate(&r, "edit", "a.txt"), Effect::Deny);
        assert_eq!(evaluate(&r, "read", "/x/.env"), Effect::Ask);
        assert_eq!(evaluate(&r, "read", "/x/.env.example"), Effect::Allow);
        assert!(disabled(&r, "edit"));
        assert!(!disabled(&r, "shell"));
    }
}
