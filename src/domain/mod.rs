//! Domain types shared across data sources and commands.
//!
//! `Symbol` parses user-supplied stock identifiers; `Market` / `Board`
//! plus `infer_board` / `em_secid_prefix` give the per-source layers a
//! single place to look up market metadata so each data-source story
//! does not need to re-decide the mappings.

#![allow(dead_code)]

pub mod market;
pub mod symbol;

#[allow(unused_imports)] // consumed by Story 03+; re-exported for ergonomics now.
pub use market::{em_secid_prefix, infer_board, Board, Market};
#[allow(unused_imports)]
pub use symbol::Symbol;
