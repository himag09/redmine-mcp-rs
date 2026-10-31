//! Entry point for the Redmine MCP server.

use std::process::ExitCode;
use std::sync::Arc;

use serde_json::Value;

use redmine_mcp_rs::config::{Config, StartupInfo};
use redmine_mcp_rs::error::Result;
use redmine_mcp_rs::tools::App;
use redmine_mcp_rs::{log, log_debug, server};

const USAGE: &str = r#"A fast Model Context Protocol (MCP) server for Redmine (stdio transport).

USAGE:
    redmine-mcp-rs [OPTIONS]

OPTIONS:
    --check        Validate configuration, verify Redmine connectivity (read-only) and exit.
    -h, --help     Print this help.
    -V, --version  Print version.

ENVIRONMENT:
    REDMINE_URL                  Required. Redmine base URL (http/https).
    REDMINE_API_KEY              Required. Redmine API key.
    REDMINE_DEFAULT_PROJECT_ID   Optional. Default project for create_issue.
    REDMINE_TIMEOUT_MS           Optional. HTTP timeout, 1000-300000 (default 30000).
    REDMINE_READ_ONLY            Optional. true/1/yes/on disables every write tool.
    REDMINE_LOG                  Optional. off, error, warn, info, debug (default error).

A .env file in the working directory fills variables that are not already set
in the environment. MCP client configuration always wins over .env.

EXAMPLES:
    REDMINE_URL=https://redmine.example.com REDMINE_API_KEY=... redmine-mcp-rs --check
    REDMINE_URL=https://redmine.example.com REDMINE_API_KEY=... redmine-mcp-rs
"#;

#[tokio::main]
async fn main() -> ExitCode {
    let command = match Command::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            print_error(message);
            print_stderr("run redmine-mcp-rs --help for usage");
            return ExitCode::from(2);
        }
    };

    match command {
        Command::Help => {
            println!("redmine-mcp-rs {}\n\n{USAGE}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Command::Version => {
            println!("redmine-mcp-rs {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Command::Serve => match serve().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                print_error(error);
                ExitCode::FAILURE
            }
        },
        Command::Check => match check().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                print_error(error);
                ExitCode::FAILURE
            }
        },
    }
}

async fn serve() -> Result<()> {
    let (config, info) = Config::load()?;
    log::init(config.log_level);
    report_startup_info(&info);

    log_debug!(
        "starting redmine-mcp-rs {} (read_only={}, base_url={})",
        env!("CARGO_PKG_VERSION"),
        config.read_only,
        config.base_url()
    );

    let app = App::new(config)?;
    server::run(Arc::new(app)).await
}

async fn check() -> Result<()> {
    let (config, info) = Config::load()?;
    log::init(config.log_level);

    if let Some(path) = &info.dotenv_path {
        print_stderr(format!("using .env: {}", path.display()));
    }
    for warning in &info.warnings {
        print_stderr(format!("warning: {warning}"));
    }

    let app = App::new(config)?;
    let user = app.client.get_current_user().await?;

    let login = user
        .pointer("/user/login")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let id = user
        .pointer("/user/id")
        .and_then(Value::as_u64)
        .map(|id| id.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let read_only = if app.read_only { "yes" } else { "no" };

    println!(
        "OK: connected to {} as {login} (id {id}); read-only: {read_only}",
        app.client.base_url()
    );
    Ok(())
}

fn report_startup_info(info: &StartupInfo) {
    if let Some(path) = &info.dotenv_path {
        log_debug!("loaded .env from {}", path.display());
    }
    // Configuration warnings are rare and actionable; show them regardless of
    // the configured log level (stderr only).
    for warning in &info.warnings {
        print_stderr(format!("warning: {warning}"));
    }
}

fn print_error(message: impl std::fmt::Display) {
    print_stderr(format!("redmine-mcp-rs: {message}"));
}

fn print_stderr(message: impl std::fmt::Display) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{message}");
}

enum Command {
    Serve,
    Check,
    Help,
    Version,
}

impl Command {
    fn parse(args: impl Iterator<Item = String>) -> std::result::Result<Self, String> {
        let mut command = Self::Serve;
        for arg in args {
            match arg.as_str() {
                "-h" | "--help" => return Ok(Self::Help),
                "-V" | "--version" => return Ok(Self::Version),
                "--check" => command = Self::Check,
                other => {
                    return Err(format!(
                        "unknown argument {other:?}; run with --help for usage"
                    ));
                }
            }
        }
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_arguments() {
        let parse = |args: &[&str]| Command::parse(args.iter().map(|arg| (*arg).to_string()));

        assert!(matches!(parse(&[]), Ok(Command::Serve)));
        assert!(matches!(parse(&["--check"]), Ok(Command::Check)));
        assert!(matches!(parse(&["-h"]), Ok(Command::Help)));
        assert!(matches!(parse(&["--version"]), Ok(Command::Version)));
        assert!(parse(&["--nope"]).is_err());
    }
}
