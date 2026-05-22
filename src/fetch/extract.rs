//! Data-access coordinator for `sift extract` (F4 fast mode).
//!
//! `commands/extract.rs` calls into this module for everything that
//! touches PDF bytes, the F3 PDF file cache, or the image landing
//! directory. The split mirrors `fetch::announce` / `fetch::report` /
//! `fetch::search`: commands own user-facing wiring and rendering,
//! `fetch::*` owns the resolve / cache / encode policy.
//!
//! Story-02 lands the `fast` orchestration: open the PDF (via
//! `AnnounceResolver` for an announcementId, direct read for a local
//! path), walk the requested pages once, harvest text + images +
//! scan verdict per page, write images to the configured directory,
//! and rewrite the markdown to reference them by absolute path.
//!
//! Stories 03 / 04 will add `fine` (PaddleOCR) and `auto` (mixed)
//! orchestrators alongside.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::app::AppContext;
use crate::cache::file::FileCache;
use crate::error::SiftError;
use crate::fetch::announce::AnnounceResolver;
use crate::pdf::extract::{EmbeddedImage, PdfDoc};
use crate::pdf::scan_detect::classify;
use crate::sources::paddleocr::{
    self, OcrClient, PageResult, PADDLEOCR_BATCH_SIZE,
};

const PADDLEOCR_PARALLELISM: usize = 4;
/// Rough per-batch time estimate for the `[fine]` budget line.
/// Picked to match real-world end-to-end (PDF assemble + upload +
/// OCR + download + decode) for a fully-loaded 8-page chunk — the
/// constant is a UX hint, not a real budget, so a couple of seconds
/// either way is fine.
const FINE_SEC_PER_BATCH: u64 = 3;

/// Resolved form of the user's `<target>` argument. The CLI layer
/// parses the raw string via [`Target::parse`]; from there on,
/// `Target` flows through `fetch::extract` and never up into the
/// command layer except as a borrow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    LocalPdf(PathBuf),
    AnnouncementId(String),
}

impl Target {
    /// Apply the README rules in order:
    /// 1. contains `/` or `\` → local PDF path;
    /// 2. ends with `.pdf` / `.PDF` → local PDF path;
    /// 3. all ASCII digits → announcementId;
    /// 4. otherwise → `SiftError::Parse`.
    pub fn parse(s: &str) -> Result<Self, SiftError> {
        if s.contains('/') || s.contains('\\') {
            return Ok(Self::LocalPdf(PathBuf::from(s)));
        }
        let lower = s.to_ascii_lowercase();
        if lower.ends_with(".pdf") {
            return Ok(Self::LocalPdf(PathBuf::from(s)));
        }
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
            return Ok(Self::AnnouncementId(s.to_owned()));
        }
        Err(SiftError::Parse(format!(
            "cannot interpret {s:?} as announcementId or PDF file path"
        )))
    }
}

/// Document-level metadata harvested at open time. Fed to the
/// `[info]` header in `commands::extract` so the command doesn't
/// need to hold the [`PdfDoc`] handle itself.
#[derive(Debug, Clone)]
pub struct DocMeta {
    pub page_count: u32,
    /// PDF file size in bytes. Populated when the on-disk path is
    /// known (which it always is for both targets in story-02).
    pub size_bytes: u64,
    /// Average characters per page across the whole document.
    /// Drives the `text_layer` info row + the `hint` line.
    pub text_layer_avg: f32,
    /// Same path the orchestrator resolved internally — handed back
    /// so the command layer can print `[info] file <path>` for a
    /// LocalPdf target after canonicalization.
    pub pdf_path: PathBuf,
}

/// Per-page output from [`fast`]. Carries both the rendered
/// markdown bodies (in the order the caller's `pages` slice
/// asked for) and the **subset** of page numbers `scan_detect`
/// flagged as scanned. The two together let
/// [`crate::commands::extract`]'s auto-mode orchestrator escalate
/// scanned pages to `fine` without re-running the fast pass.
///
/// The `pages: Vec<String>` is indexed positionally — `pages[i]`
/// corresponds to the i-th page number in the caller's input
/// slice. `scanned: Vec<u32>` lists page numbers (not indices) so
/// callers don't have to round-trip back to the input slice to
/// know which pages need OCR.
#[derive(Debug, Clone)]
pub struct FastOutput {
    pub pages: Vec<String>,
    pub scanned: Vec<u32>,
}

