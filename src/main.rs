use std::process::ExitCode;

use clap::Parser;

mod callgraph;
mod channel;
mod cli;
mod diff;
mod embed;
mod eval;
mod grep;
mod hierarchy;
mod history;
mod index;
mod integrations;
mod parse;
mod pattern;
mod protocol;
mod search;
mod store;
mod util;
mod watch;
mod workspace;

use cli::args::Cli;

/// v1.12.0 S8.2 — explicit 0 / 1 / 2 exit-code contract.
///
/// - `ExitCode::SUCCESS` (0): the command produced results or an action
///   completed (`vex index`, `vex update`, etc.).
/// - `ExitCode::from(1)`: the command's expected-empty outcome —
///   `vex search Foo` found nothing, `vex callers Bar` returned no
///   edges, `vex find_symbol X` could not locate the symbol. Scripts /
///   CI gate on this distinct from real errors.
/// - `ExitCode::from(2)`: an actual error — bad regex, corrupted index,
///   I/O failure, invalid arguments past clap. anyhow's auto-formatted
///   error message is emitted to stderr (`Error: ...`) before exit.
fn main() -> ExitCode {
    // Route tracing to stderr so it never corrupts stdout-bound JSON
    // envelopes consumed by the MCP server (which parses `vex` stdout
    // as JSON). Any `tracing::warn!`/`debug!` on stdout would prepend
    // to the envelope and break `serde_json::from_str`.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli::dispatch(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e:#}");
            ExitCode::from(2)
        }
    }
}
