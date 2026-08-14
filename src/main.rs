use anyhow::Result;
use clap::Parser;

use seedle::cli::Cli;
use seedle::commands;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose, cli.quiet);

    if let Err(e) = run(cli).await {
        // `{:#}` renders the whole anyhow context chain, which is where the
        // "what were we doing" detail lives.
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    commands::dispatch(cli).await
}

fn init_tracing(verbose: bool, quiet: bool) {
    let default = if quiet {
        "error"
    } else if verbose {
        "seedle=debug,sqlx=warn"
    } else {
        "seedle=info,sqlx=error"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .try_init();
}