/// Resolve a [`Target`] to the on-disk PDF path.
///
/// - `LocalPdf` → check the file exists; **no** canonicalize here
///   (we'd lose the user's preferred display form). Future stories
///   can canonicalize for the `[info]` header.
/// - `AnnouncementId` → `AnnounceResolver::pdf_path` + `is_pdf_cached`;
///   missing returns the actionable "run `sift announce download`"
///   hint per the story.
pub fn resolve_pdf_path(ctx: &AppContext, target: &Target) -> Result<PathBuf, SiftError> {
    match target {
        Target::LocalPdf(p) => {
            if p.is_file() {
                Ok(p.clone())
            } else {
                Err(SiftError::Io(format!("PDF not found: {}", p.display())))
            }
        }
        Target::AnnouncementId(id) => {
            let resolver = AnnounceResolver::new(ctx);
            match (resolver.pdf_path(id), resolver.is_pdf_cached(id)) {
                (Some(p), true) => Ok(p),
                _ => Err(SiftError::Io(format!(
                    "PDF not cached for {id}; run `sift announce download {id} -o <dir>` first"
                ))),
            }
        }
    }
}

/// Resolve the directory where extracted images should land.
/// Returns the path and a `bool` indicating whether the value was
/// the implicit default (so the command layer can decide whether
/// to print the `[info] image_dir` advisory). Order:
///
/// 1. `override_` — the user passed `--image-dir <p>`; honored as-is.
/// 2. `AnnouncementId` + a configured file cache →
///    `<cache_root>/announcements/<id>/images/`.
/// 3. `AnnouncementId` without a file cache → `./extracted_images/`
///    (CWD) — caches are disabled and we still need somewhere to land.
/// 4. `LocalPdf` → `./extracted_images/` (CWD).
pub fn resolve_image_dir(
    ctx: &AppContext,
    target: &Target,
    override_: Option<&Path>,
) -> (PathBuf, bool) {
    if let Some(p) = override_ {
        return (p.to_path_buf(), false);
    }
    match (target, ctx.file_cache.as_ref()) {
        (Target::AnnouncementId(id), Some(fc)) => {
            (fc.path(&format!("announcements/{id}/images")), true)
        }
        _ => (PathBuf::from("extracted_images"), true),
    }
}

/// Open the PDF for `target`, harvest [`DocMeta`], and hand both
/// back. Separate from [`fast`] so the command layer can print the
/// stderr `[info]` header **before** the per-page extraction loop
/// kicks off — long PDFs would otherwise leave the user staring at
/// a silent terminal.
pub fn open_doc(ctx: &AppContext, target: &Target) -> Result<(PdfDoc, DocMeta), SiftError> {
    let pdf_path = resolve_pdf_path(ctx, target)?;
    let size_bytes = std::fs::metadata(&pdf_path).map(|m| m.len()).unwrap_or(0);
    let doc = PdfDoc::open(&pdf_path)?;
    let page_count = doc.page_count()?;
    let text_layer_avg = doc.text_layer_avg()?;
    Ok((
        doc,
        DocMeta {
            page_count,
            size_bytes,
            text_layer_avg,
            pdf_path,
        },
    ))
}

/// Validate every page in `pages` is within `1..=max`. Bails on the
/// first out-of-range page with a `SiftError::Parse` mentioning both
/// the offending page and the document length — saves the user a
/// trip back to `--help` to discover the PDF only has N pages.
pub fn validate_pages(pages: &[u32], max: u32) -> Result<(), SiftError> {
    for &p in pages {
        if p == 0 || p > max {
            return Err(SiftError::Parse(format!(
                "page {p} out of range (PDF has {max} pages)"
            )));
        }
    }
    Ok(())
}

