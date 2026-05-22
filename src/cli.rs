use clap::{Parser, Subcommand};

use crate::output::Format;

#[derive(Parser, Debug)]
#[command(
    name = "sift",
    version,
    about = "Pull CN A-share / HK stock data — listings, financials, announcements, quotes, bars — to stdout",
    long_about = "Pull CN A-share / HK stock data — listings, financials, announcements, quotes, bars — to stdout.\n\n\
                  Output is Unix-friendly TSV/JSON by default, with a human-aligned table renderer when no `--format` flag is passed.\n\n\
                  Common starting points:\n  \
                  sift search 茅台                       # find a code by name / pinyin\n  \
                  sift report income 600519 --last 4    # last 4 quarters of income statement\n  \
                  sift announce list 600519 --type 年报 # annual reports for one symbol\n  \
                  sift quote 600519 00700               # current price snapshot\n\n\
                  Run `sift <command> --help` for command-specific options."
)]
pub struct Cli {
    /// Output format: `tsv` | `json` (default: aligned table)
    #[arg(long, global = true, value_parser = parse_user_format)]
    pub format: Option<Format>,

    #[command(subcommand)]
    pub command: Command,
}

/// Custom value parser for `--format`. Yields the internal
/// [`Format`] directly (no `UserFormat` indirection) and rejects
/// `--format table` with an actionable hint pointing back to the
/// "omit the flag" default behavior — a generic `[possible values:
/// tsv, json]` rejection would leave users wondering how to get the
/// table back.
///
/// Returning `Err(String)` from a clap `value_parser` triggers
/// clap's parse-error path → exit code 2, matching other clap-level
/// rejections.
pub fn parse_user_format(s: &str) -> Result<Format, String> {
    match s {
        "tsv" => Ok(Format::Tsv),
        "json" => Ok(Format::Json),
        "table" => {
            Err("table is the default — omit `--format` to get it".into())
        }
        other => Err(format!(
            "unknown format {other:?} — expected `tsv` or `json`"
        )),
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(
        about = "Fuzzy-search CN A-share + HK listings by code, name, or pinyin initials",
        long_about = "Fuzzy-search CN A-share + HK listings by code, name, or pinyin initials.\n\n\
                      Examples:\n  \
                      sift search 茅台\n  \
                      sift search 600\n  \
                      sift search gzmt --limit 5"
    )]
    Search(SearchArgs),
    #[command(
        about = "Financial reports + key indicators (income / balance / cashflow / indicator / periods)",
        long_about = "Financial reports + key indicators (income / balance / cashflow / indicator / periods).\n\n\
                      Examples:\n  \
                      sift report income 600519 --last 4\n  \
                      sift report balance 00700 --period 2024A,2023A\n  \
                      sift report periods 600519"
    )]
    Report {
        #[command(subcommand)]
        cmd: crate::commands::report::ReportCmd,
    },
    #[command(
        about = "Browse, fetch, and download CN A-share / HK announcements (list / show / download / types)",
        long_about = "Browse, fetch, and download CN A-share / HK announcements (list / show / download / types).\n\n\
                      Examples:\n  \
                      sift announce list 600519 --type 年报\n  \
                      sift announce list --start 2024-01-01 --end 2024-03-31\n  \
                      sift announce list 600519 --format json | sift announce download <id> -o ./pdfs"
    )]
    Announce {
        #[command(subcommand)]
        cmd: crate::commands::announce::AnnounceCmd,
    },
    #[command(
        about = "Extract a PDF announcement (by id or local path) as Markdown",
        long_about = "Extract a PDF announcement (by id or local path) as Markdown.\n\n\
                      Examples:\n  \
                      sift extract 1219506510 --pages 1-5\n  \
                      sift extract ./report.pdf --pages 3,7,10-12"
    )]
    Extract(crate::commands::extract::ExtractArgs),
    #[command(
        about = "Current-price snapshot for one or more symbols",
        long_about = "Current-price snapshot for one or more symbols.\n\n\
                      Examples:\n  \
                      sift quote 600519\n  \
                      sift quote 600519 00700 sh000001"
    )]
    Quote(crate::commands::quote::QuoteArgs),
    #[command(
        about = "Historical OHLC bars (daily / weekly / monthly) for one or more symbols",
        long_about = "Historical OHLC bars (daily / weekly / monthly) for one or more symbols.\n\n\
                      Examples:\n  \
                      sift bars 600519 --limit 30\n  \
                      sift bars 600519 --start 2024-01-01 --end 2024-12-31\n  \
                      sift bars 00700 --period weekly --limit 12"
    )]
    Bars(crate::commands::bars::BarsArgs),
}

#[derive(clap::Args, Debug)]
pub struct SearchArgs {
    /// Query: stock code, code prefix, Chinese name substring, or pinyin initials (e.g. `gzmt` for 贵州茅台)
    pub query: String,

    /// Maximum number of matches to return
    #[arg(long, default_value_t = 10)]
    pub limit: u32,

    /// Skip the local cache and force a fresh fetch of the listing
    #[arg(long)]
    pub no_cache: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_format_accepts_tsv_and_json() {
        assert_eq!(parse_user_format("tsv").unwrap(), Format::Tsv);
        assert_eq!(parse_user_format("json").unwrap(), Format::Json);
    }

    #[test]
    fn parse_user_format_rejects_table_with_omit_hint() {
        // The hint must mention both "default" and "omit" so any
        // future rephrase still steers the user to the right action.
        let err = parse_user_format("table").unwrap_err();
        assert!(err.contains("default"), "msg should explain why: {err}");
        assert!(err.contains("omit"), "msg should suggest action: {err}");
    }

    #[test]
    fn parse_user_format_rejects_unknown_listing_expected_values() {
        let err = parse_user_format("xml").unwrap_err();
        assert!(err.contains("xml"), "msg should echo the bad value: {err}");
        assert!(err.contains("tsv"), "msg should list tsv: {err}");
        assert!(err.contains("json"), "msg should list json: {err}");
    }
}
