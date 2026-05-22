//! Classify a single PDF page as "text-type" vs "scanned image" by
//! looking at extracted text density and the largest image bbox
//! coverage. Pure-function so callers (fast / future auto) can feed
//! it their already-extracted text / image / area data without
//! re-invoking pdf-oxide.
//!
//! Thresholds: `text_chars / page_area_px² < 1e-3` **and** the largest
//! image bbox covers > 80% of page area. Both knobs are `const` — not
//! exposed as flags.
//!
//! It's a heuristic, not a physical invariant.

use crate::pdf::extract::EmbeddedImage;

/// Per-page scan-detection thresholds (constants — not user-tunable
/// per README §"扫描页处理").
pub const SCAN_TEXT_DENSITY: f64 = 1e-3;
pub const SCAN_IMG_COVERAGE: f64 = 0.80;

/// Output of [`classify`]. Carries the raw stats alongside the verdict
/// so the caller can emit them in the stderr `[warn]` line ("提取到
/// X 字符 / 整页大图 Y%") without re-counting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScanVerdict {
    pub is_scanned: bool,
    pub text_chars: usize,
    /// Largest image bbox coverage as a fraction of page area.
    /// `0.0` when there are no images or `area_px == 0`.
    pub max_img_coverage: f64,
}

/// Classify a page. Pure: no I/O, no pdf-oxide calls. Returns
/// `is_scanned = true` only when **both** thresholds trip — text
/// density below `1e-3` and the largest embedded image bbox covers
/// > 80% of page area.
pub fn classify(text: &str, images: &[EmbeddedImage], area_px: u64) -> ScanVerdict {
    let text_chars = text.chars().count();
    let density = if area_px == 0 {
        f64::INFINITY
    } else {
        text_chars as f64 / (area_px as f64).powi(2)
    };
    let max_img_coverage = if area_px == 0 {
        0.0
    } else {
        images
            .iter()
            .map(|img| {
                let (w, h) = img.bbox_px;
                (u64::from(w) * u64::from(h)) as f64 / area_px as f64
            })
            .fold(0.0_f64, f64::max)
    };
    let is_scanned =
        density < SCAN_TEXT_DENSITY && max_img_coverage > SCAN_IMG_COVERAGE;
    ScanVerdict {
        is_scanned,
        text_chars,
        max_img_coverage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: u32, h: u32) -> EmbeddedImage {
        EmbeddedImage {
            bytes: Vec::new(),
            ext: "png",
            bbox_px: (w, h),
        }
    }

    /// A normal text-type page: many characters, no full-page
    /// image — should not be classified as scanned.
    #[test]
    fn text_heavy_page_is_not_scanned() {
        let v = classify(&"x".repeat(2000), &[img(100, 100)], 1000);
        assert!(!v.is_scanned);
        assert_eq!(v.text_chars, 2000);
    }

    /// A scanned page: near-zero text, one image covering ~100% of
    /// the page — both thresholds trip.
    #[test]
    fn scan_type_page_is_scanned() {
        let v = classify("", &[img(1000, 1000)], 1_000_000);
        assert!(v.is_scanned);
        assert_eq!(v.text_chars, 0);
        assert!(v.max_img_coverage >= 0.99, "got: {}", v.max_img_coverage);
    }

    /// Mixed: half a page of image plus modest text. Image coverage
    /// is 50% (< 80%), so even with sparse text the page is not
    /// flagged scanned.
    #[test]
    fn mixed_page_with_half_size_image_is_not_scanned() {
        let v = classify("a", &[img(100, 500)], 1_000_000);
        assert!(!v.is_scanned);
        assert!(v.max_img_coverage < 0.8, "got: {}", v.max_img_coverage);
    }
}