/// fast-mode orchestrator. Walks `pages` in order, returning one
/// markdown body per page (in the same order). For each page:
///
/// 1. Extract markdown via `pdf-oxide` (text layer + headings + tables).
/// 2. Extract embedded images; encode each to JPEG or PNG.
/// 3. Run [`classify`] to decide if it's a scanned page.
/// 4. Write images to `image_dir` under `p<NN>-img<MM>.<ext>` —
///    skipping write when an identical-name file already exists and
///    `no_cache` is false.
/// 5. Append `![](<absolute path>)` lines to the page's markdown so
///    the rendered result links the freshly-landed images.
///
/// After the loop, if `emit_scan_warn` and any pages were classified
/// as scanned, a **single** consolidated `[warn]` block is printed
/// to stderr — consecutive scanned pages are coalesced into ranges
/// (`12-15,18,22-30`) and the suggested retry command is the
/// minimal `--mode fine` re-extract.
///
/// The function is single-threaded and synchronous: F4's fast path
/// is meant to be local-only and millisecond-fast; the OCR / batch
/// concurrency lives in fine/auto (stories 03/04).
pub fn fast(
    doc: &PdfDoc,
    target: &Target,
    pages: &[u32],
    image_dir: &Path,
    page_count: u32,
    no_cache: bool,
    emit_scan_warn: bool,
) -> Result<FastOutput, SiftError> {
    validate_pages(pages, page_count)?;

    let page_num_width = digits(page_count);
    let mut out = Vec::with_capacity(pages.len());
    let mut scanned: Vec<u32> = Vec::new();

    for &page in pages {
        let md = doc.page_markdown(page)?;
        let images = doc.page_images(page)?;
        let area_px = doc.page_area_px(page)?;
        let verdict = classify(&md, &images, area_px);

        let image_lines = land_images(image_dir, page, page_num_width, &images, no_cache)?;
        let mut combined = md.trim_end().to_string();
        if !image_lines.is_empty() {
            if !combined.is_empty() {
                combined.push_str("\n\n");
            }
            combined.push_str(&image_lines.join("\n\n"));
        }
        // README §"扫描页 stdout": no placeholder line — an empty
        // body is fine for a pure-scan page with no embedded raster.

        if verdict.is_scanned {
            scanned.push(page);
        }

        out.push(combined);
    }

    if emit_scan_warn && !scanned.is_empty() {
        emit_scan_summary(target, &scanned);
    }

    Ok(FastOutput {
        pages: out,
        scanned,
    })
}

/// Summary returned by [`fine`]: per-page markdown (cached + freshly
/// OCR'd, page-sorted ascending), counts to drive the `[fine]`
/// budget line, and the list of pages whose chunk failed so the
/// command layer can exit with code 3 + list the failures.
#[derive(Debug, Clone)]
pub struct FineOutcome {
    pub pages: Vec<(u32, String)>,
    pub cached_n: usize,
    pub api_n: usize,
    pub batches: usize,
    pub failed: Vec<u32>,
    /// Distinct error messages observed across failed chunks
    /// (deduped, in first-seen order). Surfaced to stderr by
    /// `commands::extract::fine_run` so the user sees the actual
    /// upstream / decode failure — not just "page failed".
    pub failure_reasons: Vec<String>,
    /// Backend that handled this run ("token" or "oauth").
    /// Surfaced in the `[fine]` budget line so it's obvious which
    /// credentials were picked up.
    pub backend: &'static str,
    /// Rough wall-clock estimate (seconds) for the `[fine]` line.
    pub eta_secs: u64,
}

