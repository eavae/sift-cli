//! `sift extract` — fast / fine / auto PDF extraction.
//!
//! Story-02 wires `fast` end-to-end: PDF open, per-page markdown
//! plus image extraction, scan-page `[warn]` lines, and the full
//! `[info]` header per README. The `fine` and `auto` arms still
//! return `SiftError::Internal("<mode> not yet implemented")` —
//! stories 03 and 04 fill them in.
//!
//! This file is intentionally thin — all PDF / cache / image-dir
//! policy lives in [`crate::fetch::extract`]. The command layer
//! owns argument parsing, the [`info`] header layout, the stdout
//! framing (one markdown body per page, blank line between pages,
//! no synthetic `## Page N` headers — the user can derive page
//! ranges from `--pages`), and exit-code mapping.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args;

use crate::app::AppContext;
use crate::error::SiftError;
use crate::fetch::announce::AnnounceResolver;
use crate::fetch::extract::{self, resolve_image_dir, DocMeta, Target};
use crate::output::Format;
use crate::pdf::pages::PageSpec;

/// `sift extract <target> [options]`.
#[derive(Args, Debug)]
pub struct ExtractArgs {
    /// announcementId (e.g. `1219506510`) or local PDF path (e.g.
    /// `./report.pdf`). sift autodetects which it is from the shape
    /// of the string (all-digit → id; anything else → path).
    pub target: String,

    /// Pages to extract. Accepts single page (`3`), range (`1-5`),
    /// or multi-segment (`1-3,7,10-12`). Omit to take the whole
    /// document — not recommended for long PDFs.
    #[arg(long)]
    pub pages: Option<String>,

    /// Extraction mode: `fast` (default — local pdf-oxide, text-layer
    /// only, zero API calls), `fine` (cloud OCR via PaddleOCR; needs
    /// PADDLEOCR_API_BASE+TOKEN or API_KEY+SECRET, billed per call),
    /// or `auto` (fast first; escalate scanned pages to `fine`).
    #[arg(long, value_enum, default_value_t = Mode::Fast)]
    pub mode: Mode,

    /// Override the image landing directory. Defaults to
    /// `~/.sift/cache/announcements/<id>/images/` for an
    /// announcementId target, or `./extracted_images/` for a
    /// local PDF.
    #[arg(long)]
    pub image_dir: Option<PathBuf>,

    /// Skip the on-disk image cache: rewrite images even when a
    /// file with the same `p<NN>-img<MM>.<ext>` name already
    /// exists. Writes still happen so a subsequent run benefits.
    #[arg(long)]
    pub no_cache: bool,
}

/// The three extraction strategies the user can pick via `--mode`.
/// Lowercase variants match the README / clap value names.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Fast,
    Fine,
    Auto,
}

/// Entry point invoked by `main::run_extract`.
pub fn run(args: ExtractArgs, ctx: &AppContext, _fmt: Format) -> Result<(), SiftError> {
    let target = Target::parse(&args.target)?;
    let pages_opt = args.pages.as_deref().map(PageSpec::parse).transpose()?;

    match args.mode {
        Mode::Fast => fast_run(ctx, &target, pages_opt, args.image_dir.as_deref(), args.no_cache),
        Mode::Fine => fine_run(ctx, &target, pages_opt, args.image_dir.as_deref(), args.no_cache),
        Mode::Auto => auto_run(ctx, &target, pages_opt, args.image_dir.as_deref(), args.no_cache),
    }
}

