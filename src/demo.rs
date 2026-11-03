//! Scripted offline demo, ported from opencode-mobile: a bug-fix walkthrough covering reasoning,
//! search, a diff and a permission prompt. Purely presentational; it never touches the engine or
//! the disk.

use opencode_core::session::{Message, Part, Role, ToolState, Usage};
use serde_json::json;

pub const PERMISSION_ID: &str = "demo-permission";

fn msg(id: &str, role: Role, parts: Vec<Part>) -> Message {
    let mut m = Message::new(role);
    m.id = id.into();
    m.parts = parts;
    if role == Role::Assistant {
        m.model = Some("anthropic/claude-opus-4-6".into());
        m.completed = Some(m.time);
    }
    m
}

fn text(t: &str) -> Part {
    Part::Text { text: t.into(), synthetic: false }
}

/// The scripted conversation up to and including `step`.
pub fn messages(step: usize) -> Vec<Message> {
    let user = msg("demo-msg-user", Role::User, vec![text("The login button doesn't respond on Android when the keyboard is open — can you fix it?")]);
    let mut parts = vec![Part::Reasoning {
        text: "The tap handler on the login button isn't firing while the Android keyboard is open — likely the surrounding scroll view is swallowing the first tap to dismiss the keyboard instead of forwarding it to the button. Let me check the handler and confirm.".into(),
        signature: None,
        redacted: None,
        item_id: None,
    }];
    if step >= 1 {
        parts.push(text("Found it — `keyboardShouldPersistTaps` isn't set on the form's scroll view, so Android eats the first tap. Searching for the file..."));
        parts.push(Part::Tool {
            id: "demo-call-search".into(),
            name: "grep".into(),
            input: json!({ "pattern": "onPress", "path": "src/components/LoginButton.tsx" }),
            state: ToolState::Completed { output: "src/components/LoginButton.tsx:12: onPress={handleLogin}".into(), title: "onPress".into(), metadata: json!({ "matches": 1 }), attachments: vec![] },
            provider_meta: None,
        });
    }
    if step >= 2 {
        parts.push(text("Adding `keyboardShouldPersistTaps=\"handled\"` so taps reach the button even while the keyboard is up."));
        parts.push(Part::Tool {
            id: "demo-call-edit".into(),
            name: "edit".into(),
            input: json!({
                "path": "src/components/LoginButton.tsx",
                "oldString": "<ScrollView contentContainerStyle={styles.form}>",
                "newString": "<ScrollView contentContainerStyle={styles.form} keyboardShouldPersistTaps=\"handled\">",
            }),
            state: ToolState::Completed { output: "Edit applied successfully.".into(), title: "src/components/LoginButton.tsx".into(), metadata: json!({}), attachments: vec![] },
            provider_meta: None,
        });
    }
    let mut assistant = msg("demo-msg-assistant", Role::Assistant, parts);
    if step >= 2 {
        assistant.usage = Usage { input: 812, output: 246, ..Default::default() };
        assistant.cost = 0.0143;
    } else {
        assistant.completed = None;
    }
    vec![user, assistant]
}

pub fn outcome(allowed: bool) -> Message {
    let t = if allowed {
        "Tests passed — the login button now responds correctly with the keyboard open. Start a real session to try it on your own code."
    } else {
        "No changes were made — permission was denied. You're always in control of what the agent runs."
    };
    msg(if allowed { "demo-msg-completion" } else { "demo-msg-denial" }, Role::Assistant, vec![text(t)])
}