/// fine-mode orchestrator. Walks `pages` in order:
///
/// 1. Open the source PDF (announcementId or local).
/// 2. Partition pages into `cached_pages` (hit the on-disk OCR md
///    cache) vs `missing_pages` (need a PaddleOCR call).
/// 3. Slice `missing_pages` into ≤ [`PADDLEOCR_BATCH_SIZE`]-page
///    chunks; each chunk becomes one HTTP request.
/// 4. Dispatch chunks across [`PADDLEOCR_PARALLELISM`] worker
///    threads via [`std::thread::scope`] + [`mpsc`]. Each worker
///    repackages its chunk into a small multi-page PDF via
///    [`PdfDoc::split_into_batch`], posts to PaddleOCR, decodes the
///    response, lands per-page md to cache, and writes images.
/// 5. Merge cached + fresh, sort by page, return.
///
/// Chunk failure is atomic: any error in one worker drops every
/// page in that chunk to `failed` without writing any of its md
/// to cache. Other chunks proceed independently — partial progress
/// is the whole point of the `(announcement_id, page)` cache.
pub fn fine(
    ctx: &AppContext,
    target: &Target,
    pages: &[u32],
    image_dir: &Path,
    no_cache: bool,
) -> Result<FineOutcome, SiftError> {
    let (doc, meta) = open_doc(ctx, target)?;
    validate_pages(pages, meta.page_count)?;

    let page_num_width = digits(meta.page_count);
    let announce_id = match target {
        Target::AnnouncementId(id) => Some(id.as_str()),
        Target::LocalPdf(_) => None,
    };
    let used_cache = announce_id.is_some() && ctx.file_cache.is_some();

    // Tier 1: serve any pages already on disk. Bypassed under
    // `--no-cache` and unavailable for local PDF targets.
    let mut cached: HashMap<u32, String> = HashMap::new();
    if used_cache && !no_cache {
        if let (Some(id), Some(files)) = (announce_id, ctx.file_cache.as_ref()) {
            for &p in pages {
                if let Some(md) = ocr_cached(files, id, p, page_num_width) {
                    cached.insert(p, md);
                }
            }
        }
    }

    let missing: Vec<u32> = pages.iter().copied().filter(|p| !cached.contains_key(p)).collect();
    let chunks: Vec<Vec<u32>> = missing
        .chunks(PADDLEOCR_BATCH_SIZE)
        .map(|c| c.to_vec())
        .collect();

    let pdf_path = meta.pdf_path.clone();
    let files_for_workers = if used_cache { ctx.file_cache.as_ref() } else { None };

    // Pick the backend up front so workers can share one client +
    // (for OAuth mode) one access-token cache. Surface the
    // "neither mode configured" error as `OcrTokenMissing` —
    // `commands::extract::fine_run` already preflights this so
    // production never hits it; this is the worker-side belt and
    // braces.
    let client = paddleocr::build_client(&ctx.http)?;
    let backend = client.name();
    let client_ref: &(dyn OcrClient + '_) = client.as_ref();

    // Per-chunk results funnelled back over an mpsc channel. We
    // dispatch all chunks up front and rely on the worker pool to
    // serialise actual HTTP load to `PADDLEOCR_PARALLELISM` —
    // `thread::scope` joins everything before we leave.
    let mut fresh: HashMap<u32, String> = HashMap::new();
    let mut failed: Vec<u32> = Vec::new();
    let mut failure_reasons: Vec<String> = Vec::new();

    if !chunks.is_empty() {
        let (tx, rx) = mpsc::channel::<ChunkResult>();
        std::thread::scope(|scope| {
            let chunks_arc: Vec<Vec<u32>> = chunks.clone();
            let mut iter = chunks_arc.into_iter();
            let mut in_flight = 0usize;

            // Prime the pool up to PADDLEOCR_PARALLELISM.
            while in_flight < PADDLEOCR_PARALLELISM {
                let Some(chunk) = iter.next() else { break };
                spawn_worker(
                    scope,
                    &tx,
                    chunk,
                    pdf_path.clone(),
                    client_ref,
                    image_dir.to_path_buf(),
                    page_num_width,
                    files_for_workers,
                    announce_id.map(|s| s.to_owned()),
                    no_cache,
                );
                in_flight += 1;
            }

            // As each chunk finishes we admit one more, keeping
            // total in-flight ≤ PADDLEOCR_PARALLELISM until the
            // pending queue drains.
            while in_flight > 0 {
                let res = rx.recv().expect("worker dropped tx without sending");
                in_flight -= 1;
                match res {
                    ChunkResult::Ok(pages_md) => {
                        for (p, md) in pages_md {
                            fresh.insert(p, md);
                        }
                    }
                    ChunkResult::Failed(chunk_pages, reason) => {
                        failed.extend(chunk_pages);
                        if !failure_reasons.iter().any(|r| r == &reason) {
                            failure_reasons.push(reason);
                        }
                    }
                }
                if let Some(chunk) = iter.next() {
                    spawn_worker(
                        scope,
                        &tx,
                        chunk,
                        pdf_path.clone(),
                        client_ref,
                        image_dir.to_path_buf(),
                        page_num_width,
                        files_for_workers,
                        announce_id.map(|s| s.to_owned()),
                        no_cache,
                    );
                    in_flight += 1;
                }
            }
        });
    }

    failed.sort_unstable();
    failed.dedup();

    let mut merged: Vec<(u32, String)> = pages
        .iter()
        .filter_map(|p| {
            cached
                .get(p)
                .cloned()
                .or_else(|| fresh.get(p).cloned())
                .map(|md| (*p, md))
        })
        .collect();
    merged.sort_by_key(|(p, _)| *p);

    let cached_n = cached.len();
    let api_n = missing.len();
    let batches = chunks.len();
    let eta_secs = (batches as u64).saturating_mul(FINE_SEC_PER_BATCH);

    // Touch `doc` so the borrow checker treats this as live across
    // the call — the editor opened inside split_into_batch is
    // separate, but keeping the reader alive guards against future
    // refactors that want to harvest text via &doc instead of
    // re-opening.
    let _ = doc.page_count()?;
    Ok(FineOutcome {
        pages: merged,
        cached_n,
        api_n,
        batches,
        failed,
        failure_reasons,
        backend,
        eta_secs,
    })
}

enum ChunkResult {
    Ok(Vec<(u32, String)>),
    /// Chunk failed atomically. Carries the failing page numbers
    /// (so the orchestrator can mark them as failed) and the
    /// underlying error message — surfaced to stderr by
    /// `commands::extract::fine_run` so users see the actual
    /// cause instead of an opaque "page failed".
    Failed(Vec<u32>, String),
}

#[allow(clippy::too_many_arguments)]
fn spawn_worker<'scope>(
    scope: &'scope std::thread::Scope<'scope, '_>,
    tx: &mpsc::Sender<ChunkResult>,
    chunk: Vec<u32>,
    pdf_path: PathBuf,
    client: &'scope (dyn OcrClient + 'scope),
    image_dir: PathBuf,
    page_num_width: usize,
    files: Option<&'scope FileCache>,
    announce_id: Option<String>,
    no_cache: bool,
) {
    let tx = tx.clone();
    scope.spawn(move || {
        let result = run_chunk(
            &chunk,
            &pdf_path,
            client,
            &image_dir,
            page_num_width,
            files,
            announce_id.as_deref(),
            no_cache,
        );
        let msg = match result {
            Ok(pages_md) => ChunkResult::Ok(pages_md),
            Err(e) => ChunkResult::Failed(chunk, e.to_string()),
        };
        let _ = tx.send(msg);
    });
}

