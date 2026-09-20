mod cli;
mod judge;
mod lines;
mod output;
mod pipeline;
mod walk;

use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;

use cli::Cli;
use judge::Judge;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("jevgrep: {msg}");
    let _ = std::io::stderr().flush();
    std::process::exit(2);
}

#[tokio::main]
async fn main() -> ExitCode {
    // Load .env from the current directory, without overriding real
    // environment variables that are already set.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    if cli.dry_run {
        if let Err(e) = pipeline::dry_run(&cli) {
            fail(e);
        }
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        return ExitCode::from(0);
    }

    let api_key = match std::env::var("TYPESAFE_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => fail("TYPESAFE_API_KEY is not set (env or ./.env)"),
    };

    let judge = Arc::new(Judge::new(
        cli.api_url.clone(),
        api_key,
        cli.model.clone(),
        cli.timeout_ms,
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