fn fast_run(
    ctx: &AppContext,
    target: &Target,
    pages_opt: Option<PageSpec>,
    image_dir_override: Option<&Path>,
    no_cache: bool,
) -> Result<(), SiftError> {
    // Open the PDF once so we can validate `--pages`, harvest meta
    // for the `[info]` header, and reuse the same handle across the
    // per-page extract loop. This is the only PDF parse on the
    // happy path.
    let (doc, meta) = extract::open_doc(ctx, target)?;
    let pages = resolve_page_list(pages_opt, meta.page_count);
    extract::validate_pages(&pages, meta.page_count)?;

    let (image_dir, using_default) = resolve_image_dir(ctx, target, image_dir_override);

    print_info_header(ctx, target, &meta);
    if using_default && matches!(target, Target::LocalPdf(_)) {
        let _ = writeln!(
            std::io::stderr().lock(),
            "[info] image_dir   {}",
            image_dir.display()
        );
    }
    if pages_opt_was_none(&pages, meta.page_count) {
        let _ = writeln!(
            std::io::stderr().lock(),
            "[warn] extracting full document ({} pages); pass --pages to limit scope",
            meta.page_count,
        );
    }

    let out = extract::fast(
        &doc,
        target,
        &pages,
        &image_dir,
        meta.page_count,
        no_cache,
        true,
    )?;
    write_stdout(&out.pages)
}

fn fine_run(
    ctx: &AppContext,
    target: &Target,
    pages_opt: Option<PageSpec>,
    image_dir_override: Option<&Path>,
    no_cache: bool,
) -> Result<(), SiftError> {
    // Fail fast before anything else: a fine run without
    // credentials will burn retries against the wrong endpoint
    // for nothing. `paddleocr::build_client` knows the env-var
    // contract for both backends; we discard the constructed
    // client immediately and let the orchestrator build a fresh
    // one inside its own scope.
    let _ = crate::sources::paddleocr::build_client(&ctx.http)?;

    let (doc, meta) = extract::open_doc(ctx, target)?;
    let pages = resolve_page_list(pages_opt, meta.page_count);
    extract::validate_pages(&pages, meta.page_count)?;

    let (image_dir, using_default) = resolve_image_dir(ctx, target, image_dir_override);

    print_info_header(ctx, target, &meta);
    if using_default && matches!(target, Target::LocalPdf(_)) {
        let _ = writeln!(
            std::io::stderr().lock(),
            "[info] image_dir   {}",
            image_dir.display()
        );
    }
    if matches!(target, Target::LocalPdf(_)) {
        let _ = writeln!(
            std::io::stderr().lock(),
            "[info] cache=disabled (local PDF without announcement_id)",
        );
    }

    drop(doc); // fine() re-opens internally for its own DocumentEditor.
    let outcome = extract::fine(ctx, target, &pages, &image_dir, no_cache)?;

    let _ = writeln!(
        std::io::stderr().lock(),
        "[fine] backend={} cached {} / API {} pages ({} batch{}); eta ~{}s, billed per call",
        outcome.backend,
        outcome.cached_n,
        outcome.api_n,
        outcome.batches,
        if outcome.batches == 1 { "" } else { "es" },
        outcome.eta_secs,
    );

    let stdout_pages: Vec<String> = outcome.pages.iter().map(|(_, md)| md.clone()).collect();
    write_stdout(&stdout_pages)?;

    if !outcome.failed.is_empty() {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(
            err,
            "[fine] {} page(s) failed: {}",
            outcome.failed.len(),
            outcome
                .failed
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
        );
        // Surface the underlying cause(s) — otherwise a 500 from
        // PaddleOCR, a token issue, or a malformed response all
        // look the same to the user.
        for reason in &outcome.failure_reasons {
            let _ = writeln!(err, "[fine] cause: {reason}");
        }
        drop(err);
        return Err(SiftError::Network(format!(
            "{} page(s) failed",
            outcome.failed.len()
        )));
    }

    Ok(())
}