#[allow(clippy::too_many_arguments)]
fn run_chunk(
    chunk: &[u32],
    pdf_path: &Path,
    client: &dyn OcrClient,
    image_dir: &Path,
    page_num_width: usize,
    files: Option<&FileCache>,
    announce_id: Option<&str>,
    no_cache: bool,
) -> Result<Vec<(u32, String)>, SiftError> {
    // Each worker opens its own PdfDoc — pdf_oxide's reader is
    // not `Sync` across threads, and split_into_batch needs the
    // doc anyway. The cost is one extra parse per chunk, dwarfed
    // by the OCR HTTP round-trip.
    let doc = PdfDoc::open(pdf_path)?;
    let pdf_bytes = doc.split_into_batch(chunk)?;
    let results = client.parse_batch(&pdf_bytes)?;
    if results.len() != chunk.len() {
        return Err(SiftError::Internal(format!(
            "{} backend: expected {} results, got {}",
            client.name(),
            chunk.len(),
            results.len(),
        )));
    }
    let mut out = Vec::with_capacity(chunk.len());
    for (idx, page) in chunk.iter().copied().enumerate() {
        let md = land_ocr_page(
            &results[idx],
            page,
            page_num_width,
            image_dir,
            no_cache,
        )?;
        if let (Some(id), Some(fc)) = (announce_id, files) {
            ocr_write(fc, id, page, page_num_width, &md)?;
        }
        out.push((page, md));
    }
    Ok(out)
}

/// Land one PaddleOCR `PageResult` to disk: write every image to
/// `image_dir/p<NN>-img<MM>.<ext>` (same naming as fast), then
/// rewrite the markdown body so each image reference points at the
/// absolute on-disk path.
fn land_ocr_page(
    result: &PageResult,
    page: u32,
    page_num_width: usize,
    image_dir: &Path,
    no_cache: bool,
) -> Result<String, SiftError> {
    if result.images.is_empty() {
        return Ok(result.markdown.clone());
    }
    std::fs::create_dir_all(image_dir)
        .map_err(|e| SiftError::Io(format!("create image dir {}: {e}", image_dir.display())))?;

    let mut md = result.markdown.clone();
    for (idx, (orig_name, bytes)) in result.images.iter().enumerate() {
        let ext = std::path::Path::new(orig_name)
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| "png".into());
        let name = format!(
            "p{:0>width$}-img{:02}.{}",
            page,
            idx + 1,
            ext,
            width = page_num_width
        );
        let path = image_dir.join(&name);
        let already = matches!(
            std::fs::metadata(&path),
            Ok(m) if m.is_file() && m.len() > 0
        );
        if !already || no_cache {
            atomic_write(&path, bytes)?;
        }
        let abs = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        // Replace the upstream relative reference inline. PaddleOCR
        // emits `![](orig_name)` style refs; a literal `String::replace`
        // catches both `![](orig_name)` and bare `orig_name` mentions.
        md = md.replace(orig_name, &abs.display().to_string());
    }
    Ok(md)
}

/// On-disk key for a cached PaddleOCR result. Lives under
/// `announcements/<id>/fine/p<NN>.md` — `NN` zero-padded to the
/// document's total page-count width (same as the image-naming
/// convention) so a 7-page doc gets `p1.md`/`p7.md`, a 143-page
/// doc gets `p001.md`/`p143.md`.
fn ocr_key(id: &str, page: u32, pad: usize) -> String {
    format!("announcements/{id}/fine/p{page:0pad$}.md", pad = pad)
}

