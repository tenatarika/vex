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
    // Route tracing to stderr so it never corrupts stdout-bound JSON
    // envelopes consumed by the MCP server (which parses `vex` stdout
    // as JSON). Any `tracing::warn!`/`debug!` on stdout would prepend
    // to the envelope and break `serde_json::from_str`.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    cli::dispatch(cli)
}
