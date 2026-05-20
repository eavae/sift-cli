//! Data-source layer. Each submodule encapsulates one upstream
//! provider (currently just `cninfo`); cache plumbing for these sources
//! lives in [`crate::cache`].

pub mod cninfo;