fn ocr_cached(files: &FileCache, id: &str, page: u32, pad: usize) -> Option<String> {
    let bytes = files.read(&ocr_key(id, page, pad))?;
    String::from_utf8(bytes).ok().filter(|s| !s.is_empty())
}

fn ocr_write(
    files: &FileCache,
    id: &str,
    page: u32,
    pad: usize,
    md: &str,
) -> Result<(), SiftError> {
    files.write(&ocr_key(id, page, pad), md.as_bytes())
}

/// Write each image in `images` to `dir / p<NN>-img<MM>.<ext>`,
/// honoring `no_cache` for the "skip if already on disk" behavior.
/// Returns one markdown `![](abs_path)` line per image, in the
/// upstream order. Creates `dir` recursively if missing.
fn land_images(
    dir: &Path,
    page: u32,
    page_num_width: usize,
    images: &[EmbeddedImage],
    no_cache: bool,
) -> Result<Vec<String>, SiftError> {
    if images.is_empty() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| SiftError::Io(format!("create image dir {}: {e}", dir.display())))?;

    let mut lines = Vec::with_capacity(images.len());
    for (idx, img) in images.iter().enumerate() {
        let name = format!(
            "p{:0>width$}-img{:02}.{}",
            page,
            idx + 1,
            img.ext,
            width = page_num_width
        );
        let path = dir.join(&name);
        let already = matches!(
            std::fs::metadata(&path),
            Ok(m) if m.is_file() && m.len() > 0
        );
        if !already || no_cache {
            atomic_write(&path, &img.bytes)?;
        }
        // Markdown wants absolute paths so the file can be moved
        // anywhere and still render. `canonicalize` requires the
        // file to exist (which it does after the write above) but
        // can still fail on exotic filesystems — fall back to a
        // dir.join when it does.
        let abs = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        lines.push(format!("![]({})", abs.display()));
    }
    Ok(lines)
}

/// Sibling `.tmp` + `rename` atomic write. Mirrors `FileCache::write`
/// semantics but operates against an arbitrary path so it works for
/// user-supplied `--image-dir` outside the sift cache root.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SiftError> {
    let parent = path
        .parent()
        .ok_or_else(|| SiftError::Io(format!("no parent dir for {}", path.display())))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| SiftError::Io(format!("mkdir -p {}: {e}", parent.display())))?;
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("part")
    ));
    std::fs::write(&tmp, bytes)
        .map_err(|e| SiftError::Io(format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| SiftError::Io(format!("rename to {}: {e}", path.display())))?;
    Ok(())
}

/// Emit one consolidated `[warn]` block after the fast loop finishes.
/// Pages are coalesced into contiguous ranges so a 20-page scanned
/// stretch shows as `12-31` instead of 20 separate lines. The retry
/// command points at `--mode fine` only — bumping the user to
/// `--mode auto` from inside fast would feel like sift overriding
/// their explicit `--mode fast` choice.
fn emit_scan_summary(target: &Target, scanned: &[u32]) {
    let id_hint = match target {
        Target::AnnouncementId(id) => id.clone(),
        Target::LocalPdf(p) => p.display().to_string(),
    };
    let pages_spec = format_ranges(&coalesce_ranges(scanned));
    let mut err = std::io::stderr().lock();
    // Errors writing to stderr are silently dropped — losing the
    // terminal shouldn't fail an extract whose stdout already
    // landed.
    let _ = writeln!(
        err,
        "[warn] scanned pages detected: {pages_spec} (text layer too sparse for fast mode)"
    );
    let _ = writeln!(
        err,
        "       re-extract with OCR: sift extract {id_hint} --pages {pages_spec} --mode fine"
    );
}

/// Collapse a sorted slice of page numbers into `(start, end)`
/// inclusive ranges. Input must be sorted ascending and deduped —
/// which it is, because `fast` iterates `pages` (a `PageSpec.0`
/// vector, sorted+deduped by [`crate::pdf::pages::PageSpec::parse`])
/// in order and only pushes scanned hits as it sees them.
fn coalesce_ranges(pages: &[u32]) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for &p in pages {
        match out.last_mut() {
            Some(last) if last.1 + 1 == p => last.1 = p,
            _ => out.push((p, p)),
        }
    }
    out
}

