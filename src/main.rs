use anyhow::Context;
use clap::Parser;
use cullr::{cli::Cli, gui};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing();
    gui::run(cli).context("cullr failed")
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
}
