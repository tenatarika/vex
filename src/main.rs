use anyhow::Result;
use clap::Parser;

mod callgraph;
mod cli;
mod diff;
mod embed;
mod eval;
mod grep;
mod hierarchy;
mod index;
mod parse;
mod pattern;
mod protocol;
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
