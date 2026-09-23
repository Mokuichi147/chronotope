//! B. Resolved Temporal Range — 検索用に解決された 4 点境界区間。

use super::expr::{Clock, Recurrence};
use super::{TICKS_PER_DAY, Tick};
use serde::{Deserialize, Serialize};

/// 不確実な区間 `[start, end)`。`start ∈ [earliest_start, latest_start]`、
/// `end ∈ [earliest_end, latest_end]` であることだけが分かっている。
///
/// 例: 「2026年9月頃」 → 9 月を中心に前後へ広げた可能性区間。
/// 例: 「2026-09-20」（時刻不明の出来事） → その日のどこか: `within(day_start, day_end)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FuzzyRange {
    pub earliest_start: Tick,
    pub latest_start: Tick,
    pub earliest_end: Tick,
    pub latest_end: Tick,
}

impl FuzzyRange {
    /// 境界が確定している区間。
    pub fn exact(start: Tick, end: Tick) -> Self {
        FuzzyRange { earliest_start: start, latest_start: start, earliest_end: end, latest_end: end }.normalized()
    }

    /// `[a, b)` のどこかに収まる出来事（開始・終了とも不明）。
    pub fn within(a: Tick, b: Tick) -> Self {
        FuzzyRange { earliest_start: a, latest_start: b, earliest_end: a, latest_end: b }.normalized()
    }

    /// 完全に不明。
    pub fn unknown() -> Self {
        FuzzyRange::within(Tick::NEG_INF, Tick::POS_INF)
    }

    pub fn is_unbounded(&self) -> bool {
        !self.earliest_start.is_finite() && !self.latest_end.is_finite()
    }

    /// 不変条件を満たすように整える（es ≤ ls ≤ le, es ≤ ee ≤ le）。
    pub fn normalized(mut self) -> Self {
        if self.earliest_start > self.latest_start {
            std::mem::swap(&mut self.earliest_start, &mut self.latest_start);
        }
        if self.earliest_end > self.latest_end {
            std::mem::swap(&mut self.earliest_end, &mut self.latest_end);
        }
        if self.latest_start > self.latest_end {
            self.latest_start = self.latest_end;
        }
        if self.earliest_end < self.earliest_start {
            self.earliest_end = self.earliest_start;
        }
        self
    }

    /// 取り得る最大の範囲 `[es, le)`。インデックスのキーになる。
    pub fn possible_span(&self) -> (Tick, Tick) {
        (self.earliest_start, self.latest_end)
    }

    /// 確実に含まれる範囲 `[ls, ee)`（存在する場合）。
    pub fn certain_span(&self) -> Option<(Tick, Tick)> {
        (self.latest_start < self.earliest_end).then_some((self.latest_start, self.earliest_end))
    }

    /// `[qs, qe)` と重なる可能性がある。
    pub fn possibly_overlaps(&self, qs: Tick, qe: Tick) -> bool {
        self.earliest_start < qe && self.latest_end > qs
    }

    /// どのような解釈でも `[qs, qe)` と重なる。
    pub fn certainly_overlaps(&self, qs: Tick, qe: Tick) -> bool {
        self.latest_start < qe && self.earliest_end > qs
    }

    /// どのような解釈でも `[qs, qe)` に収まる。
    pub fn certainly_within(&self, qs: Tick, qe: Tick) -> bool {
        self.earliest_start >= qs && self.latest_end <= qe
    }

    /// 全解釈で確実に `t` より前に終わる。
    pub fn certainly_before(&self, t: Tick) -> bool {
        self.latest_end <= t
    }

    pub fn hull(&self, o: &FuzzyRange) -> FuzzyRange {
        FuzzyRange {
            earliest_start: self.earliest_start.min(o.earliest_start),
            latest_start: self.latest_start.max(o.latest_start),
            earliest_end: self.earliest_end.min(o.earliest_end),
            latest_end: self.latest_end.max(o.latest_end),
        }
        .normalized()
    }

