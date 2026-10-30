//! System prompt assembly: the model-specific base prompt, environment, date, instruction files
//! (AGENTS.md and config `instructions`) and available skills.

use crate::agent::Agent;
use crate::app::App;
use crate::models::Model;
use std::path::{Path, PathBuf};

pub const SYSTEM: &str = include_str!("../assets/prompts/system.txt");
pub const ANTHROPIC: &str = include_str!("../assets/prompts/anthropic.txt");
pub const GPT: &str = include_str!("../assets/prompts/gpt.txt");
pub const GPT_ASTRA: &str = include_str!("../assets/prompts/gpt-astra.txt");
pub const KIMI: &str = include_str!("../assets/prompts/kimi.txt");
pub const META: &str = include_str!("../assets/prompts/meta.txt");
pub const TRINITY: &str = include_str!("../assets/prompts/trinity.txt");
pub const INITIALIZE: &str = include_str!("../assets/prompts/initialize.txt");
pub const REVIEW: &str = include_str!("../assets/prompts/review.txt");

pub const MAX_STEPS: &str = "CRITICAL - MAXIMUM STEPS REACHED

The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.

STRICT REQUIREMENTS:
1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)
2. MUST provide a text response summarizing work done so far
3. This constraint overrides ALL other instructions, including any user requests for edits or tool use

Response must include:
- Statement that maximum steps for this agent have been reached
- Summary of what has been accomplished so far
- List of any remaining tasks that were not completed
- Recommendations for what should be done next

Any attempt to use tools is a critical violation. Respond with text ONLY.";

pub const LENGTH: &str = "Your last response hit the output token limit (stop reason: length). Do not apologize, recap, or repeat yourself. Break the remaining work into smaller pieces.";

/// Replaces `${OPENCODE_TOOL_GUIDANCE}` with guidance for the tools that are present.
pub fn tool_guidance(base: &str, tools: &[String]) -> String {
    let has = |n: &str| tools.iter().any(|t| t == n);
    let mut lines: Vec<&str> = Vec::new();
    if has("shell") {
        lines.push("- Prefer dedicated tools over shell commands; fall back to the shell when a tool cannot do what you need.");
        lines.push("- Do not chain shell commands with separators like `echo \"====\";` or `printf '---'`; the output becomes noisy in a way that makes the user's side of the conversation worse.");
    }
    if has("write") {
        lines.push("- Use the write tool to create files or completely replace their content. Prefer using the edit tool for targeted changes.");
    }
    if has("edit") {
        lines.push("- Use the edit tool for targeted changes to existing text files. It replaces the exact text in `oldString` with `newString`, and the values must differ. By default, `oldString` must occur exactly once. If it occurs multiple times, include more surrounding context to make it unique or set `replaceAll` to true to replace every occurrence.");
    }
    base.replace("${OPENCODE_TOOL_GUIDANCE}", &lines.join("\n"))
}

pub fn base_prompt(agent: &Agent, model: &Model, tools: &[String]) -> String {
    if let Some(s) = &agent.system {
        return s.clone();
    }
    let id = model.id.to_lowercase();
    let base = if id.contains("gpt") {
        if id.contains("astra") {
            GPT_ASTRA.to_string()
        } else {
            GPT.to_string()
        }
    } else if id.contains("kimi") {
        KIMI.to_string()
    } else if id.contains("trinity") {
        TRINITY.to_string()
    } else if id.contains("muse") {
        META.replace("{{MODEL_NAME}}", &model.name)
    } else if id.contains("claude") {
        format!("{SYSTEM}\n\n{ANTHROPIC}")
    } else {
        SYSTEM.to_string()
    };
    tool_guidance(&base, tools)
}

fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

pub fn environment(app: &App) -> String {
    let tmp = crate::paths::get().tmp.display().to_string();
    [
        "Here is some useful information about the environment you are running in:".to_string(),
        "<env>".into(),
        format!("  Working directory: {}", app.directory.display()),
        format!("  Workspace root folder: {}", app.worktree.display()),
        format!("  Is directory a git repo: {}", if app.is_git { "yes" } else { "no" }),
        format!("  Platform: {}", platform()),
        format!("  Prefer {tmp} over generic system temporary directories such as /tmp; it is pre-created and approved for external access."),
        "</env>".into(),
    ]
    .join("\n")
}

