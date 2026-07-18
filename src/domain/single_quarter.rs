//! 累计口径 → 单季口径换算（`report --qmode single`）。
//!
//! A 股利润表 / 现金流量表的季度数是**累计（YTD）**：一季报=1–3 月、
//! 中报=1–6 月累计、三季报=1–9 月累计、年报=全年。做季度趋势 / 环比 /
//! 季节性需要**单季**：
//!
//! | 单季 | = | 需要的累计输入 |
//! | --- | --- | --- |
//! | Q1 | Q1累 | Q1 |
//! | Q2 | H1累 − Q1累 | Q1, H1 |
//! | Q3 | Q3累 − H1累 | H1, Q3 |
//! | Q4 | 年报 − Q3累 | Q3, Annual |
//!
//! [`derive`] 是一个纯函数：吃一批累计 [`FinancialRow`]，按
//! `(symbol.code, item)` 分组、组内按年拿累计四期、产出单季行。只对流量表
//! 调用（`balance` 时点数 / `indicator` 比率不换算，命令层已拦截）。缺中间
//! 累计期（如年报未披露 → Q4）时不产出该单季，并把缺口记进
//! [`Missing`] 供命令层汇总一行 `[warn]`。换算是读后转换，**不入库**。

use std::collections::HashMap;

use time::{Date, Month};

use crate::domain::{FinancialRow, PeriodType};

/// 一个因缺中间累计期而无法换算的单季（按 item + 年 + 季度）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub item: String,
    pub year: i32,
    /// 缺的是哪个单季（2 / 3 / 4；Q1 无前置、不会缺）。
    pub quarter: u8,
}

/// 把累计口径的行换成单季。返回 `(单季行, 缺口列表)`。
///
/// 输入行的 `period_type` 期望落在 `{Q1, H1, Q3, Annual}`（累计四期）；
/// 其它类型（含已是单季的 Q2/Q4）忽略。输出行 `period_type` 为
/// `Q1..Q4`、`period` 为季末日期、`value` 为单季值，其余元数据取被减的
/// 较晚一期。
pub fn derive(rows: Vec<FinancialRow>) -> (Vec<FinancialRow>, Vec<Missing>) {
    // key = (symbol code, item) → year → cumulative-type → row
    type ByType = HashMap<PeriodType, FinancialRow>;
    let mut groups: HashMap<(String, String), HashMap<i32, ByType>> = HashMap::new();
    for r in rows {
        let year = r.period.year();
        groups
            .entry((r.symbol.code.clone(), r.item.clone()))
            .or_default()
            .entry(year)
            .or_default()
            .insert(r.period_type, r);
    }

    let mut out: Vec<FinancialRow> = Vec::new();
    let mut missing: Vec<Missing> = Vec::new();

    // Stable iteration: sort group keys, then years, for deterministic
    // output + warn ordering.
    let mut keys: Vec<&(String, String)> = groups.keys().collect();
    keys.sort();
    for key in keys {
        let by_year = &groups[key];
        let mut years: Vec<&i32> = by_year.keys().collect();
        years.sort();
        for &year in years {
            let cum = &by_year[&year];
            let get = |pt: PeriodType| cum.get(&pt);

            // Q1 单 = Q1 累（无前置，恒可得）。
            if let Some(q1) = get(PeriodType::Q1) {
                out.push(single_row(q1, PeriodType::Q1, q1.value));
            }
            // Q2 = H1 − Q1；Q3 = Q3累 − H1；Q4 = 年报 − Q3累。
            for (end_pt, pred_pt, q_no, out_pt) in [
                (PeriodType::H1, PeriodType::Q1, 2u8, PeriodType::Q2),
                (PeriodType::Q3, PeriodType::H1, 3, PeriodType::Q3),
                (PeriodType::Annual, PeriodType::Q3, 4, PeriodType::Q4),
            ] {
                match (get(end_pt), get(pred_pt)) {
                    (Some(end), Some(pred)) => {
                        out.push(single_row(end, out_pt, end.value - pred.value));
                    }
                    // Have the ending cumulative but not its predecessor
                    // → a genuine gap; flag it. (Neither present → the
                    // quarter simply isn't filed yet, not a gap.)
                    (Some(_), None) => missing.push(Missing {
                        item: key.1.clone(),
                        year,
                        quarter: q_no,
                    }),
                    _ => {}
                }
            }
        }
    }

    (out, missing)
}

/// Clone `base`'s metadata into a single-quarter row with the given
/// type + value. `period` is re-derived to the quarter-end date so a
/// Q2 row ends 06-30 (same as its H1 source) — the render distinguishes
/// it via single mode, not the date.
fn single_row(base: &FinancialRow, pt: PeriodType, value: f64) -> FinancialRow {
    FinancialRow {
        period: quarter_end(base.period.year(), pt),
        period_type: pt,
        value,
        ..base.clone()
    }
}

