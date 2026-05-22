//! Schema-level checks on the data files shipped under `data/`.
//!
//! Runs `cargo test` independently of any production code path so a
//! malformed data file fails CI even if no caller touches the dict in
//! that test run. The `items.txt` dict is parsed inline here with the
//! same line-oriented rules that `src/domain/items_dict.rs` uses, so
//! the two stay observationally equivalent.

use std::collections::HashSet;

use serde::Deserialize;

const ITEMS_TEXT: &str = include_str!("../data/items.txt");
const DEFAULTS_JSON: &str = include_str!("../data/financials_default_items.json");

struct ItemEntry {
    cn: String,
    aliases: Vec<String>,
}

#[derive(Deserialize)]
struct Defaults {
    income: Vec<String>,
    balance: Vec<String>,
    cashflow: Vec<String>,
    indicator: Vec<String>,
}

fn parse_items(text: &str) -> Vec<ItemEntry> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split('|').map(str::trim);
        let cn = parts.next().unwrap_or("").to_string();
        let mut aliases: Vec<String> = vec![cn.clone()];
        for p in parts {
            if !aliases.iter().any(|a| a == p) {
                aliases.push(p.to_string());
            }
        }
        out.push(ItemEntry { cn, aliases });
    }
    out
}

#[test]
fn items_dict_has_at_least_30_entries() {
    let entries = parse_items(ITEMS_TEXT);
    assert!(
        entries.len() >= 30,
        "expected >= 30 entries, got {}",
        entries.len()
    );
}

#[test]
fn every_entry_has_non_empty_cn_and_aliases_including_cn() {
    let entries = parse_items(ITEMS_TEXT);
    for entry in &entries {
        assert!(!entry.cn.is_empty(), "empty cn in some entry");
        assert!(
            !entry.aliases.is_empty(),
            "empty aliases for {:?}",
            entry.cn
        );
        assert!(
            entry.aliases.contains(&entry.cn),
            "{:?}: aliases must include cn itself",
            entry.cn
        );
    }
}

#[test]
fn aliases_are_globally_unique_across_all_entries() {
    let entries = parse_items(ITEMS_TEXT);
    let mut seen: HashSet<&str> = HashSet::new();
    for entry in &entries {
        for alias in &entry.aliases {
            assert!(
                seen.insert(alias.as_str()),
                "alias {alias:?} appears in more than one entry",
            );
        }
    }
}

#[test]
fn defaults_has_all_four_statements_with_documented_counts() {
    let d: Defaults =
        serde_json::from_str(DEFAULTS_JSON).expect("financials_default_items.json must parse");
    // Counts match the documented "默认数据列" table.
    assert_eq!(d.income.len(), 7, "income default list length");
    assert_eq!(d.balance.len(), 7, "balance default list length");
    assert_eq!(d.cashflow.len(), 5, "cashflow default list length");
    assert_eq!(d.indicator.len(), 9, "indicator default list length");
}