pub fn date() -> String {
    format!("Today's date: {}", chrono::Local::now().format("%a %b %d %Y"))
}

/// AGENTS.md files from the working directory up to the worktree root, plus the global one.
pub fn instruction_files(app: &App) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let global = crate::paths::get().config.join("AGENTS.md");
    if global.is_file() {
        out.push(global);
    }
    let mut chain = Vec::new();
    let mut cur: Option<&Path> = Some(&app.directory);
    while let Some(d) = cur {
        let f = d.join("AGENTS.md");
        if f.is_file() {
            chain.push(f);
        }
        if crate::util::normalize(d) == crate::util::normalize(&app.worktree) {
            break;
        }
        cur = d.parent();
    }
    chain.reverse();
    out.extend(chain);
    for pattern in &app.config.instructions {
        if pattern.starts_with("http://") || pattern.starts_with("https://") {
            continue;
        }
        let expanded = crate::util::expand_home(pattern);
        let base = if Path::new(&expanded).is_absolute() { PathBuf::new() } else { app.worktree.clone() };
        let full = base.join(&expanded);
        if !expanded.contains('*') {
            if full.is_file() && !out.contains(&full) {
                out.push(full);
            }
            continue;
        }
        if let Ok(glob) = globset::Glob::new(&full.to_string_lossy()) {
            let m = glob.compile_matcher();
            let root = Path::new(&expanded).components().take_while(|c| !c.as_os_str().to_string_lossy().contains('*')).collect::<PathBuf>();
            let walk_root = if base.as_os_str().is_empty() { root } else { base.join(root) };
            for e in ignore::WalkBuilder::new(&walk_root).build().flatten() {
                let p = e.path();
                if p.is_file() && m.is_match(p) && !out.contains(&p.to_path_buf()) {
                    out.push(p.to_path_buf());
                }
            }
        }
    }
    out
}

pub async fn instructions(app: &App) -> Option<String> {
    let mut blocks = Vec::new();
    for f in instruction_files(app) {
        if let Ok(c) = std::fs::read_to_string(&f) {
            blocks.push(format!("Instructions from: {}\n{}", f.display(), c));
        }
    }
    for url in app.config.instructions.iter().filter(|p| p.starts_with("http")) {
        let res = crate::http::send(crate::http::client().get(url).timeout(std::time::Duration::from_secs(5))).await;
        if let Ok(r) = res {
            if let Ok(t) = r.text().await {
                blocks.push(format!("Instructions from: {url}\n{t}"));
            }
        }
    }
    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join("\n\n"))
    }
}

pub fn skills_block(skills: &[crate::tools::skill::Skill]) -> String {
    let mut lines = vec![
        "Skills provide specialized instructions and workflows for specific tasks.".to_string(),
        "Use the skill tool to load a skill when a task matches its description.".into(),
        "The user may also invoke a skill directly. When that happens, its instructions appear in the conversation as a <skill_content> block, the same shape the skill tool returns. A skill that is already present this way does not need to be invoked again.".into(),
    ];
    if skills.is_empty() {
        lines.push("No skills are currently available.".into());
    } else {
        lines.push("<available_skills>".into());
        for s in skills {
            lines.push("  <skill>".into());
            lines.push(format!("    <id>{}</id>", s.id));
            lines.push(format!("    <name>{}</name>", s.name));
            lines.push(format!("    <description>{}</description>", s.description));
            lines.push("  </skill>".into());
        }
        lines.push("</available_skills>".into());
    }
    lines.join("\n")
}

/// The full system prompt as separate blocks, stable content first so it caches well.
pub async fn system(app: &App, agent: &Agent, model: &Model, tools: &[String]) -> Vec<String> {
    let mut out = vec![base_prompt(agent, model, tools)];
    let mut ctx = vec![environment(app)];
    if let Some(i) = instructions(app).await {
        ctx.push(i);
    }
    if tools.iter().any(|t| t == "skill") {
        ctx.push(skills_block(&app.skills));
    }
    ctx.push(date());
    out.push(ctx.join("\n\n"));
    out
}
