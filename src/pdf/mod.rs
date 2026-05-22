//! PDF-related helpers for `sift extract` (F4).
//!
//! Story-01 lands only the [`pages`] submodule (page-spec parsing,
//! pure-string in / pure-data out). Stories 02 / 03 will add
//! `extract` (pdf-oxide wrapper) and `scan_detect` siblings here.

pub mod extract;
pub mod pages;
pub mod scan_detect;
