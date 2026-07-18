//! Item-name normalization dictionary plus a process-global collector
//! for items the dictionary did **not** know about.
//!
//! Behaviors:
//!
//! - Hit → return the standardized Chinese name (`entry.cn`).
//! - Miss → return the upstream label verbatim *and* record it via
//!   [`record_unmapped`], so the command layer can print a one-time
//!   hint at process exit listing every item that needs a dictionary
//!   entry.
//!
//! The dictionary is compiled into the binary via `include_str!`;
//! updates ship as new builds. There is no CI coverage assertion —
//! the dictionary is grown incrementally as new upstream labels are
//! observed.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

const ITEMS_TEXT: &str = include_str!("../../data/items.txt");

#[derive(Debug, Clone)]
pub struct ItemEntry {
    /// The standardized Chinese name. The full alias set lives in the
    /// dictionary's `by_alias` map; we don't store it per-entry because
    /// nothing in the codebase iterates it.
    pub cn: String,
}

#[derive(Debug)]
pub struct ItemsDict {
    entries: Vec<ItemEntry>,
    by_alias: HashMap<String, usize>,
}

impl ItemsDict {
    /// Lookup any alias (including the `cn` itself). `O(1)` hashmap.
    pub fn lookup(&self, key: &str) -> Option<&ItemEntry> {
        self.by_alias.get(key).map(|i| &self.entries[*i])
    }

    /// Map an upstream item label to its standardized Chinese name.
    /// Misses pass through verbatim and are recorded via
    /// [`record_unmapped`] for end-of-run reporting.
    pub fn normalize(&self, upstream_item: &str) -> String {
        match self.lookup(upstream_item) {
            Some(e) => e.cn.clone(),
            None => {
                record_unmapped(upstream_item);
                upstream_item.to_string()
            }
        }
    }

}

/// Process-wide dictionary singleton. First call parses the embedded
/// text file; subsequent calls are free. A malformed shipped file
/// panics here — that is a build-time bug, not a runtime condition.
pub fn dict() -> &'static ItemsDict {
    static D: OnceLock<ItemsDict> = OnceLock::new();
    D.get_or_init(|| {
        load_dict(ITEMS_TEXT)
            .expect("data/items.txt failed to load — bad data shipped in the binary")
    })
}

/// Parse + index a dictionary from the pipe-delimited text format.
///
/// One entry per non-empty, non-`#` line:
/// `标准中文名 | alias1 | alias2 | ...`
///
/// The first column becomes `cn` and is also auto-prepended to the
/// alias set (callers don't repeat it). Whitespace around each field
/// is trimmed. Returns `Err` on empty `cn`, empty alias field, or any
/// alias appearing in more than one entry (which would make `lookup`
/// ambiguous). Error messages include the source line number.
fn load_dict(text: &str) -> Result<ItemsDict, String> {
    let mut entries: Vec<ItemEntry> = Vec::new();
    let mut by_alias: HashMap<String, usize> = HashMap::new();

    for (line_idx, raw) in text.lines().enumerate() {
        let line_no = line_idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut parts = line.split('|').map(str::trim);
        let cn = parts.next().unwrap_or("");
        if cn.is_empty() {
            return Err(format!("line {line_no}: empty cn"));
        }

        let mut aliases: Vec<String> = vec![cn.to_string()];
        for p in parts {
            if p.is_empty() {
                return Err(format!("line {line_no}: empty alias field"));
            }
            if !aliases.iter().any(|a| a == p) {
                aliases.push(p.to_string());
            }
        }

        let idx = entries.len();
        for alias in &aliases {
            if let Some(prev) = by_alias.insert(alias.clone(), idx) {
                return Err(format!(
                    "line {line_no}: alias {alias:?} already defined in entry #{prev}"
                ));
            }
        }
        entries.push(ItemEntry {
            cn: cn.to_string(),
        });
    }

    Ok(ItemsDict { entries, by_alias })
}

// --- Unmapped collector --------------------------------------------------

static UNMAPPED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Record an upstream item label that the dictionary did not resolve.
/// Safe to call from any thread.
pub fn record_unmapped(name: &str) {
    let mu = UNMAPPED.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut set) = mu.lock() {
        set.insert(name.to_string());
    }
}

