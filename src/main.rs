mod cli;
mod config;
mod judge;
mod lines;
mod output;
mod pipeline;
mod source;
mod walk;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;

use cli::{Cli, Command, ConfigAction};
use judge::Judge;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("jevgrep: {msg}");
    let _ = std::io::stderr().flush();
    std::process::exit(2);
}

/// Resolve the API key: `TYPESAFE_API_KEY` env (already populated from
/// `.env` by the time this runs) takes precedence over the config file.
fn resolve_api_key() -> String {
    if let Ok(k) = std::env::var("TYPESAFE_API_KEY") {
        if !k.is_empty() {
            return k;
        }
    }

    match config::load() {
        Ok(Some(cfg)) => {
            if let Some(key) = cfg.api_key {
                if !key.is_empty() {
                    return key;
                }
            }
        }
        Ok(None) => {}
        Err(e) => fail(format!("{e:#}")),
    }

    fail(
        "missing API key: run `jevgrep config set-key`, or set TYPESAFE_API_KEY \
         (environment or .env)",
    )
}

/// Handle the `jevgrep config ...` subcommand and exit. Never returns.
fn run_config(action: ConfigAction) -> ! {
    match action {
        ConfigAction::SetKey { key } => {
            let key = match key {
                Some(k) => k,
                None if std::io::stdin().is_terminal() => {
                    match rpassword::prompt_password("TypeSafe API key: ") {
                        Ok(k) => k,
                        Err(e) => fail(format!("reading API key: {e}")),
                    }
                }
                None => {
                    let mut line = String::new();
                    match std::io::stdin().read_line(&mut line) {
                        Ok(_) => line,
                        Err(e) => fail(format!("reading API key: {e}")),
                    }
                }
            };
            let key = key.trim();
            if key.is_empty() {
                fail("API key must not be empty");
            }
            match config::save_api_key(key) {
                Ok(path) => {
                    eprintln!("jevgrep: API key saved to {}", path.display());
                    let _ = std::io::stderr().flush();
                    std::process::exit(0);
                }
                Err(e) => fail(format!("{e:#}")),
            }
        }
        ConfigAction::Show => {
            let path = config::config_path();
            println!("path: {}", path.display());
            let api_key_line = match config::load() {
                Ok(Some(cfg)) => match cfg.api_key {
                    Some(k) if !k.is_empty() => format!("api_key: {}", config::mask_key(&k)),
                    _ => "api_key: (not set)".to_string(),
                },
                Ok(None) => "api_key: (not set)".to_string(),
                Err(e) => fail(format!("{e:#}")),
            };
            println!("{api_key_line}");
            let env_set = std::env::var("TYPESAFE_API_KEY")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            println!(
                "env TYPESAFE_API_KEY: {}",
                if env_set { "set" } else { "unset" }
            );
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
        ConfigAction::Path => {
            println!("{}", config::config_path().display());
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // Load .env from the current directory, without overriding real
    // environment variables that are already set.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    if let Some(Command::Config { action }) = cli.command.clone() {
        run_config(action);
    }

    if cli.query.is_none() {
        fail("a query is required unless using a subcommand, e.g. `jevgrep config set-key`");
    }

    if cli.dry_run {
        if let Err(e) = pipeline::dry_run(&cli) {
            fail(e);
        }
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        return ExitCode::from(0);
    }

    let api_key = resolve_api_key();

    let judge = Arc::new(Judge::new(
        cli.api_url.clone(),
        api_key,
        cli.model.clone(),
        cli.timeout_ms,
        cli.no_cache,
    ));

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                shutdown.store(true, Ordering::Relaxed);
            }
        });
    }

    let outcome = match pipeline::run(&cli, judge, shutdown).await {
        Ok(o) => o,
        Err(e) => fail(e),
    };

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    let code = if outcome.had_error {
        2
    } else if outcome.matched_any {
        0
    } else {
        1
    };

    ExitCode::from(code)
}
