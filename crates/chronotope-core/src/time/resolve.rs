//! TemporalExpression → ResolvedTemporal の解決。
//!
//! 解決器自体は非再帰で、他の出来事への参照（Anchor）は呼び出し側が渡す `lookup` で解決する。
//! 深さ上限・循環検出・無効化キューは Engine 側（Resolver / Materializer）の責務。

use super::Tick;
use super::calendar::CalendarFrame;
use super::expr::*;
use super::range::{FuzzyRange, Granularity, RecurrenceSpec, ResolvedTemporal};
use crate::ResourceId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedKind {
    /// 「不明」と明示されている。
    Unknown,
    /// 原文を解析できなかった。
    Unparsed,
    /// 相対表現だが参照時刻（source_time / acquired_at）が無い。
    NeedsReference,
    AnchorNotFound,
    AmbiguousAnchor,
    /// 参照先の時間自体が未解決。
    AnchorUnresolved,
    Cycle,
    DepthExceeded,
    /// 時間軸が異なる（別世界の暦など）。
    Incomparable,
    Unsupported,
    /// 制約が矛盾している（after > before など）。
    Contradiction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unresolved {
    pub kind: UnresolvedKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anchors: Vec<Anchor>,
}

impl Unresolved {
    pub fn new(kind: UnresolvedKind, message: impl Into<String>) -> Self {
        Unresolved { kind, message: message.into(), anchors: vec![] }
    }
    fn with_anchor(mut self, a: &Anchor) -> Self {
        self.anchors.push(a.clone());
        self
    }
}

/// Anchor の解決結果。
#[derive(Debug, Clone)]
pub enum AnchorResult {
    Found(ResolvedTemporal),
    NotFound,
    Ambiguous(Vec<ResourceId>),
    Unresolved(Unresolved),
}

pub struct ResolveContext<'a> {
    /// 相対表現の基準時刻（情報源の source_time。無ければ acquired_at）。
    pub reference: Option<Tick>,
    pub calendar: &'a CalendarFrame,
    pub lookup: &'a dyn Fn(&Anchor) -> AnchorResult,
}

pub fn resolve(expr: &TemporalExpression, ctx: &ResolveContext) -> Result<ResolvedTemporal, Unresolved> {
    let r = Resolver { ctx };
    let (range, recurrence) = r.ast(&expr.ast)?;
    // 粒度はその暦の「1 日」を基準に測る（ゲーム内暦では 1 日が 20 分のこともある）。
    let tpd = ctx.calendar.ticks_per_day().unwrap_or(super::TICKS_PER_DAY).max(1);
    let scale = |w: i64| (w as i128 * super::TICKS_PER_DAY as i128 / tpd as i128) as i64;
    let granularity = match &recurrence {
        Some(rec) => Granularity::from_width(Some(scale(rec.occurrence_len))),
        None => Granularity::from_width(range.width().map(scale)),
    };
    Ok(ResolvedTemporal { range, axis: ctx.calendar.axis.clone(), calendar_frame: ctx.calendar.key.clone(), granularity, recurrence })
}

struct Resolver<'a, 'b> {
    ctx: &'b ResolveContext<'a>,
}

type Out = Result<(FuzzyRange, Option<RecurrenceSpec>), Unresolved>;

impl Resolver<'_, '_> {
    fn cal(&self) -> &CalendarFrame {
        self.ctx.calendar
    }

    fn reference(&self) -> Result<Tick, Unresolved> {
        self.ctx
            .reference
            .ok_or_else(|| Unresolved::new(UnresolvedKind::NeedsReference, "relative expression requires a reference time (source_time or acquired_at)"))
    }

    fn unsupported(&self, what: &str) -> Unresolved {
        Unresolved::new(UnresolvedKind::Unsupported, format!("{what} is not supported by calendar `{}`", self.cal().key))
    }

    fn ref_day(&self) -> Result<i64, Unresolved> {
        let r = self.reference()?;
        self.cal().day_of(r).ok_or_else(|| self.unsupported("day arithmetic"))
    }

