//! `shell`: runs a command in the user's shell, in the foreground or the background.

use super::{arg_bool, arg_str, arg_u64, req_str, Context, Output, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;

const DEFAULT_TIMEOUT: u64 = 120_000;

const BACKGROUND_NOTE: &str = "You will be notified automatically when the command finishes. The notification will include the command's output. Unless the user explicitly asks otherwise, DO NOT poll for completion, even if you need the final result to continue. Repeatedly sleeping and reading or searching the output file is polling, not useful work. You may read the current output if it lets you do useful work now, but do not wait on it.";

/// How many leading tokens identify a command in "always allow" patterns.
const ARITY: &[(&str, usize)] = &[
    ("cat", 1), ("cd", 1), ("chmod", 1), ("chown", 1), ("cp", 1), ("echo", 1), ("env", 1), ("export", 1), ("grep", 1),
    ("kill", 1), ("killall", 1), ("ln", 1), ("ls", 1), ("mkdir", 1), ("mv", 1), ("ps", 1), ("pwd", 1), ("rm", 1),
    ("rmdir", 1), ("sleep", 1), ("source", 1), ("tail", 1), ("touch", 1), ("unset", 1), ("which", 1),
    ("aws", 3), ("az", 3), ("bazel", 2), ("brew", 2), ("bun", 2), ("bun run", 3), ("bun x", 3), ("cargo", 2),
    ("cargo add", 3), ("cargo run", 3), ("cdk", 2), ("cf", 2), ("cmake", 2), ("composer", 2), ("consul", 2),
    ("consul kv", 3), ("crictl", 2), ("deno", 2), ("deno task", 3), ("doctl", 3), ("docker", 2),
    ("docker builder", 3), ("docker compose", 3), ("docker container", 3), ("docker image", 3),
    ("docker network", 3), ("docker volume", 3), ("eksctl", 2), ("eksctl create", 3), ("firebase", 2),
    ("flyctl", 2), ("gcloud", 3), ("gh", 3), ("git", 2), ("git config", 3), ("git remote", 3), ("git stash", 3),
    ("go", 2), ("gradle", 2), ("helm", 2), ("heroku", 2), ("hugo", 2), ("ip", 2), ("ip addr", 3), ("ip link", 3),
    ("ip netns", 3), ("ip route", 3), ("kind", 2), ("kind create", 3), ("kubectl", 2), ("kubectl kustomize", 3),
    ("kubectl rollout", 3), ("kustomize", 2), ("make", 2), ("mc", 2), ("mc admin", 3), ("minikube", 2),
    ("mongosh", 2), ("mysql", 2), ("mvn", 2), ("ng", 2), ("npm", 2), ("npm exec", 3), ("npm init", 3),
    ("npm run", 3), ("npm view", 3), ("npx", 2), ("nvm", 2), ("nx", 2), ("openssl", 2), ("openssl req", 3),
    ("openssl x509", 3), ("pip", 2), ("pipenv", 2), ("pnpm", 2), ("pnpm dlx", 3), ("pnpm exec", 3),
    ("pnpm run", 3), ("poetry", 2), ("podman", 2), ("podman container", 3), ("podman image", 3), ("psql", 2),
    ("pulumi", 2), ("pulumi stack", 3), ("python", 2), ("pyenv", 2), ("rake", 2), ("rbenv", 2), ("redis-cli", 2),
    ("rustup", 2), ("serverless", 2), ("sfdx", 3), ("skaffold", 2), ("sls", 2), ("sst", 2), ("swift", 2),
    ("systemctl", 2), ("terraform", 2), ("terraform workspace", 3), ("tmux", 2), ("turbo", 2), ("ufw", 2),
    ("vault", 2), ("vault auth", 3), ("vault kv", 3), ("vercel", 2), ("volta", 2), ("wp", 2), ("yarn", 2),
    ("yarn dlx", 3), ("yarn run", 3),
];

fn arity_prefix(tokens: &[String]) -> Vec<String> {
    for len in (1..=3.min(tokens.len())).rev() {
        let key = tokens[..len].join(" ");
        if let Some((_, n)) = ARITY.iter().find(|(k, _)| *k == key) {
            return tokens[..(*n).min(tokens.len())].to_vec();
        }
    }
    tokens[..1.min(tokens.len())].to_vec()
}

fn tokenize(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in cmd.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Splits a command line into simple commands at `&&`, `||`, `;`, `|` and newlines outside quotes.
pub fn split_commands(cmd: &str) -> Vec<String> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            cur.push(c);
            i += 1;
            continue;
        }
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        if two == "&&" || two == "||" {
            out.push(std::mem::take(&mut cur));
            i += 2;
            continue;
        }
        let redirect = c == '&' && (two == "&>" || (i > 0 && chars[i - 1] == '>'));
        if c == ';' || c == '|' || c == '\n' || (c == '&' && !redirect) {
            out.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().trim_start_matches(['(', '{']).trim_end_matches([')', '}']).trim().to_string()).filter(|s| !s.is_empty()).collect()
}

