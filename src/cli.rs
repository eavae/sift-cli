use clap::{Parser, Subcommand};

use crate::output::Format;

#[derive(Parser, Debug)]
#[command(
    name = "sift",
    version,
    about = "Fuzzy-search CN A-share + HK stock listings from cninfo"
)]
pub struct Cli {
    /// Output format: `tsv` | `json`. Omit for the default
    /// human-aligned table renderer. Passing `--format table` is
    /// rejected with a hint to drop the flag.
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
    /// Fuzzy-search the cninfo A-share + HK listings
    Search(SearchArgs),
    /// Financial reports + key indicators (income / balance /
    /// cashflow / indicator / periods)
    Report {
        #[command(subcommand)]
        cmd: crate::commands::report::ReportCmd,
    },
    /// Announcements: list / show / download / types
    Announce {
        #[command(subcommand)]
        cmd: crate::commands::announce::AnnounceCmd,
    },
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