    fn day_window(&self, first: i64, last: i64, clock: Option<&Clock>) -> Result<FuzzyRange, Unresolved> {
        let cal = self.cal();
        let (s, e) = match clock {
            Some(c) => {
                let (s, _) = cal.clock_bounds(first, c).ok_or_else(|| self.unsupported("clock"))?;
                let (_, e) = cal.clock_bounds(last, c).ok_or_else(|| self.unsupported("clock"))?;
                (s, e)
            }
            None => (cal.day_start(first).ok_or_else(|| self.unsupported("day"))?, cal.day_start(last + 1).ok_or_else(|| self.unsupported("day"))?),
        };
        Ok(FuzzyRange::within(s, e))
    }

    fn anchor(&self, a: &Anchor) -> Result<ResolvedTemporal, Unresolved> {
        match (self.ctx.lookup)(a) {
            AnchorResult::Found(t) => {
                if t.axis != self.cal().axis {
                    return Err(Unresolved::new(
                        UnresolvedKind::Incomparable,
                        format!("anchor is on time axis `{}`, expression uses `{}`", t.axis, self.cal().axis),
                    )
                    .with_anchor(a));
                }
                Ok(t)
            }
            AnchorResult::NotFound => Err(Unresolved::new(UnresolvedKind::AnchorNotFound, "anchor not found").with_anchor(a)),
            AnchorResult::Ambiguous(ids) => {
                Err(Unresolved::new(UnresolvedKind::AmbiguousAnchor, format!("anchor matches {} resources", ids.len())).with_anchor(a))
            }
            AnchorResult::Unresolved(u) => Err(Unresolved { anchors: vec![a.clone()], ..u }),
        }
    }