#[derive(Debug, Clone)]
pub struct ShellKind {
    pub program: PathBuf,
    pub args: Vec<String>,
}

pub fn detect() -> ShellKind {
    let sh = std::env::var("SHELL").ok().filter(|s| {
        let n = s.rsplit('/').next().unwrap_or("");
        !matches!(n, "fish" | "nu" | "xonsh" | "elvish") && std::path::Path::new(s).exists()
    });
    let program = PathBuf::from(sh.unwrap_or_else(|| if std::path::Path::new("/bin/bash").exists() { "/bin/bash".into() } else { "/bin/sh".into() }));
    ShellKind { program, args: vec!["-c".into()] }
}

pub struct Shell {
    kind: ShellKind,
}

impl Shell {
    pub fn new() -> Self {
        static KIND: std::sync::OnceLock<ShellKind> = std::sync::OnceLock::new();
        Shell { kind: KIND.get_or_init(detect).clone() }
    }
}

struct RunResult {
    output: String,
    exit: Option<i32>,
    timed_out: bool,
    cancelled: bool,
}

/// Kills the command's whole process group; `run` starts each command in its own group.
fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill").args(["-TERM", &format!("-{pid}")]).status();
        let _ = std::process::Command::new("kill").args(["-KILL", &format!("-{pid}")]).status();
    }
    #[cfg(not(unix))]
    let _ = pid;
}

async fn run(kind: &ShellKind, command: &str, cwd: &PathBuf, timeout_ms: u64, cancel: tokio_util::sync::CancellationToken, sink: Option<PathBuf>) -> Result<RunResult> {
    let mut cmd = tokio::process::Command::new(&kind.program);
    cmd.args(&kind.args).arg(command).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    cmd.env("PAGER", "cat").env("GIT_PAGER", "cat").env("OPENCODE", "1");
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| anyhow!("failed to start {}: {e}", kind.program.display()))?;
    let pid = child.id().unwrap_or(0);
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let file = sink.and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()).map(|f| Arc::new(Mutex::new(f)));
    let mut readers = Vec::new();
    for stream in [child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)].into_iter().flatten() {
        let buf = buf.clone();
        let file = file.clone();
        readers.push(tokio::spawn(async move {
            let mut stream = stream;
            let mut chunk = [0u8; 8192];
            loop {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.lock().unwrap().extend_from_slice(&chunk[..n]);
                        if let Some(f) = &file {
                            use std::io::Write;
                            let _ = f.lock().unwrap().write_all(&chunk[..n]);
                        }
                    }
                }
            }
        }));
    }
    let timeout = async {
        if timeout_ms == 0 {
            futures::future::pending::<()>().await
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await
        }
    };
    let (mut timed_out, mut cancelled) = (false, false);
    let status = tokio::select! {
        s = child.wait() => s.ok(),
        _ = timeout => { timed_out = true; kill_tree(pid); let _ = child.kill().await; None }
        _ = cancel.cancelled() => { cancelled = true; kill_tree(pid); let _ = child.kill().await; None }
    };
    for r in readers {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), r).await;
    }
    let output = String::from_utf8_lossy(&buf.lock().unwrap()).to_string();
    Ok(RunResult { output, exit: status.and_then(|s| s.code()), timed_out, cancelled })
}

/// Truncates shell output from the front, keeping the end, which is where errors usually are.
fn tail(output: &str) -> (String, bool) {
    let text = output.strip_suffix('\n').unwrap_or(output);
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= super::MAX_LINES && text.len() <= super::MAX_BYTES {
        return (if text.is_empty() { "(no output)".into() } else { text.to_string() }, false);
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0;
    for l in lines.iter().rev().take(super::MAX_LINES) {
        if bytes + l.len() + 1 > super::MAX_BYTES {
            break;
        }
        bytes += l.len() + 1;
        kept.push(l);
    }
    kept.reverse();
    let dir = crate::paths::get().tool_output();
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("{}.txt", crate::util::id("shell")));
    let _ = std::fs::write(&file, output);
    let total = lines.len();
    (format!("{}\n\n[showing lines {}-{total} of {total}; full output saved to {}]", kept.join("\n"), total - kept.len() + 1, file.display()), true)
}

fn status_line(r: &RunResult) -> Option<String> {
    if r.timed_out {
        return Some("Timed out before completion".into());
    }
    if r.cancelled {
        return Some("Killed by SIGTERM".into());
    }
    match r.exit {
        Some(0) => None,
        Some(c) => Some(format!("Exited with code {c}")),
        None => Some("Killed by signal".into()),
    }
}

fn format_result(r: &RunResult) -> (String, Value) {
    let (out, truncated) = tail(&r.output);
    let mut text = out;
    if let Some(s) = status_line(r) {
        text.push_str("\n\n");
        text.push_str(&s);
    }
    (text, json!({ "exit": r.exit, "truncated": truncated, "timeout": r.timed_out }))
}

