//! `kitchenctl` — the public CLI of the kitchen. See `kitchenctl guide`.

mod cli;
mod client;
mod hook;
mod relay;
mod render;

use std::io::BufReader;
use std::process::ExitCode;

use clap::Parser;
use serde_json::json;

use cli::{Call, Cli};
use client::CallError;

pub const GUIDE: &str = include_str!("guide.md");

const EXIT_API: u8 = 1;
const EXIT_UNREACHABLE: u8 = 3;

fn main() -> ExitCode {
    let cli = Cli::parse_from(cli::normalize_args(std::env::args().collect()));
    let call = match cli.to_call() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kitchenctl: {e:#}");
            return ExitCode::from(2);
        }
    };
    match call {
        Call::PluginLogs { name, lines } => {
            let path = kitchen_proto::state_dir().join("logs").join("plugins").join(format!("{name}.log"));
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let all: Vec<&str> = text.lines().collect();
                    for l in &all[all.len().saturating_sub(lines)..] {
                        println!("{l}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("kitchenctl: {}: {e}", path.display());
                    ExitCode::from(1)
                }
            }
        }
        Call::Guide => {
            if cli.json {
                println!("{}", json!({"topic": "kitchen", "format": "markdown", "content": GUIDE}));
            } else {
                print!("{GUIDE}");
            }
            ExitCode::SUCCESS
        }
        Call::Version => {
            if cli.json {
                println!("{}", json!({"version": env!("CARGO_PKG_VERSION"), "protocol": kitchen_proto::PROTOCOL_VERSION}));
            } else {
                println!("kitchenctl {} (protocol {})", env!("CARGO_PKG_VERSION"), kitchen_proto::PROTOCOL_VERSION);
            }
            ExitCode::SUCCESS
        }
        Call::Relay => {
            let socket = cli.socket.clone().unwrap_or_else(kitchen_proto::default_socket_path);
            let stdin = BufReader::new(std::io::stdin());
            match relay::run(&socket, stdin, std::io::stdout()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("kitchenctl __relay: {e:#}");
                    ExitCode::from(EXIT_UNREACHABLE)
                }
            }
        }
        Call::Skip => ExitCode::SUCCESS,
        Call::Index(paths) => {
            // Symbol extraction lives in kitchen-symbols; not wired into the CLI yet.
            eprintln!("kitchenctl __index: symbol indexing is not available in this build ({} paths)", paths.len());
            ExitCode::from(EXIT_API)
        }
        Call::Api { method, params } => {
            let socket = cli.socket.clone().unwrap_or_else(kitchen_proto::default_socket_path);
            match client::call(&socket, &method, params) {
                Ok(v) => {
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                    } else {
                        print!("{}", render::render(&method, &v));
                    }
                    ExitCode::SUCCESS
                }
                Err(CallError::Api(msg)) => {
                    if cli.json {
                        println!("{}", json!({"ok": false, "error": msg}));
                    }
                    eprintln!("kitchenctl: {msg}");
                    ExitCode::from(EXIT_API)
                }
                Err(CallError::Connect(e)) => {
                    eprintln!("kitchenctl: {e:#}");
                    ExitCode::from(EXIT_UNREACHABLE)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// The skill file shipped for agents must be the same text as `kitchenctl guide`.
    #[test]
    fn skill_matches_guide() {
        let skill = include_str!("../../../skill/kitchen/SKILL.md");
        let body = skill.splitn(3, "---\n").nth(2).expect("SKILL.md has frontmatter").trim_start();
        assert_eq!(body, super::GUIDE, "regenerate skill/kitchen/SKILL.md from crates/kitchenctl/src/guide.md");
    }
}
