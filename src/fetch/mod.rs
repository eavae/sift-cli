//! Fetch — the cache + source + fallback coordination layer.
//!
//! Sits between `commands/` (CLI dispatch + rendering) and
//! `sources/` + `cache/` (HTTP / DuckDB primitives). One feature
//! per file: every module here exposes the "given a query / id,
//! resolve the domain value" operations a command needs.
//!
//! ## Why a separate layer
//!
//! The original 7-layer diagram has commands calling sources +
//! cache directly. In practice that breaks down whenever a feature
//! has multiple sources (F2 financials: race two upstreams, cache
//! winners) or non-trivial fallback (F3 announce: cache → stdin
//! pipe → whole-market paginate). Coordinating those inside
//! `commands/` mixes user-facing concerns (CLI args, rendering)
//! with data-access policy (cache freshness, fallback order). The
//! commands files balloon and the cache / source dependencies leak
//! into every subcommand.
//!
//! `fetch/` exists so commands can say "give me this row" once and
//! let one place own the strategy. See
//! [`docs/refactor-fetch-layer/`](../../docs/refactor-fetch-layer/README.md)
//! for the full motivation and migration plan.
//!
//! ## Access discipline
//!
//! `commands/**` accesses external data only through `fetch::*` and
//! `output::*`. Direct imports of `sources::cninfo::Announcements` /
//! `sources::cninfo::AnnouncementQuery` / `cache::record::*` from
//! `commands/` are forbidden — see the project invariant table.
//!
//! Both project caches live on [`crate::app::AppContext`]:
//! - `ctx.file_cache: Option<FileCache>` — F1 listings JSON and F3
//!   PDF binaries on the filesystem
//! - `ctx.records_cache: Option<RecordCache>` — F2 financials + F3
//!   announce metadata, blob-by-key in DuckDB
//!
//! `fetch::*` is the only layer that handles either directly. F3 PDF
//! operations (`is_pdf_cached`, `pdf_path`, `download_pdf`,
//! `copy_pdf_to`) hang off [`crate::fetch::announce::AnnounceResolver`].

pub mod announce;
pub mod report;
pub mod search;