#[async_trait]
impl Tool for Shell {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> String {
        let mut parts = vec!["Execute a shell command and return its output.".to_string()];
        parts.extend(
            [
                "Quote file paths containing spaces or special characters.",
                "Prefer dedicated tools over shell commands when possible.",
                "When output is large, the full result is saved to a file and a truncated preview is returned.",
                "Rely on automatic truncation unless filtering the output is more useful.",
                "Commands accept an optional timeout, background commands have no timeout by default.",
                "Background commands return immediately, and you will be notified when they complete.",
            ]
            .map(String::from),
        );
        parts.join(" ")
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command string to execute" },
                "workdir": { "type": "string", "description": "Working directory to execute the command in. Defaults to the current working directory. When possible, avoid changing directories in the command and set the working directory here instead." },
                "timeout": { "type": "integer", "minimum": 0, "description": format!("Timeout in milliseconds. Set to 0 to disable the timeout. Defaults to {DEFAULT_TIMEOUT} for foreground commands. Background commands have no timeout by default.") },
                "background": { "type": "boolean", "description": "Run the command in the background and return immediately (useful for dev servers and long-running builds). You do not need to use '&' at the end of the command when using this parameter. You will be notified when it completes. DO NOT poll for completion." }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let command = req_str(&input, "command")?.to_string();
        let background = arg_bool(&input, "background").unwrap_or(false);
        let timeout = arg_u64(&input, "timeout").unwrap_or(if background { 0 } else { DEFAULT_TIMEOUT });
        let cwd = ctx.resolve(arg_str(&input, "workdir").unwrap_or("."));
        if !cwd.exists() {
            return Err(anyhow!("Working directory does not exist: {}", cwd.display()));
        }
        if !cwd.is_dir() {
            return Err(anyhow!("Working directory is not a directory: {}", cwd.display()));
        }
        ctx.authorize_external(&cwd).await?;

        // One permission check per simple command; `cd` targets count as directory access.
        let mut resources = Vec::new();
        let mut save = Vec::new();
        for sub in split_commands(&command) {
            let tokens = tokenize(&sub);
            let Some(first) = tokens.first() else { continue };
            let lower = first.to_lowercase();
            if matches!(lower.as_str(), "cd" | "chdir" | "pushd" | "popd" | "set-location" | "push-location") {
                if let Some(target) = tokens.iter().skip(1).find(|t| !t.starts_with('-')) {
                    let p = crate::util::resolve(&cwd, target);
                    ctx.authorize_external(&p).await?;
                }
                continue;
            }
            resources.push(sub.clone());
            save.push(format!("{} *", arity_prefix(&tokens).join(" ")));
        }
        if !resources.is_empty() {
            ctx.ask("shell", &resources, &save, format!("Run: {command}"), json!({ "command": command, "workdir": cwd.display().to_string() })).await?;
        }

        if background {
            let id = crate::util::id("sh");
            let dir = crate::paths::get().tool_output();
            let _ = std::fs::create_dir_all(&dir);
            let file = dir.join(format!("{id}.log"));
            let kind = self.kind.clone();
            let app = ctx.app.clone();
            let session = ctx.session_id.clone();
            let cmd = command.clone();
            let f = file.clone();
            tokio::spawn(async move {
                let cancel = tokio_util::sync::CancellationToken::new();
                let res = run(&kind, &cmd, &cwd, timeout, cancel, Some(f)).await;
                let (state, text) = match res {
                    Ok(r) => ("completed", format_result(&r).0),
                    Err(e) => ("error", e.to_string()),
                };
                let note = format!("<shell id=\"{id}\" state=\"{state}\" command=\"{}\">\n{text}\n</shell>", cmd.replace('"', "&quot;"));
                crate::engine::notify(app, session, note).await;
            });
            return Ok(Output::text(
                format!("Command moved to the background (shell ID: {}).\nOutput is streaming to: {}\n\n{BACKGROUND_NOTE}", file.file_stem().unwrap().to_string_lossy(), file.display()),
                command,
            )
            .meta(json!({ "status": "running" })));
        }

        let r = run(&self.kind, &command, &cwd, timeout, ctx.cancel.clone(), None).await?;
        if r.cancelled {
            return Err(anyhow!("Command cancelled"));
        }
        let (text, meta) = format_result(&r);
        Ok(Output::text(text, command).meta(meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn splits() {
        assert_eq!(split_commands("git add . && git commit -m 'a && b' | cat"), vec!["git add .", "git commit -m 'a && b'", "cat"]);
        assert_eq!(arity_prefix(&tokenize("git commit -m x")), vec!["git", "commit"]);
        assert_eq!(arity_prefix(&tokenize("npm run build")), vec!["npm", "run", "build"]);
        assert_eq!(arity_prefix(&tokenize("ls -la")), vec!["ls"]);
    }
}