    fn ast(&self, ast: &TimeAst) -> Out {
        let cal = self.cal();
        match ast {
            TimeAst::Unknown => Err(Unresolved::new(UnresolvedKind::Unknown, "time explicitly unknown")),
            TimeAst::Unparsed => Err(Unresolved::new(UnresolvedKind::Unparsed, "raw text could not be parsed")),
            TimeAst::Date { date } => {
                let reference = if date.year.is_none() { Some(self.reference()?) } else { self.ctx.reference };
                let (s, e) = cal.date_bounds(date, reference).map_err(|e| Unresolved::new(UnresolvedKind::Unsupported, e.to_string()))?;
                Ok((FuzzyRange::within(s, e), None))
            }
            TimeAst::Approx { inner } => {
                let (r, rec) = self.ast(inner)?;
                // 「頃」は精度と同じ幅だけ前後に広げる（2026年9月頃 → 8月〜10月）。
                let fuzz = r.width().unwrap_or(0);
                Ok((r.widen(fuzz), rec))
            }
            TimeAst::Interval { start, end } => {
                let s = match start.as_ref() {
                    TimeAst::Unknown => FuzzyRange::unknown(),
                    a => self.ast(a)?.0,
                };
                let e = match end.as_ref() {
                    TimeAst::Unknown => FuzzyRange::unknown(),
                    a => self.ast(a)?.0,
                };
                if s.earliest_start.is_finite() && e.latest_end.is_finite() && s.earliest_start >= e.latest_end {
                    return Err(Unresolved::new(UnresolvedKind::Contradiction, "interval start is after its end"));
                }
                // 開始は開始項のどこか、終了は終了項のどこか。
                Ok((
                    FuzzyRange { earliest_start: s.earliest_start, latest_start: s.latest_end, earliest_end: e.earliest_start, latest_end: e.latest_end }
                        .normalized(),
                    None,
                ))
            }
            TimeAst::Relative { anchor: Anchor::Reference, offset, unit, clock } => {
                let r = self.reference()?;
                match unit {
                    Unit::Day | Unit::Week => {
                        let k = if *unit == Unit::Week { 7 } else { 1 };
                        let d = self.ref_day()?;
                        Ok((self.day_window(d + offset.min * k, d + offset.max * k, clock.as_ref())?, None))
                    }
                    Unit::Month | Unit::Year => {
                        let (y, m, _) = cal.civil_of(r).ok_or_else(|| self.unsupported("month arithmetic"))?;
                        let bounds = |delta: i64| -> Option<(Tick, Tick)> {
                            if *unit == Unit::Year {
                                cal.year_bounds(y + delta)
                            } else {
                                let (yy, mm) = cal.shift_month(y, m, delta)?;
                                cal.month_bounds(yy, mm)
                            }
                        };
                        let (s, _) = bounds(offset.min).ok_or_else(|| self.unsupported("month arithmetic"))?;
                        let (_, e) = bounds(offset.max).ok_or_else(|| self.unsupported("month arithmetic"))?;
                        Ok((FuzzyRange::within(s, e), None))
                    }
                    _ => {
                        let u = self.scaled_unit(*unit)?;
                        Ok((FuzzyRange::within(r.offset(offset.min * u), r.offset(offset.max * u + u)), None))
                    }
                }
            }
            TimeAst::Relative { anchor, offset, unit, .. } => {
                let t = self.anchor(anchor)?;
                let u = self.scaled_unit(*unit)?;
                Ok((t.range.shift(offset.min * u, offset.max * u), None))
            }
            TimeAst::Weekday { week_offset, weekday, clock } => {
                let d = self.ref_day()?;
                let wd = cal.weekday(d).ok_or_else(|| self.unsupported("weekday"))? as i64;
                let wlen = cal.days_per_week().ok_or_else(|| self.unsupported("weekday"))?;
                match (week_offset, weekday) {
                    (Some(o), Some(w)) => {
                        let day = d - wd + *o as i64 * wlen + *w as i64;
                        Ok((self.day_window(day, day, clock.as_ref())?, None))
                    }
                    (Some(o), None) => {
                        let first = d - wd + *o as i64 * wlen;
                        Ok((self.day_window(first, first + wlen - 1, None)?, None))
                    }
                    (None, Some(w)) => {
                        // 参照日以前で直近のその曜日（参照日当日を含む）。
                        let back = (wd - *w as i64).rem_euclid(wlen);
                        let day = d - back;
                        Ok((self.day_window(day, day, clock.as_ref())?, None))
                    }
                    (None, None) => Err(Unresolved::new(UnresolvedKind::Unsupported, "empty weekday expression")),
                }
            }
            TimeAst::Recurring { rule, clock } => {
                if !matches!(cal.kind, super::calendar::CalendarKind::Gregorian { .. }) {
                    return Err(self.unsupported("recurrence"));
                }
                let occurrence_len = clock.as_ref().map(|c| c.offset_and_len().1).unwrap_or(super::TICKS_PER_DAY);
                Ok((FuzzyRange::unknown(), Some(RecurrenceSpec { rule: *rule, clock: *clock, occurrence_len, utc_offset_minutes: cal.utc_offset_minutes() })))
            }
            TimeAst::Between { after, before } => {
                let mut lo = Tick::NEG_INF;
                let mut hi = Tick::POS_INF;
                for a in after {
                    // X が A より後 ⇒ X.start ≥ A.end ≥ A.earliest_end
                    lo = lo.max(self.anchor(a)?.range.earliest_end);
                }
                for b in before {
                    // X が B より前 ⇒ X.end ≤ B.start ≤ B.latest_start
                    hi = hi.min(self.anchor(b)?.range.latest_start);
                }
                if lo >= hi {
                    return Err(Unresolved::new(UnresolvedKind::Contradiction, "`after` anchors end later than `before` anchors start"));
                }
                Ok((FuzzyRange::within(lo, hi), None))
            }
        }
    }