    /// 全境界を `delta` だけずらす（無限大は保持）。
    pub fn shift(&self, min_delta: i64, max_delta: i64) -> FuzzyRange {
        FuzzyRange {
            earliest_start: self.earliest_start.offset(min_delta),
            latest_start: self.latest_start.offset(max_delta),
            earliest_end: self.earliest_end.offset(min_delta),
            latest_end: self.latest_end.offset(max_delta),
        }
        .normalized()
    }

    /// 両側に `fuzz` だけ曖昧さを広げる（「頃」）。
    pub fn widen(&self, fuzz: i64) -> FuzzyRange {
        FuzzyRange {
            earliest_start: self.earliest_start.offset(-fuzz),
            latest_start: self.latest_start.offset(fuzz),
            earliest_end: self.earliest_end.offset(-fuzz),
            latest_end: self.latest_end.offset(fuzz),
        }
        .normalized()
    }

    /// 代表点（可能区間の中央。無限の場合は有限側）。類似度計算や並べ替えに使う。
    pub fn midpoint(&self) -> Option<Tick> {
        let (a, b) = self.possible_span();
        match (a.is_finite(), b.is_finite()) {
            (true, true) => Some(Tick(((a.0 as i128 + b.0 as i128) / 2) as i64)),
            (true, false) => Some(a),
            (false, true) => Some(b),
            (false, false) => None,
        }
    }

    /// 可能区間の幅（無限なら None）。
    pub fn width(&self) -> Option<i64> {
        let (a, b) = self.possible_span();
        (a.is_finite() && b.is_finite()).then(|| b.0.saturating_sub(a.0))
    }
}

/// 解決結果の精度。表示と specificity の算出に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    Millisecond,
    Second,
    Minute,
    Hour,
    PartOfDay,
    Day,
    Week,
    Month,
    Season,
    Year,
    Decade,
    Century,
    Unbounded,
}

impl Granularity {
    /// 0..1 の具体性スコア（信頼度の specificity 成分に使う）。
    pub fn specificity(self) -> f32 {
        match self {
            Granularity::Millisecond | Granularity::Second | Granularity::Minute => 1.0,
            Granularity::Hour => 0.95,
            Granularity::PartOfDay => 0.9,
            Granularity::Day => 0.85,
            Granularity::Week => 0.7,
            Granularity::Month => 0.6,
            Granularity::Season => 0.5,
            Granularity::Year => 0.4,
            Granularity::Decade => 0.25,
            Granularity::Century => 0.15,
            Granularity::Unbounded => 0.0,
        }
    }

    pub fn from_width(width: Option<i64>) -> Granularity {
        match width {
            None => Granularity::Unbounded,
            Some(w) if w <= super::TICKS_PER_SECOND => Granularity::Second,
            Some(w) if w <= super::TICKS_PER_MINUTE => Granularity::Minute,
            Some(w) if w <= super::TICKS_PER_HOUR => Granularity::Hour,
            Some(w) if w <= 6 * super::TICKS_PER_HOUR => Granularity::PartOfDay,
            Some(w) if w <= TICKS_PER_DAY => Granularity::Day,
            Some(w) if w <= 7 * TICKS_PER_DAY => Granularity::Week,
            Some(w) if w <= 31 * TICKS_PER_DAY => Granularity::Month,
            Some(w) if w <= 92 * TICKS_PER_DAY => Granularity::Season,
            Some(w) if w <= 366 * TICKS_PER_DAY => Granularity::Year,
            Some(w) if w <= 3_653 * TICKS_PER_DAY => Granularity::Decade,
            Some(_) => Granularity::Century,
        }
    }
}

/// 繰り返し出来事の解決済み表現（毎週金曜日25:30 など）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecurrenceSpec {
    pub rule: Recurrence,
    pub clock: Option<Clock>,
    /// 1 回あたりの長さ（tick）。
    pub occurrence_len: i64,
    pub utc_offset_minutes: i32,
}

