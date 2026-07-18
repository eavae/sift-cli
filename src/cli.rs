use clap::{Parser, Subcommand};

use crate::output::Format;

#[derive(Parser, Debug)]
#[command(
    name = "sift",
    version,
    about = "Pull CN A-share / HK stock data — listings, reports, announcements, extracts, quotes, bars — to stdout",
    long_about = "Pull CN A-share / HK stock data — listings, reports, announcements, extracts, quotes, bars — to stdout.\n\n\
                  Output is Unix-friendly TSV / NDJSON by default; omit `--format` for a human-aligned table. \
                  All commands accept multiple symbols where it makes sense and degrade gracefully (per-symbol failures \
                  surface as `[warn]` lines on stderr while successful rows still reach stdout).",
    after_long_help = "Examples:\n  \
                       sift search gzmt --limit 3                                              # pinyin → top 3 matches\n  \
                       sift report income 600519 600036 --last 4 --scope parent --unit yi      # multi-symbol, parent-only, in 亿\n  \
                       sift report indicator 600519 --start 2020 --end 2024 --annual           # 5-year annual ratios (A-share)\n  \
                       sift announce list 600519 --type 定期报告 --start 2024-01-01 --end 2025-12-31 --limit 50\n  \
                       sift announce list 600519 --format json | sift announce download <id> -o ./pdfs\n  \
                       sift extract 1219506510 --pages 1-20 --mode auto > report.md           # OCR-escalate scanned pages\n  \
                       sift bars 600519 00700 --period weekly --limit 52 --format tsv > weekly.tsv\n\n\
                       Run `sift <command> --help` for command-specific options."
)]
pub struct Cli {
    /// Output format: `tsv` | `json` (NDJSON — one object per line).
    /// Omit the flag for the default aligned table; `--format table`
    /// is rejected with a hint pointing back to that default.
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
        after_long_help = "Examples:\n  \
                           sift search 茅台                                         # name substring\n  \
                           sift search 600 --limit 20                              # code prefix, expand result cap\n  \
                           sift search gzmt --limit 3 --no-cache                   # pinyin initials, bypass listing cache\n  \
                           sift search 银行 --format json | jq -r .code | xargs sift quote   # search → quote pipeline"
    )]
    Search(SearchArgs),
    #[command(
        about = "Financial reports + key indicators (income / balance / cashflow / indicator / periods)",
        after_long_help = "Examples:\n  \
                           sift report income 600519 --last 4 --unit yi\n  \
                           sift report balance 600519 600036 --period 2024A,2023A --scope parent --format tsv\n  \
                           sift report indicator 600519 --start 2020 --end 2024 --annual --items ROE加权,EPS,毛利率\n  \
                           sift report cashflow 600519 --period 2024Q3 --source eastmoney    # pin upstream for repro\n  \
                           sift report periods 600519                                        # what's available?"
    )]
    Report {
        #[command(subcommand)]
        cmd: crate::commands::report::ReportCmd,
    },
    #[command(
        about = "Browse, fetch, and download CN A-share / HK announcements (list / show / download / types)",
        after_long_help = "Examples:\n  \
                           sift announce list 600519 --type 年报 --limit 5\n  \
                           sift announce list 600519 00700 --start 2024-01-01 --end 2024-06-30\n  \
                           sift announce list --start 2025-04-01 --end 2025-04-30 --keyword 减持 --limit 100\n  \
                           sift announce list 600519 --type 定期报告 --start 2023-01-01 --end 2025-12-31    # aggregate 4 sub-types\n  \
                           sift announce list 600519 --format json | sift announce download <id> -o ./pdfs"
    )]
    Announce {
        #[command(subcommand)]
        cmd: crate::commands::announce::AnnounceCmd,
    },
    #[command(
        about = "Extract a PDF announcement (by id or local path) as Markdown",
        after_long_help = "Examples:\n  \
                           sift extract 1219506510 --pages 1-5                              # cached PDF, first 5 pages\n  \
                           sift extract ./report.pdf --pages 3,7,10-12 --mode auto          # local PDF, OCR-on-demand\n  \
                           sift extract 1219506510 --mode auto --pages 1-30 > report.md     # auto + redirect\n  \
                           sift announce download 1219506510 -o /tmp && sift extract 1219506510 --mode auto --pages 1-30"
    )]
    Extract(crate::commands::extract::ExtractArgs),
    #[command(
        about = "Current-price snapshot for one or more symbols",
        after_long_help = "Examples:\n  \
                           sift quote 600519\n  \
                           sift quote 600519 00700 sh000001 --format tsv\n  \
                           sift search 银行 --limit 5 --format json | jq -r .code | xargs sift quote   # batch from search"
    )]
    Quote(crate::commands::quote::QuoteArgs),
    #[command(
        about = "Historical OHLC bars (daily / weekly / monthly) for one or more symbols",
        after_long_help = "Examples:\n  \
                           sift bars 600519 --limit 30                                              # last 30 daily bars\n  \
                           sift bars 600519 00700 --period weekly --limit 52 --format tsv           # multi-symbol, 1 year weekly\n  \
                           sift bars 600519 --start 2024-01-01 --end 2024-12-31 --adjust pre        # explicit range, pre-adjusted\n  \
                           sift bars 600519 --period monthly --limit 24 --source eastmoney         # 2y monthly, EM upstream"
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