    /// 暦の 1 日の長さに合わせた単位長。
    fn scaled_unit(&self, unit: Unit) -> Result<i64, Unresolved> {
        let tpd = self.cal().ticks_per_day().ok_or_else(|| self.unsupported("duration arithmetic"))?;
        Ok((unit.approx_ticks() as i128 * tpd as i128 / super::TICKS_PER_DAY as i128) as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::TICKS_PER_DAY;

    fn none(_: &Anchor) -> AnchorResult {
        AnchorResult::NotFound
    }

    fn res(raw: &str, reference: Option<Tick>, cal: &CalendarFrame, lookup: &dyn Fn(&Anchor) -> AnchorResult) -> Result<ResolvedTemporal, Unresolved> {
        let e = TemporalExpression::strict(raw, &cal.key).unwrap();
        resolve(&e, &ResolveContext { reference, calendar: cal, lookup })
    }

    #[test]
    fn absolute_and_approx() {
        let cal = CalendarFrame::gregorian_with_offset(540);
        let r = res("2026-09-20 15:30", None, &cal, &none).unwrap();
        assert_eq!(r.range.earliest_start, Tick::from_civil(2026, 9, 20, 15, 30, 0, 540));
        assert_eq!(r.granularity, Granularity::Minute);
        let m = res("2026年9月", None, &cal, &none).unwrap();
        let approx = res("2026年9月頃", None, &cal, &none).unwrap();
        assert!(approx.range.earliest_start < m.range.earliest_start);
        assert!(approx.range.latest_end > m.range.latest_end);
    }

    #[test]
    fn late_night_clock_rolls_over() {
        let cal = CalendarFrame::gregorian();
        let r = res("2026-09-18 25:30", None, &cal, &none).unwrap();
        assert_eq!(r.range.earliest_start, Tick::from_civil(2026, 9, 19, 1, 30, 0, 0));
    }

    #[test]
    fn relative_to_reference() {
        let cal = CalendarFrame::gregorian();
        // 2026-09-23 (水) 12:00
        let reference = Some(Tick::from_civil(2026, 9, 23, 12, 0, 0, 0));
        let r = res("先週火曜日", reference, &cal, &none).unwrap();
        assert_eq!(r.range.earliest_start, Tick::from_civil(2026, 9, 15, 0, 0, 0, 0));
        let r = res("月曜日の夕方", reference, &cal, &none).unwrap();
        assert_eq!(r.range.earliest_start, Tick::from_civil(2026, 9, 21, 16, 0, 0, 0));
        let r = res("数日前", reference, &cal, &none).unwrap();
        assert_eq!(r.range.earliest_start, Tick::from_civil(2026, 9, 18, 0, 0, 0, 0));
        assert_eq!(r.range.latest_end, Tick::from_civil(2026, 9, 22, 0, 0, 0, 0));
        assert_eq!(res("数日前", None, &cal, &none).unwrap_err().kind, UnresolvedKind::NeedsReference);
    }

    #[test]
    fn relative_to_event_and_between() {
        let cal = CalendarFrame::gregorian();
        let a = ResolvedTemporal {
            range: FuzzyRange::within(Tick(10 * TICKS_PER_DAY), Tick(11 * TICKS_PER_DAY)),
            axis: "earth".into(),
            calendar_frame: "gregorian".into(),
            granularity: Granularity::Day,
            recurrence: None,
        };
        let b = ResolvedTemporal { range: FuzzyRange::within(Tick(20 * TICKS_PER_DAY), Tick(21 * TICKS_PER_DAY)), ..a.clone() };
        let lookup = |x: &Anchor| match x {
            Anchor::Named(n) if n == "A事件" || n == "A" => AnchorResult::Found(a.clone()),
            Anchor::Named(n) if n == "B" => AnchorResult::Found(b.clone()),
            _ => AnchorResult::NotFound,
        };
        let r = res("A事件の3日前", None, &cal, &lookup).unwrap();
        assert_eq!(r.range.earliest_start, Tick(7 * TICKS_PER_DAY));
        let r = res("Aより後、Bより前", None, &cal, &lookup).unwrap();
        assert_eq!(r.range.possible_span(), (Tick(10 * TICKS_PER_DAY), Tick(21 * TICKS_PER_DAY)));
        let r = res("Bより後、Aより前", None, &cal, &lookup).unwrap_err();
        assert_eq!(r.kind, UnresolvedKind::Contradiction);
    }

    #[test]
    fn recurrence() {
        let cal = CalendarFrame::gregorian_with_offset(540);
        let r = res("毎週金曜日25:30", None, &cal, &none).unwrap();
        // 2026-09-18 は金曜。25:30 JST = 09-19 01:30 JST = 09-18 16:30 UTC
        let occ = Tick::from_civil(2026, 9, 18, 16, 30, 0, 0);
        assert!(r.matches_window(occ, occ.offset(1), false));
        assert!(!r.matches_window(occ.offset(-3_600_000), occ.offset(-3_000_000), false));
        assert!(r.matches_window(Tick::from_civil(2026, 9, 1, 0, 0, 0, 0), Tick::from_civil(2026, 9, 30, 0, 0, 0, 0), false));
    }
}