fn quarter_end(year: i32, pt: PeriodType) -> Date {
    let (m, d) = match pt {
        PeriodType::Q1 => (Month::March, 31),
        PeriodType::Q2 => (Month::June, 30),
        PeriodType::Q3 => (Month::September, 30),
        PeriodType::Q4 => (Month::December, 31),
        // derive only constructs Q1..Q4; other types never reach here.
        PeriodType::H1 => (Month::June, 30),
        PeriodType::Annual => (Month::December, 31),
    };
    Date::from_calendar_date(year, m, d).expect("quarter-end date is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::market::{InstrumentKind, Market};
    use crate::domain::{AuditStatus, Scope, SourceTag, Statement, Symbol, Unit};

    fn row(item: &str, year: i32, pt: PeriodType, value: f64) -> FinancialRow {
        let (m, d) = match pt {
            PeriodType::Q1 => (3u8, 31u8),
            PeriodType::H1 => (6, 30),
            PeriodType::Q3 => (9, 30),
            PeriodType::Annual => (12, 31),
            PeriodType::Q2 => (6, 30),
            PeriodType::Q4 => (12, 31),
        };
        FinancialRow {
            symbol: Symbol {
                code: "600519".into(),
                market: Market::CnA,
                kind: InstrumentKind::Equity,
            },
            name: "贵州茅台".into(),
            period: Date::from_calendar_date(year, Month::try_from(m).unwrap(), d).unwrap(),
            period_type: pt,
            statement: Statement::Income,
            scope: Scope::Consolidated,
            item: item.into(),
            value,
            unit: Unit::Raw,
            currency: "CNY".into(),
            publish_date: None,
            audit: AuditStatus::Unknown,
            source: SourceTag::EastMoney,
        }
    }

    fn find(rows: &[FinancialRow], pt: PeriodType) -> Option<&FinancialRow> {
        rows.iter().find(|r| r.period_type == pt)
    }

    #[test]
    fn q1_single_equals_q1_cumulative() {
        let (out, missing) = derive(vec![row("营收", 2024, PeriodType::Q1, 10.0)]);
        assert!(missing.is_empty());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].period_type, PeriodType::Q1);
        assert_eq!(out[0].value, 10.0);
    }

    #[test]
    fn q2_q3_q4_are_adjacent_cumulative_diffs() {
        let (out, missing) = derive(vec![
            row("营收", 2024, PeriodType::Q1, 10.0),
            row("营收", 2024, PeriodType::H1, 25.0),
            row("营收", 2024, PeriodType::Q3, 45.0),
            row("营收", 2024, PeriodType::Annual, 70.0),
        ]);
        assert!(missing.is_empty(), "missing: {missing:?}");
        assert_eq!(find(&out, PeriodType::Q1).unwrap().value, 10.0);
        assert_eq!(find(&out, PeriodType::Q2).unwrap().value, 15.0); // 25-10
        assert_eq!(find(&out, PeriodType::Q3).unwrap().value, 20.0); // 45-25
        assert_eq!(find(&out, PeriodType::Q4).unwrap().value, 25.0); // 70-45
        // Q2 keeps the 06-30 end date but is typed Q2.
        assert_eq!(find(&out, PeriodType::Q2).unwrap().period.month() as u8, 6);
    }

    #[test]
    fn missing_intermediate_cumulative_flags_only_the_real_gap() {
        // Have Q1 and Q3累 but no H1. Q1 derivable. Q3 single needs H1
        // (its predecessor) and the ending Q3累 IS present → genuine gap
        // → Missing(3). Q2's ending cumulative is H1 itself, which is
        // absent, so Q2 is "not yet available", NOT a gap → not flagged.
        let (out, missing) = derive(vec![
            row("营收", 2024, PeriodType::Q1, 10.0),
            row("营收", 2024, PeriodType::Q3, 45.0),
        ]);
        assert_eq!(find(&out, PeriodType::Q1).unwrap().value, 10.0);
        assert!(find(&out, PeriodType::Q2).is_none());
        assert!(find(&out, PeriodType::Q3).is_none());
        let quarters: Vec<u8> = missing.iter().map(|m| m.quarter).collect();
        assert_eq!(quarters, vec![3], "missing: {missing:?}");
    }

    #[test]
    fn negative_single_quarter_is_passed_through() {
        // Q3累 < H1累 (e.g. reversal) → negative single Q3, not clamped.
        let (out, _) = derive(vec![
            row("净利润", 2024, PeriodType::H1, 30.0),
            row("净利润", 2024, PeriodType::Q3, 20.0),
        ]);
        assert_eq!(find(&out, PeriodType::Q3).unwrap().value, -10.0);
    }

    #[test]
    fn multi_year_does_not_cross_years() {
        let (out, _) = derive(vec![
            row("营收", 2023, PeriodType::Q1, 8.0),
            row("营收", 2023, PeriodType::H1, 18.0),
            row("营收", 2024, PeriodType::Q1, 10.0),
            row("营收", 2024, PeriodType::H1, 25.0),
        ]);
        let q2_23 = out
            .iter()
            .find(|r| r.period.year() == 2023 && r.period_type == PeriodType::Q2)
            .unwrap();
        let q2_24 = out
            .iter()
            .find(|r| r.period.year() == 2024 && r.period_type == PeriodType::Q2)
            .unwrap();
        assert_eq!(q2_23.value, 10.0); // 18-8
        assert_eq!(q2_24.value, 15.0); // 25-10
    }
}
