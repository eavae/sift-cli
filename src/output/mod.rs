pub mod financial_render;
pub mod render;
pub mod tabular;

pub use render::{render, RenderRow};

/// Internal output format. `Table` is the default but is **not** a clap
/// user-visible value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Table,
    Tsv,
    Ndjson,
}

impl Format {
    /// Map the user-supplied `--format` value onto the internal enum;
    /// absence resolves to `Table`.
    pub fn from_user(opt: Option<crate::cli::UserFormat>) -> Self {
        match opt {
            None => Format::Table,
            Some(crate::cli::UserFormat::Tsv) => Format::Tsv,
            Some(crate::cli::UserFormat::Json) => Format::Ndjson,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::UserFormat;

    #[test]
    fn from_user_none_is_table() {
        assert_eq!(Format::from_user(None), Format::Table);
    }

    #[test]
    fn from_user_tsv_is_tsv() {
        assert_eq!(Format::from_user(Some(UserFormat::Tsv)), Format::Tsv);
    }

    #[test]
    fn from_user_json_is_ndjson() {
        assert_eq!(Format::from_user(Some(UserFormat::Json)), Format::Ndjson);
    }
}
