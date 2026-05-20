use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "sift",
    version,
    about = "Fuzzy-search CN A-share + HK stock listings from cninfo"
)]
pub struct Cli {
    /// Output format. Omit to use the internal table renderer
    /// (human-aligned columns). `table` is intentionally not a user-visible
    /// value — passing `--format table` is rejected by clap (exit 2).
    #[arg(long, value_enum, global = true)]
    pub format: Option<UserFormat>,

    #[command(subcommand)]
    pub command: Command,
}

/// User-visible values for `--format`.
///
/// The internal default [`crate::output::Format::Table`] is **not** exposed
/// as a clap value; an explicit `--format table` is rejected by clap and
/// terminates the process with exit code 2.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserFormat {
    Tsv,
    Json,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Fuzzy-search the cninfo A-share + HK listings
    Search(SearchArgs),
}

#[derive(clap::Args, Debug)]
pub struct SearchArgs {
    /// Query: stock code / code prefix / Chinese name substring / pinyin initials
    pub query: String,

    /// Maximum number of matches to return
    #[arg(long, default_value_t = 10)]
    pub limit: u32,

    /// Skip the local cache and force a fresh fetch of the listing
    #[arg(long)]
    pub no_cache: bool,
}
