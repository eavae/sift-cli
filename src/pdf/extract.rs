//! Thin wrapper over the `pdf_oxide` crate exposing only the entry
//! points `sift extract` needs. Keeps upstream API shape (1-based vs 0-based
//! indices, `Result<usize>` vs `usize`, `ConversionOptions`, the
//! `ImageData` / `ColorSpace` enums, …) confined to this module so a
//! future swap of the PDF backend is local.
//!
//! Index convention: every method on [`PdfDoc`] takes **1-based**
//! page numbers — that matches `--pages "1-5"` user input and the
//! README's "Page N" wording. We translate to `pdf_oxide`'s 0-based
//! `page_index` at the boundary.

use std::path::{Path, PathBuf};

use pdf_oxide::converters::ConversionOptions;
use pdf_oxide::editor::DocumentEditor;
use pdf_oxide::extractors::images::{ImageData, PdfImage};
use pdf_oxide::PdfDocument;

use crate::error::SiftError;

/// One embedded image extracted from a single PDF page. We resolve
/// pdf_oxide's `PdfImage` into ready-to-write bytes here so the
/// orchestrator can decide where to put them without touching the
/// upstream API.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedImage {
    /// Ready-to-write file body. JPEGs are passed through; raw /
    /// CMYK / palette data is rendered to PNG by pdf_oxide.
    pub bytes: Vec<u8>,
    /// File extension to use on disk (no leading dot). Matches the
    /// chosen encoding in `bytes`.
    pub ext: &'static str,
    /// Image bbox on the PDF page, in PDF user-space units (width ×
    /// height of the placement rect on the page). This is the area
    /// that scan-detect measures coverage against — **not** the
    /// image's intrinsic pixel dimensions. `(0, 0)` when pdf_oxide
    /// couldn't recover a placement rect (older PDFs).
    pub bbox_px: (u32, u32),
}

/// Owned, opened PDF document. Cheap clone is **not** provided — a
/// single sift extract command opens one doc and reuses it. All
/// methods take `&self` so the caller can iterate pages without
/// re-opening.
pub struct PdfDoc {
    inner: PdfDocument,
    options: ConversionOptions,
    /// Source path on disk — needed for [`PdfDoc::split_into_batch`]
    /// which round-trips through `pdf_oxide::editor::DocumentEditor`
    /// (page-subset extraction lives on the editor, not the reader).
    source: PathBuf,
}

impl PdfDoc {
    /// Open a PDF from disk. Maps upstream errors to
    /// `SiftError::Parse` — pdf_oxide raises both IO errors and
    /// parser errors through the same type, and at this layer all
    /// we can tell the user is "this file isn't a PDF we can read".
    /// The orchestrator pre-checks file existence to produce a
    /// nicer `SiftError::Io("PDF not found: ...")` first.
    pub fn open(path: &Path) -> Result<Self, SiftError> {
        let inner = PdfDocument::open(path).map_err(|e| {
            SiftError::Parse(format!("failed to parse PDF {}: {e}", path.display()))
        })?;
        Ok(Self {
            inner,
            options: ConversionOptions::default(),
            source: path.to_path_buf(),
        })
    }

    /// Number of pages in the document. `0` is technically valid
    /// per the upstream API but never observed for the PDFs F4
    /// targets; treated as "empty doc" by callers.
    pub fn page_count(&self) -> Result<u32, SiftError> {
        let n = self.inner.page_count().map_err(map_oxide_err)?;
        u32::try_from(n).map_err(|_| {
            SiftError::Parse(format!("PDF page count overflows u32: {n}"))
        })
    }

    /// Render page `n` (1-based) to Markdown. Image references
    /// remain as the upstream library emits them — relative file
    /// names; the orchestrator rewrites them to absolute paths
    /// after image extraction.
    pub fn page_markdown(&self, n: u32) -> Result<String, SiftError> {
        self.inner
            .to_markdown(self.zero_based(n)?, &self.options)
            .map_err(map_oxide_err)
    }

    /// Embedded images on page `n` (1-based), already encoded for
    /// disk. JPEGs are pass-through; raw / CMYK images are
    /// re-encoded as PNG by pdf_oxide via [`PdfImage::to_png_bytes`].
    pub fn page_images(&self, n: u32) -> Result<Vec<EmbeddedImage>, SiftError> {
        let raws = self
            .inner
            .extract_images(self.zero_based(n)?)
            .map_err(map_oxide_err)?;
        let mut out = Vec::with_capacity(raws.len());
        for img in raws {
            out.push(encode_image(&img)?);
        }
        Ok(out)
    }

    /// Page area in PDF user-space pixels (`width × height`).
    /// Used by [`crate::pdf::scan_detect::classify`] as the
    /// denominator for both text density and image coverage. A
    /// degenerate `0 × H` or `W × 0` page falls through as `0` —
    /// scan_detect handles that by short-circuiting density.
    pub fn page_area_px(&self, n: u32) -> Result<u64, SiftError> {
        let (x0, y0, x1, y1) = self
            .inner
            .get_page_media_box(self.zero_based(n)?)
            .map_err(map_oxide_err)?;
        let w = (x1 - x0).abs() as u64;
        let h = (y1 - y0).abs() as u64;
        Ok(w * h)
    }