/// Render `[(12,15), (18,18), (22,30)]` as `"12-15,18,22-30"` — the
/// same syntax `--pages` accepts, so users can copy-paste the warn
/// line straight into the retry command.
fn format_ranges(ranges: &[(u32, u32)]) -> String {
    ranges
        .iter()
        .map(|&(lo, hi)| {
            if lo == hi {
                lo.to_string()
            } else {
                format!("{lo}-{hi}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Convenience: coalesce + render in one call. The input slice
/// must be sorted ascending and deduped (callers that work off
/// `PageSpec.0` get that for free).
pub fn format_page_list(pages: &[u32]) -> String {
    format_ranges(&coalesce_ranges(pages))
}

fn digits(n: u32) -> usize {
    let mut n = n.max(1);
    let mut d = 0;
    while n > 0 {
        n /= 10;
        d += 1;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn target_parses_pure_digits_as_announcement_id() {
        assert_eq!(
            Target::parse("1219506510").unwrap(),
            Target::AnnouncementId("1219506510".into())
        );
    }

    #[test]
    fn target_with_slash_is_local_pdf_even_without_extension() {
        assert_eq!(
            Target::parse("./1219506510").unwrap(),
            Target::LocalPdf(PathBuf::from("./1219506510"))
        );
    }

    #[test]
    fn target_uppercase_pdf_extension_is_local_pdf() {
        assert_eq!(
            Target::parse("./Foo.PDF").unwrap(),
            Target::LocalPdf(PathBuf::from("./Foo.PDF"))
        );
    }

    #[test]
    fn target_pure_letters_rejected() {
        let err = Target::parse("abc").unwrap_err();
        assert!(matches!(err, SiftError::Parse(_)));
        assert!(err.to_string().contains("cannot interpret"));
    }

    #[test]
    fn validate_pages_rejects_overflow() {
        let err = validate_pages(&[1, 2, 5], 3).unwrap_err();
        assert!(matches!(err, SiftError::Parse(_)));
        let s = err.to_string();
        assert!(s.contains("page 5"), "msg: {s}");
        assert!(s.contains("PDF has 3"), "msg: {s}");
    }

    #[test]
    fn validate_pages_rejects_zero() {
        let err = validate_pages(&[0], 10).unwrap_err();
        assert!(matches!(err, SiftError::Parse(_)));
    }

    #[test]
    fn validate_pages_accepts_in_range() {
        assert!(validate_pages(&[1, 2, 3], 3).is_ok());
    }

    #[test]
    fn resolve_pdf_path_local_missing_returns_io() {
        let ctx = AppContext::default();
        let err = resolve_pdf_path(
            &ctx,
            &Target::LocalPdf(PathBuf::from("/nonexistent/__nope__.pdf")),
        )
        .unwrap_err();
        assert!(matches!(err, SiftError::Io(_)));
        assert!(err.to_string().contains("PDF not found"));
    }

    #[test]
    fn resolve_pdf_path_announce_missing_suggests_download() {
        let ctx = AppContext::default();
        let err =
            resolve_pdf_path(&ctx, &Target::AnnouncementId("9999".into())).unwrap_err();
        assert!(matches!(err, SiftError::Io(_)));
        let s = err.to_string();
        assert!(s.contains("sift announce download"), "msg: {s}");
        assert!(s.contains("9999"), "msg: {s}");
    }

    #[test]
    fn resolve_image_dir_override_takes_precedence() {
        let ctx = AppContext::default();
        let (p, default) = resolve_image_dir(
            &ctx,
            &Target::AnnouncementId("1".into()),
            Some(Path::new("/tmp/x")),
        );
        assert_eq!(p, PathBuf::from("/tmp/x"));
        assert!(!default);
    }

    #[test]
    fn resolve_image_dir_announcement_uses_cache_subdir() {
        use crate::cache::file::FileCache;
        let tmp = TempDir::new().unwrap();
        let ctx = AppContext {
            file_cache: Some(FileCache::open(tmp.path().to_path_buf())),
            ..AppContext::default()
        };
        let (p, default) =
            resolve_image_dir(&ctx, &Target::AnnouncementId("42".into()), None);
        assert!(default);
        assert_eq!(p, tmp.path().join("announcements/42/images"));
    }

    #[test]
    fn resolve_image_dir_local_pdf_uses_cwd_default() {
        let ctx = AppContext::default();
        let (p, default) = resolve_image_dir(
            &ctx,
            &Target::LocalPdf(PathBuf::from("./foo.pdf")),
            None,
        );
        assert!(default);
        assert_eq!(p, PathBuf::from("extracted_images"));
    }

    #[test]
    fn digits_handles_edge_values() {
        assert_eq!(digits(0), 1);
        assert_eq!(digits(1), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(143), 3);
        assert_eq!(digits(1000), 4);
    }

    #[test]
    fn atomic_write_creates_parent_and_writes_bytes() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a/b/c/file.bin");
        atomic_write(&p, b"hello").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
    }

    #[test]
    fn land_images_skips_existing_unless_no_cache() {
        let tmp = TempDir::new().unwrap();
        let img = EmbeddedImage {
            bytes: b"first".to_vec(),
            ext: "png",
            bbox_px: (10, 10),
        };
        // First write — file appears.
        let _ = land_images(tmp.path(), 1, 3, std::slice::from_ref(&img), false).unwrap();
        let p = tmp.path().join("p001-img01.png");
        assert_eq!(std::fs::read(&p).unwrap(), b"first");

        // Same name, different bytes, no_cache=false → preserved.
        let img2 = EmbeddedImage {
            bytes: b"second".to_vec(),
            ..img.clone()
        };
        let _ = land_images(tmp.path(), 1, 3, std::slice::from_ref(&img2), false).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"first");

        // no_cache=true → overwritten.
        let _ = land_images(tmp.path(), 1, 3, std::slice::from_ref(&img2), true).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
    }

    #[test]
    fn ocr_key_pads_to_doc_width() {
        // Padding tracks the doc's total page count: a 7-page doc
        // shouldn't render p<NN> with multi-digit leading zeros.
        assert_eq!(ocr_key("X", 3, 3), "announcements/X/fine/p003.md");
        assert_eq!(ocr_key("X", 3, 2), "announcements/X/fine/p03.md");
        assert_eq!(ocr_key("X", 143, 3), "announcements/X/fine/p143.md");
    }

    #[test]
    fn ocr_cached_round_trips_through_filecache() {
        use crate::cache::file::FileCache;
        let tmp = TempDir::new().unwrap();
        let files = FileCache::open(tmp.path().to_path_buf());
        ocr_write(&files, "ID", 3, 3, "## md body").unwrap();
        assert_eq!(
            ocr_cached(&files, "ID", 3, 3),
            Some("## md body".to_string()),
        );
    }

    #[test]
    fn ocr_cached_returns_none_for_missing_or_empty() {
        use crate::cache::file::FileCache;
        let tmp = TempDir::new().unwrap();
        let files = FileCache::open(tmp.path().to_path_buf());
        // Missing → None
        assert_eq!(ocr_cached(&files, "ID", 1, 3), None);
        // Zero-byte → also None (FileCache::read returns Some(Vec::new())
        // but our helper filters out empty strings to match the
        // FileCache::exists "non-empty file" convention).
        std::fs::create_dir_all(tmp.path().join("announcements/ID/fine")).unwrap();
        std::fs::write(tmp.path().join("announcements/ID/fine/p001.md"), b"").unwrap();
        assert_eq!(ocr_cached(&files, "ID", 1, 3), None);
    }

    #[test]
    fn coalesce_ranges_groups_consecutive_runs() {
        assert_eq!(
            coalesce_ranges(&[12, 13, 14, 15, 18, 22, 23, 24]),
            vec![(12, 15), (18, 18), (22, 24)],
        );
    }

    #[test]
    fn coalesce_ranges_handles_single_and_empty() {
        assert_eq!(coalesce_ranges(&[]), Vec::<(u32, u32)>::new());
        assert_eq!(coalesce_ranges(&[7]), vec![(7, 7)]);
    }

    #[test]
    fn format_ranges_emits_pages_spec_syntax() {
        assert_eq!(
            format_ranges(&[(12, 15), (18, 18), (22, 30)]),
            "12-15,18,22-30",
        );
        assert_eq!(format_ranges(&[(3, 3)]), "3");
        assert_eq!(format_ranges(&[]), "");
    }

    #[test]
    fn land_images_emits_absolute_path_in_markdown() {
        let tmp = TempDir::new().unwrap();
        let img = EmbeddedImage {
            bytes: b"x".to_vec(),
            ext: "png",
            bbox_px: (10, 10),
        };
        let lines =
            land_images(tmp.path(), 2, 3, std::slice::from_ref(&img), false).unwrap();
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(line.starts_with("!["), "got: {line}");
        // Must contain a path with leading `/` (absolute on UNIX).
        assert!(line.contains("](/"), "expected absolute path, got: {line}");
        assert!(line.contains("p002-img01.png"), "got: {line}");
    }
}
