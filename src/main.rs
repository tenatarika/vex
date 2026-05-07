use anyhow::Result;
use clap::Parser;

mod cli;
mod embed;
mod grep;
mod index;
mod parse;
mod pattern;
mod search;
mod store;
mod util;
mod watch;

use cli::args::Cli;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    cli::dispatch(cli)
}