/// Projection に載せる解決済み時間。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedTemporal {
    pub range: FuzzyRange,
    /// 時間軸。異なる軸同士は比較不能（comparable: false）。
    pub axis: String,
    /// 解決に使ったカレンダー。
    pub calendar_frame: String,
    pub granularity: Granularity,
    /// 繰り返しの場合、`range` は有効期間の包絡、個々の発生は `recurrence` で判定する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<RecurrenceSpec>,
}

impl ResolvedTemporal {
    pub fn comparable_with(&self, other: &ResolvedTemporal) -> bool {
        self.axis == other.axis
    }

    /// クエリ窓との重なり判定。繰り返しは個々の発生で判定する。
    pub fn matches_window(&self, qs: Tick, qe: Tick, certainly: bool) -> bool {
        if let Some(rec) = &self.recurrence {
            if !self.range.possibly_overlaps(qs, qe) {
                return false;
            }
            let lo = qs.max(self.range.earliest_start);
            let hi = qe.min(self.range.latest_end);
            return rec.any_occurrence_in(lo, hi);
        }
        if certainly { self.range.certainly_overlaps(qs, qe) } else { self.range.possibly_overlaps(qs, qe) }
    }
}

impl RecurrenceSpec {
    /// `[lo, hi)` 内に 1 回でも発生するか（グレゴリオ暦ベース）。
    pub fn any_occurrence_in(&self, lo: Tick, hi: Tick) -> bool {
        self.occurrences(lo, hi, 1).next().is_some()
    }

    /// `[lo, hi)` と重なる発生を列挙する（最大 `limit` 件）。
    pub fn occurrences(&self, lo: Tick, hi: Tick, limit: usize) -> impl Iterator<Item = FuzzyRange> + '_ {
        let offset = self.utc_offset_minutes as i64 * super::TICKS_PER_MINUTE;
        // 窓が無限の場合は発生の列挙範囲を現実的な長さに制限する。
        let lo_f = if lo.is_finite() { lo.0 } else { hi.0.saturating_sub(400 * 366 * TICKS_PER_DAY) };
        let hi_f = if hi.is_finite() { hi.0 } else { lo_f.saturating_add(400 * 366 * TICKS_PER_DAY) };
        // 25:30 のように翌日へはみ出す分と発生長だけ手前から探す。
        let start_day = (lo_f + offset).div_euclid(TICKS_PER_DAY) - 2 - self.occurrence_len / TICKS_PER_DAY;
        let end_day = (hi_f + offset).div_euclid(TICKS_PER_DAY) + 1;
        let clock_start = self.clock.as_ref().map(|c| c.offset_and_len().0).unwrap_or(0);
        let occ_len = self.occurrence_len.max(1);
        (start_day..=end_day)
            .filter(move |&day| self.rule.matches_day(day))
            .map(move |day| {
                let s = day * TICKS_PER_DAY + clock_start - offset;
                FuzzyRange::exact(Tick(s), Tick(s + occ_len))
            })
            .filter(move |r| r.possibly_overlaps(Tick(lo_f), Tick(hi_f)))
            .take(limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_point_semantics() {
        let day = FuzzyRange::within(Tick(0), Tick(TICKS_PER_DAY));
        assert!(day.possibly_overlaps(Tick(10), Tick(20)));
        assert!(!day.certainly_overlaps(Tick(10), Tick(20)));
        assert!(day.certainly_overlaps(Tick(-5), Tick(TICKS_PER_DAY + 5)));
        let ex = FuzzyRange::exact(Tick(0), Tick(100));
        assert!(ex.certainly_overlaps(Tick(10), Tick(20)));
        assert_eq!(ex.certain_span(), Some((Tick(0), Tick(100))));
        assert!(FuzzyRange::unknown().possibly_overlaps(Tick(0), Tick(1)));
    }
}
