//! The opencode agent engine, vendored from opencode-rs (the Rust port of the opencode client) with
//! its TUI, ACP server and CLI front ends removed.

pub mod agent;
pub mod app;
pub mod auth;
pub mod command;
pub mod config;
pub mod engine;
pub mod http;
pub mod mcp;
pub mod mention;
pub mod models;
pub mod paths;
pub mod permission;
pub mod prompt;
pub mod provider;
pub mod session;
pub mod state;
pub mod tools;
pub mod util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The upstream release this port tracks; reported to providers the way the TypeScript client does.
pub const UPSTREAM_VERSION: &str = "2.1.4";
pub const UPSTREAM_CHANNEL: &str = "latest";

/// Client name sent as `x-opencode-client`: `OPENCODE_CLIENT` if set, else `cli`, matching upstream.
pub fn client_name() -> String {
    std::env::var("OPENCODE_CLIENT").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "cli".into())
}

/// The TypeScript client's User-Agent: `opencode/<channel>/<version>/<client>`.
pub fn user_agent() -> String {
    format!("opencode/{UPSTREAM_CHANNEL}/{UPSTREAM_VERSION}/{}", client_name())
}