    /// Average characters per page across the whole document.
    /// Drives the `[info] text_layer yes/no (avg N chars/page)`
    /// row. `0.0` when the doc is empty or every page failed
    /// extraction.
    pub fn text_layer_avg(&self) -> Result<f32, SiftError> {
        let count = self.page_count()?;
        if count == 0 {
            return Ok(0.0);
        }
        let mut total: usize = 0;
        for n in 1..=count {
            // Best-effort — a single broken page should not make
            // the whole metric `Err`. pdf_oxide's
            // `extract_text` happily returns `Ok("")` for pages
            // without a text layer, so `?` would never trigger
            // for scanned pages anyway; we still guard.
            let txt = self
                .inner
                .extract_text(self.zero_based(n)?)
                .unwrap_or_default();
            total = total.saturating_add(txt.chars().count());
        }
        Ok(total as f32 / count as f32)
    }

    /// Build a fresh PDF that contains exactly `pages` (1-based,
    /// non-contiguous allowed, in the order given). Returns the
    /// serialized bytes ready to base64-encode and ship to the
    /// PaddleOCR endpoint.
    ///
    /// Story-03 uses this to repackage e.g. `[3, 7, 12]` into a
    /// 3-page PDF so a single OCR API call covers all three
    /// without retransmitting the rest of the document.
    pub fn split_into_batch(&self, pages: &[u32]) -> Result<Vec<u8>, SiftError> {
        if pages.is_empty() {
            return Err(SiftError::Parse(
                "split_into_batch: pages must not be empty".into(),
            ));
        }
        let count = self.page_count()?;
        let mut zero_based = Vec::with_capacity(pages.len());
        for &p in pages {
            if p == 0 || p > count {
                return Err(SiftError::Parse(format!(
                    "split_into_batch: page {p} out of range (PDF has {count} pages)"
                )));
            }
            zero_based.push((p - 1) as usize);
        }
        // pdf_oxide's editor is a separate handle that can mutate
        // a doc; the reader (`PdfDocument`) we opened in [`open`]
        // is read-only. Round-tripping through the editor keeps
        // the surface area small — we don't need an editable doc
        // hanging around for the other extraction operations.
        let mut editor = DocumentEditor::open(&self.source).map_err(map_oxide_err)?;
        editor
            .extract_pages_to_bytes(&zero_based)
            .map_err(map_oxide_err)
    }

    fn zero_based(&self, n: u32) -> Result<usize, SiftError> {
        if n == 0 {
            return Err(SiftError::Parse(
                "PDF pages are 1-based; got page 0".into(),
            ));
        }
        Ok((n - 1) as usize)
    }
}

fn encode_image(img: &PdfImage) -> Result<EmbeddedImage, SiftError> {
    let (bytes, ext) = match img.data() {
        // RGB / grayscale JPEGs pass through unchanged. CMYK JPEGs
        // would otherwise display wrong in browsers, so for those
        // (and for any raw / palette image) we ask pdf_oxide to
        // render PNG bytes.
        ImageData::Jpeg(data) if img.color_space().components() != 4 => {
            (data.clone(), "jpg")
        }
        _ => {
            let png = img.to_png_bytes().map_err(map_oxide_err)?;
            (png, "png")
        }
    };
    // Prefer the on-page bbox (placement rect, in user space) over
    // the image's intrinsic resolution — scan_detect measures how
    // much of the *page* the image occupies, not how detailed the
    // raster is. Three layers of fallback:
    //
    // 1. `bbox()` — populated by pdf_oxide when the PDF carries an
    //    explicit placement rect. Production PDFs usually do.
    // 2. CTM matrix — for PDFs written by `pdf_oxide::writer` the
    //    placement is encoded as a `cm` operator (scale + translate)
    //    that lands in `matrix()` instead of `bbox()`. The
    //    diagonal entries `[0]` / `[3]` are width / height for
    //    axis-aligned placement (no rotation), which is what
    //    every sift fixture uses.
    // 3. Intrinsic raster dimensions — last-ditch; scan_detect on
    //    these will essentially never trigger (1×1 thumbnail
    //    against page area is meaningless coverage).
    let bbox_px = img
        .bbox()
        .map(|r| (r.width.abs() as u32, r.height.abs() as u32))
        .filter(|&(w, h)| w > 0 && h > 0)
        .or_else(|| {
            let m = img.matrix();
            let (w, h) = (m[0].abs() as u32, m[3].abs() as u32);
            (w > 0 && h > 0).then_some((w, h))
        })
        .unwrap_or((img.width(), img.height()));
    Ok(EmbeddedImage {
        bytes,
        ext,
        bbox_px,
    })
}

fn map_oxide_err(e: pdf_oxide::error::Error) -> SiftError {
    SiftError::Parse(format!("pdf_oxide: {e}"))
}

