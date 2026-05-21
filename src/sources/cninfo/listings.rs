//! F1 stock-listing schema: `szse_stock.json` / `hke_stock.json` →
//! [`CnInfoRow`] / [`StockLists`] / [`parse_envelope`]. Field semantics
//! pinned in the F1 README "数据源与协议".

use serde::{Deserialize, Serialize};

use crate::error::SiftError;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CnInfoRow {
    pub code: String,
    pub zwjc: String,
    pub pinyin: String,
    pub category: String,
    #[serde(rename = "orgId")]
    pub org_id: String,
}

#[derive(Debug)]
pub struct StockLists {
    /// Rows from `szse_stock.json` — covers SH / SZ / BJ / B-share / CDR
    /// despite the file name suggesting SZSE only.
    pub cn_a: Vec<CnInfoRow>,
    /// Rows from `hke_stock.json`.
    pub hk: Vec<CnInfoRow>,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "stockList")]
    stock_list: Vec<CnInfoRow>,
}

/// Decode a cninfo response body into its `stockList` array. `label`
/// gets embedded in the error message so a schema break can be traced
/// back to the originating endpoint without inspecting bytes.
pub(crate) fn parse_envelope(bytes: &[u8], label: &str) -> Result<Vec<CnInfoRow>, SiftError> {
    let env: Envelope = serde_json::from_slice(bytes).map_err(|e| {
        SiftError::Internal(format!("cninfo {label}: stockList missing ({e})"))
    })?;
    Ok(env.stock_list)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"stockList":[{"code":"600519","zwjc":"贵州茅台","pinyin":"gzmt","category":"A股","orgId":"gssh0600519"}]}"#;

    #[test]
    fn parse_envelope_extracts_rows() {
        let rows = parse_envelope(SAMPLE.as_bytes(), "szse").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].code, "600519");
        assert_eq!(rows[0].zwjc, "贵州茅台");
        assert_eq!(rows[0].pinyin, "gzmt");
        assert_eq!(rows[0].category, "A股");
        assert_eq!(rows[0].org_id, "gssh0600519");
    }

    #[test]
    fn parse_envelope_missing_field_is_internal() {
        let err = parse_envelope(br#"{"foo":1}"#, "szse").unwrap_err();
        match err {
            SiftError::Internal(m) => {
                assert!(m.contains("cninfo szse"), "msg: {m}");
                assert!(m.contains("stockList"), "msg: {m}");
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn parse_envelope_empty_array_is_ok() {
        let rows = parse_envelope(br#"{"stockList":[]}"#, "hke").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn category_passes_through_unchanged() {
        let body =
            r#"{"stockList":[{"code":"900901","zwjc":"x","pinyin":"x","category":"B股","orgId":"x"}]}"#;
        let rows = parse_envelope(body.as_bytes(), "szse").unwrap();
        assert_eq!(rows[0].category, "B股");
    }
}
