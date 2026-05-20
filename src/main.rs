mod cache;
mod cli;
mod commands;
mod domain;
mod error;
mod http;
mod output;
mod sources;

use clap::Parser;

use crate::cli::{Cli, Command};
use crate::error::SiftError;

fn main() {
    // clap's own argument errors (unknown flag, `--format table`, etc.)
    // are written to stderr and exited with code 2 by `parse()`; they do
    // **not** flow through `SiftError`.
    let cli = Cli::parse();
    let fmt = output::Format::from_user(cli.format);

    let result: Result<(), SiftError> = match cli.command {
        Command::Search(args) => commands::search::run(args, fmt),
    };

    if let Err(e) = result {
        eprintln!("sift: {e}");
        std::process::exit(e.exit_code());
    }
}