/// auto-mode orchestrator. The flow:
///
/// 1. Open the PDF + print the standard `[info]` header (same as
///    fast / fine — gives users the page count, size, text layer
///    average up front).
/// 2. Run a `fast` pass with `emit_scan_warn=false` (we don't want
///    fast's own coalesced warn line to compete with the auto-mode
///    `[fine]` summary below). Scanned pages bubble back via
///    [`extract::FastOutput::scanned`].
/// 3. If no scanned pages → write fast output straight to stdout
///    and exit cleanly. This is the **zero-API zero-token** path:
///    auto on a text-only PDF behaves exactly like `--mode fast`.
/// 4. Otherwise → preflight PaddleOCR credentials. Surface a
///    bespoke error that names the scanned page ranges so the
///    user knows what's at stake (vs. the generic
///    `OcrTokenMissing` from `fine_run`).
/// 5. Call `fine` on the scanned subset; merge fine output over
///    fast output by page number. Pages whose chunk failed in
///    fine are left at their original fast body — the user still
///    gets *something* (the image + sparse text) instead of a
///    hole, with a `[warn]` per failed page.
fn auto_run(
    ctx: &AppContext,
    target: &Target,
    pages_opt: Option<PageSpec>,
    image_dir_override: Option<&Path>,
    no_cache: bool,
) -> Result<(), SiftError> {
    let (doc, meta) = extract::open_doc(ctx, target)?;
    let pages = resolve_page_list(pages_opt, meta.page_count);
    extract::validate_pages(&pages, meta.page_count)?;

    let (image_dir, using_default) = resolve_image_dir(ctx, target, image_dir_override);

    print_info_header(ctx, target, &meta);
    if using_default && matches!(target, Target::LocalPdf(_)) {
        let _ = writeln!(
            std::io::stderr().lock(),
            "[info] image_dir   {}",
            image_dir.display()
        );
    }

    // fast pass: suppress its own [warn] summary — we'll emit a
    // single [fine] auto line below that's more actionable.
    let fast = extract::fast(
        &doc,
        target,
        &pages,
        &image_dir,
        meta.page_count,
        no_cache,
        false,
    )?;
    drop(doc);

    if fast.scanned.is_empty() {
        let _ = writeln!(
            std::io::stderr().lock(),
            "[info] auto       all pages have text layer; zero OCR calls",
        );
        return write_stdout(&fast.pages);
    }

    let scanned_label = extract::format_page_list(&fast.scanned);

    // Token / OAuth preflight — only now, because for text-only
    // PDFs we should never have failed on missing credentials.
    if crate::sources::paddleocr::build_client(&ctx.http).is_err() {
        return Err(SiftError::Internal(format!(
            "auto detected {} scanned page(s) ({}) requiring OCR but PaddleOCR is not configured; \
             set PADDLEOCR_API_BASE+PADDLEOCR_API_TOKEN or PADDLEOCR_API_KEY+PADDLEOCR_SECRET_KEY, \
             or use --mode fast to skip OCR",
            fast.scanned.len(),
            scanned_label,
        )));
    }

    let batches = fast.scanned.len().div_ceil(crate::sources::paddleocr::PADDLEOCR_BATCH_SIZE);
    let eta_secs = batches as u64 * 3; // FINE_SEC_PER_BATCH heuristic
    let _ = writeln!(
        std::io::stderr().lock(),
        "[fine] auto: {} scanned page(s) escalated to OCR ({}); eta ~{}s, billed per call",
        fast.scanned.len(),
        scanned_label,
        eta_secs,
    );

    let outcome = extract::fine(ctx, target, &fast.scanned, &image_dir, no_cache)?;
    let fine_pages: std::collections::HashMap<u32, String> = outcome.pages.into_iter().collect();

    // Merge: walk the original `pages` order; prefer fine output
    // when present, fall back to fast output otherwise (covers
    // non-escalated pages AND OCR-failed escalated pages).
    let merged: Vec<String> = pages
        .iter()
        .zip(fast.pages.iter())
        .map(|(page, fast_md)| {
            fine_pages
                .get(page)
                .cloned()
                .unwrap_or_else(|| fast_md.clone())
        })
        .collect();
    write_stdout(&merged)?;

    if !outcome.failed.is_empty() {
        let mut err = std::io::stderr().lock();
        for p in &outcome.failed {
            let _ = writeln!(
                err,
                "[warn] auto: page {p} OCR failed, falling back to fast output",
            );
        }
        for reason in &outcome.failure_reasons {
            let _ = writeln!(err, "[fine] cause: {reason}");
        }
        drop(err);
        // Only fail the command when *every* escalated page failed;
        // otherwise the user got partial OCR output and the
        // command is still useful.
        if outcome.failed.len() == fast.scanned.len() {
            return Err(SiftError::Network(format!(
                "auto: all {} OCR page(s) failed",
                outcome.failed.len()
            )));
        }
    }

    Ok(())
}

