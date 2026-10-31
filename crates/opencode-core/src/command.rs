//! Slash commands: the built-ins (`/init`, `/review`), config `command` entries and `command/*.md` files.

use crate::config::Config;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub template: String,
    pub agent: Option<String>,
    pub model: Option<String>,
}

pub fn load(config: &Config, dirs: &[PathBuf]) -> BTreeMap<String, Command> {
    let mut out = BTreeMap::new();
    out.insert(
        "init".into(),
        Command { name: "init".into(), description: "guided AGENTS.md setup".into(), template: crate::prompt::INITIALIZE.into(), agent: None, model: None },
    );
    out.insert(
        "review".into(),
        Command {
            name: "review".into(),
            description: "review changes [commit|branch|pr], defaults to uncommitted".into(),
            template: crate::prompt::REVIEW.into(),
            agent: None,
            model: None,
        },
    );
    for dir in dirs {
        for sub in ["command", "commands"] {
            let Ok(rd) = std::fs::read_dir(dir.join(sub)) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("md") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&p) else { continue };
                let (fm, body) = crate::config::frontmatter(&text);
                let name = p.file_stem().unwrap().to_string_lossy().to_string();
                let s = |k: &str| fm.get(k).and_then(|v| v.as_str()).map(String::from);
                out.insert(
                    name.clone(),
                    Command {
                        name,
                        description: s("description").unwrap_or_default(),
                        template: body.trim().to_string(),
                        agent: s("agent"),
                        model: s("model"),
                    },
                );
            }
        }
    }
    for (name, c) in &config.command {
        out.insert(
            name.clone(),
            Command {
                name: name.clone(),
                description: c.description.clone().unwrap_or_default(),
                template: c.template.clone(),
                agent: c.agent.clone(),
                model: c.model.clone(),
            },
        );
    }
    out
}

fn shell_output(cmd: &str, cwd: &Path) -> String {
    let shell = crate::tools::shell::detect();
    match std::process::Command::new(&shell.program).args(&shell.args).arg(cmd).current_dir(cwd).output() {
        Ok(o) => format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).trim_end().to_string(),
        Err(e) => format!("(failed to run `{cmd}`: {e})"),
    }
}

/// Expands `$ARGUMENTS`, `$1`..`$9`, `${path}` and `` !`cmd` `` in a command template.
pub fn render(cmd: &Command, args: &str, directory: &Path, worktree: &Path) -> String {
    let mut t = cmd.template.clone();
    let positional: Vec<&str> = args.split_whitespace().collect();
    for i in (1..=9).rev() {
        t = t.replace(&format!("${i}"), positional.get(i - 1).copied().unwrap_or(""));
    }
    let had_args = t.contains("$ARGUMENTS");
    t = t.replace("$ARGUMENTS", args);
    t = t.replace("${path}", &worktree.join("AGENTS.md").display().to_string());
    let re = regex::Regex::new(r"!`([^`]+)`").unwrap();
    t = re.replace_all(&t, |c: &regex::Captures| shell_output(&c[1], directory)).into_owned();
    if !had_args && !args.trim().is_empty() {
        t.push_str("\n\n");
        t.push_str(args);
    }
    t
}
