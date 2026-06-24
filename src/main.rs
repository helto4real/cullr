use anyhow::Context;
use clap::Parser;
use cullr::{app::App, cli::Cli};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing();

    let mut app = App::new(cli).context("failed to initialize cullr")?;
    app.run()
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