/// `[info]` header per README's `stderr 元信息 header` template.
/// Writes the full 5-row variant for an announcementId (`target`,
/// `cached`, `pages`, `size`, `text_layer`, `hint`) and a 4-row
/// variant for a local PDF (`file`, `pages`, `size`, `text_layer`,
/// `hint`). Errors writing stderr are silently dropped — the user
/// already lost their terminal if that's broken.
fn print_info_header(ctx: &AppContext, target: &Target, meta: &DocMeta) {
    let mut err = std::io::stderr().lock();
    match target {
        Target::AnnouncementId(id) => {
            let resolver = AnnounceResolver::new(ctx);
            let path_label = resolver
                .pdf_path(id)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<cache disabled>".into());
            let cached = if resolver.is_pdf_cached(id) { "yes" } else { "no" };
            let _ = writeln!(err, "[info] target      {id}");
            let _ = writeln!(err, "[info] cached      {path_label}  ({cached})");
        }
        Target::LocalPdf(_) => {
            let _ = writeln!(err, "[info] file        {}", meta.pdf_path.display());
        }
    }
    let _ = writeln!(err, "[info] pages       {}", meta.page_count);
    let _ = writeln!(err, "[info] size        {}", human_size(meta.size_bytes));
    let avg = meta.text_layer_avg;
    let has_text = avg > 1.0;
    let _ = writeln!(
        err,
        "[info] text_layer  {} (avg {} chars/page)",
        if has_text { "yes" } else { "no" },
        avg.round() as u64,
    );
    let hint = if has_text {
        "all pages have text layer; --mode fast should work"
    } else {
        "some pages may be scanned; consider --mode auto"
    };
    let _ = writeln!(err, "[info] hint        {hint}");
}

fn write_stdout(pages: &[String]) -> Result<(), SiftError> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut first = true;
    for md in pages {
        if md.is_empty() {
            continue;
        }
        if !first {
            writeln!(out).map_err(crate::output::io_err)?;
        }
        first = false;
        writeln!(out, "{}", md).map_err(crate::output::io_err)?;
    }
    Ok(())
}

fn resolve_page_list(pages_opt: Option<PageSpec>, page_count: u32) -> Vec<u32> {
    match pages_opt {
        Some(spec) => spec.0,
        None => (1..=page_count).collect(),
    }
}

/// True when the user did not pass `--pages` — we infer this from
/// the resolved list being exactly `1..=N`. Used to gate the
/// "extracting full document" stderr warning.
fn pages_opt_was_none(pages: &[u32], page_count: u32) -> bool {
    pages.len() as u64 == u64::from(page_count)
        && pages.first().copied() == Some(1)
        && pages.last().copied() == Some(page_count)
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_picks_right_unit() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2 * 1024), "2.0 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn resolve_page_list_defaults_to_full_range() {
        assert_eq!(resolve_page_list(None, 3), vec![1, 2, 3]);
    }

    #[test]
    fn resolve_page_list_uses_explicit_spec() {
        let spec = PageSpec::parse("2,4").unwrap();
        assert_eq!(resolve_page_list(Some(spec), 10), vec![2, 4]);
    }

    #[test]
    fn pages_opt_was_none_detects_full_range() {
        assert!(pages_opt_was_none(&[1, 2, 3], 3));
        assert!(!pages_opt_was_none(&[1, 2], 3));
        assert!(!pages_opt_was_none(&[2, 3], 3));
    }

}