/// Take and clear the unmapped set. Returns labels in lexicographic
/// order so the hint line is stable across runs.
pub fn drain_unmapped() -> Vec<String> {
    let Some(mu) = UNMAPPED.get() else {
        return Vec::new();
    };
    let Ok(mut set) = mu.lock() else {
        return Vec::new();
    };
    let mut v: Vec<String> = set.drain().collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The UNMAPPED set is process-global; tests that touch it must
    /// run sequentially or they will race each other.
    static UNMAPPED_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn shipped_dict_loads_without_dup_aliases() {
        // Triggering `dict()` here exercises the panic path on a real
        // shipped data file: a duplicate alias would panic via expect.
        // A well-known anchor entry confirms the lookup index is wired.
        let d = dict();
        assert!(
            d.lookup("归母净利润").is_some(),
            "shipped dict missing canonical entry"
        );
    }

    #[test]
    fn shipped_dict_resolves_eps_alias() {
        // `EPS` is the ticker-style alias users type most; it must
        // land on the canonical 基本每股收益 entry.
        let d = dict();
        assert_eq!(d.lookup("EPS").map(|e| e.cn.as_str()), Some("基本每股收益"));
    }

    #[test]
    fn lookup_resolves_chinese_english_and_synonyms_to_same_entry() {
        let d = dict();
        let by_cn = d.lookup("归母净利润").expect("cn lookup");
        let by_em = d.lookup("PARENT_NETPROFIT").expect("EM lookup");
        let by_sina_a = d.lookup("归属于母公司股东的净利润").expect("sina-style");
        let by_sina_b = d.lookup("归属于母公司所有者的净利润").expect("alt sina form");
        assert_eq!(by_cn.cn, "归母净利润");
        assert_eq!(by_em.cn, "归母净利润");
        assert_eq!(by_sina_a.cn, "归母净利润");
        assert_eq!(by_sina_b.cn, "归母净利润");
    }

    #[test]
    fn lookup_returns_none_for_unknown_label() {
        let d = dict();
        assert!(d.lookup("THIS_IS_NOT_IN_THE_DICT_001").is_none());
    }

    #[test]
    fn normalize_passes_unknown_through_and_records_it() {
        let _g = UNMAPPED_TEST_LOCK.lock().unwrap();
        drop(drain_unmapped()); // clear residue from any prior test

        let out = dict().normalize("UNMAPPED_LABEL_FOO");
        assert_eq!(out, "UNMAPPED_LABEL_FOO");

        let drained = drain_unmapped();
        assert_eq!(drained, vec!["UNMAPPED_LABEL_FOO"]);
    }

    #[test]
    fn record_unmapped_dedupes_and_sorts_on_drain() {
        let _g = UNMAPPED_TEST_LOCK.lock().unwrap();
        drop(drain_unmapped());

        for _ in 0..3 {
            record_unmapped("X");
        }
        record_unmapped("Y");
        record_unmapped("A");

        let v = drain_unmapped();
        assert_eq!(v, vec!["A".to_string(), "X".to_string(), "Y".to_string()]);

        // Drained → empty next time.
        assert!(drain_unmapped().is_empty());
    }

    #[test]
    fn record_unmapped_is_thread_safe() {
        let _g = UNMAPPED_TEST_LOCK.lock().unwrap();
        drop(drain_unmapped());

        let labels: Vec<String> = (0..50).map(|i| format!("CONCURRENT_{i}")).collect();
        std::thread::scope(|s| {
            for label in &labels {
                s.spawn(move || record_unmapped(label));
            }
        });

        let drained = drain_unmapped();
        assert_eq!(drained.len(), labels.len());
        let drained_set: HashSet<String> = drained.into_iter().collect();
        let expected: HashSet<String> = labels.into_iter().collect();
        assert_eq!(drained_set, expected);
    }

    #[test]
    fn load_dict_skips_comments_and_blank_lines() {
        let text = "\
            # header comment\n\
            \n\
            A | EN_A\n\
              # indented comment\n\
            \n\
            B | EN_B\n\
        ";
        let d = load_dict(text).expect("parse");
        assert_eq!(d.lookup("A").unwrap().cn, "A");
        assert_eq!(d.lookup("EN_B").unwrap().cn, "B");
    }

    #[test]
    fn load_dict_auto_includes_cn_in_aliases() {
        // Both the cn ("X") and the English alias ("EN_X") must
        // resolve to the same entry — i.e. cn is implicitly its own
        // alias even when not repeated in the source line.
        let text = "X | EN_X\n";
        let d = load_dict(text).expect("parse");
        assert_eq!(d.lookup("X").unwrap().cn, "X");
        assert_eq!(d.lookup("EN_X").unwrap().cn, "X");
    }

    #[test]
    fn load_dict_allows_entry_with_only_cn() {
        // No English code yet — still valid; the cn is its sole alias.
        let d = load_dict("孤立项\n").expect("parse");
        assert_eq!(d.lookup("孤立项").unwrap().cn, "孤立项");
    }

    #[test]
    fn load_dict_dedupes_cn_when_repeated_in_aliases() {
        // Tolerate a stray repeat of cn in the aliases column —
        // auto-add shouldn't refuse to load, and the lookups still
        // resolve. Without dedup `load_dict` would Err on duplicate
        // alias.
        let d = load_dict("X | X | EN_X\n").expect("parse");
        assert_eq!(d.lookup("X").unwrap().cn, "X");
        assert_eq!(d.lookup("EN_X").unwrap().cn, "X");
    }

    #[test]
    fn load_dict_rejects_duplicate_alias_across_entries() {
        let bad = "A | SHARED\nB | SHARED\n";
        let err = load_dict(bad).unwrap_err();
        assert!(err.contains("SHARED"), "err: {err}");
        assert!(err.contains("line 2"), "err should cite line 2: {err}");
    }

    #[test]
    fn load_dict_rejects_empty_cn() {
        // Leading `|` → empty first field.
        let err = load_dict(" | EN_X\n").unwrap_err();
        assert!(err.contains("empty cn"), "err: {err}");
    }

    #[test]
    fn load_dict_rejects_empty_alias_field() {
        // Trailing or interior `||` → empty alias slot.
        let err = load_dict("X | | EN_X\n").unwrap_err();
        assert!(err.contains("empty alias"), "err: {err}");
    }

}
