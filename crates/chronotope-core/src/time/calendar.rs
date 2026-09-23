//! Calendar Frame — 時間側の参照系。空間側の SpatialReferenceFrame と対称な構造。
//!
//! 各カレンダーは 1 つの時間軸（axis）へ写像される。同じ軸に写像されるカレンダー同士は比較可能、
//! 異なる軸（別の架空世界など）や `Opaque`（数値写像を持たない暦）は比較不能として扱う。

use super::expr::{Clock, DateSpec, MonthPart};
use super::{CivilDateTime, TICKS_PER_DAY, TICKS_PER_HOUR, TICKS_PER_MINUTE, Tick, days_from_civil, weekday_from_days};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};

pub const EARTH_AXIS: &str = "earth";
pub const GREGORIAN: &str = "gregorian";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarFrame {
    pub key: String,
    pub name: String,
    /// 写像先の時間軸。
    pub axis: String,
    pub kind: CalendarKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CalendarKind {
    /// 先発グレゴリオ暦（固定 UTC オフセット）。
    Gregorian { utc_offset_minutes: i32 },
    /// 等長の日・月・年からなる暦（ゲーム内暦・架空暦）。
    /// `epoch` は `first_year` 年 1 月 1 日 0 時に対応する軸上の tick。
    Uniform {
        epoch: Tick,
        ticks_per_day: i64,
        days_per_month: u32,
        months_per_year: u32,
        #[serde(default)]
        days_per_week: Option<u32>,
        #[serde(default = "one")]
        first_year: i64,
    },
    /// 数値写像を持たない暦（順序関係のみで扱う）。
    Opaque,
}

fn one() -> i64 {
    1
}

impl CalendarFrame {
    pub fn gregorian() -> Self {
        CalendarFrame {
            key: GREGORIAN.into(),
            name: "Gregorian (UTC)".into(),
            axis: EARTH_AXIS.into(),
            kind: CalendarKind::Gregorian { utc_offset_minutes: 0 },
        }
    }

    pub fn gregorian_with_offset(minutes: i32) -> Self {
        if minutes == 0 {
            return Self::gregorian();
        }
        let sign = if minutes < 0 { '-' } else { '+' };
        let m = minutes.abs();
        let key = format!("{GREGORIAN}{sign}{:02}:{:02}", m / 60, m % 60);
        CalendarFrame {
            name: format!("Gregorian (UTC{sign}{:02}:{:02})", m / 60, m % 60),
            key,
            axis: EARTH_AXIS.into(),
            kind: CalendarKind::Gregorian { utc_offset_minutes: minutes },
        }
    }

    /// `gregorian`, `gregorian+09:00` のような組み込みキーを解釈する。
    pub fn builtin(key: &str) -> Option<Self> {
        if key == GREGORIAN || key == "gregorian+00:00" {
            return Some(Self::gregorian());
        }
        let rest = key.strip_prefix(GREGORIAN)?;
        let sign = match rest.chars().next()? {
            '+' => 1,
            '-' => -1,
            _ => return None,
        };
        let (h, m) = rest[1..].split_once(':')?;
        let minutes = sign * (h.parse::<i32>().ok()? * 60 + m.parse::<i32>().ok()?);
        Some(Self::gregorian_with_offset(minutes))
    }

    pub fn is_opaque(&self) -> bool {
        matches!(self.kind, CalendarKind::Opaque)
    }

    pub fn ticks_per_day(&self) -> Option<i64> {
        match &self.kind {
            CalendarKind::Gregorian { .. } => Some(TICKS_PER_DAY),
            CalendarKind::Uniform { ticks_per_day, .. } => Some(*ticks_per_day),
            CalendarKind::Opaque => None,
        }
    }

    pub fn utc_offset_minutes(&self) -> i32 {
        match &self.kind {
            CalendarKind::Gregorian { utc_offset_minutes } => *utc_offset_minutes,
            _ => 0,
        }
    }

    fn origin(&self) -> i64 {
        match &self.kind {
            CalendarKind::Gregorian { utc_offset_minutes } => -(*utc_offset_minutes as i64) * TICKS_PER_MINUTE,
            CalendarKind::Uniform { epoch, .. } => epoch.0,
            CalendarKind::Opaque => 0,
        }
    }

    /// 軸上の tick → この暦の通し日番号。
    pub fn day_of(&self, t: Tick) -> Option<i64> {
        let tpd = self.ticks_per_day()?;
        Some((t.0 as i128 - self.origin() as i128).div_euclid(tpd as i128) as i64)
    }

    pub fn day_start(&self, day: i64) -> Option<Tick> {
        let tpd = self.ticks_per_day()?;
        let v = day as i128 * tpd as i128 + self.origin() as i128;
        Some(Tick(v.clamp(i64::MIN as i128 + 1, i64::MAX as i128 - 1) as i64))
    }

    /// 曜日（0 = 月曜）。週を持たない暦では None。
    pub fn weekday(&self, day: i64) -> Option<u8> {
        match &self.kind {
            CalendarKind::Gregorian { .. } => Some(weekday_from_days(day)),
            CalendarKind::Uniform { days_per_week: Some(w), .. } => Some(day.rem_euclid(*w as i64) as u8),
            _ => None,
        }
    }

    pub fn days_per_week(&self) -> Option<i64> {
        match &self.kind {
            CalendarKind::Gregorian { .. } => Some(7),
            CalendarKind::Uniform { days_per_week, .. } => days_per_week.map(i64::from),
            CalendarKind::Opaque => None,
        }
    }

    /// 通し日番号 → (年, 月, 日)。
    pub fn civil_of_day(&self, day: i64) -> Option<(i64, u32, u32)> {
        match &self.kind {
            CalendarKind::Gregorian { .. } => Some(super::civil_from_days(day)),
            CalendarKind::Uniform { days_per_month, months_per_year, first_year, .. } => {
                let dpy = *days_per_month as i64 * *months_per_year as i64;
                let y = day.div_euclid(dpy);
                let rem = day.rem_euclid(dpy);
                Some((y + first_year, (rem / *days_per_month as i64) as u32 + 1, (rem % *days_per_month as i64) as u32 + 1))
            }
            CalendarKind::Opaque => None,
        }
    }

    pub fn civil_of(&self, t: Tick) -> Option<(i64, u32, u32)> {
        self.civil_of_day(self.day_of(t)?)
    }

    fn months_per_year(&self) -> Option<u32> {
        match &self.kind {
            CalendarKind::Gregorian { .. } => Some(12),
            CalendarKind::Uniform { months_per_year, .. } => Some(*months_per_year),
            CalendarKind::Opaque => None,
        }
    }

    fn days_in_month(&self, y: i64, m: u32) -> Option<u32> {
        match &self.kind {
            CalendarKind::Gregorian { .. } => Some(super::days_in_month(y, m)),
            CalendarKind::Uniform { days_per_month, .. } => Some(*days_per_month),
            CalendarKind::Opaque => None,
        }
    }

    /// (年, 月, 日) → 通し日番号。
    pub fn day_from_civil(&self, y: i64, m: u32, d: u32) -> Option<i64> {
        match &self.kind {
            CalendarKind::Gregorian { .. } => Some(days_from_civil(y, m, d)),
            CalendarKind::Uniform { days_per_month, months_per_year, first_year, .. } => {
                let dpm = *days_per_month as i64;
                Some(((y - first_year) * *months_per_year as i64 + (m as i64 - 1)) * dpm + (d as i64 - 1))
            }
            CalendarKind::Opaque => None,
        }
    }

    /// 月を `delta` だけ進めた (年, 月)。
    pub fn shift_month(&self, y: i64, m: u32, delta: i64) -> Option<(i64, u32)> {
        let mpy = self.months_per_year()? as i64;
        let idx = y * mpy + (m as i64 - 1) + delta;
        Some((idx.div_euclid(mpy), (idx.rem_euclid(mpy) + 1) as u32))
    }

    /// 月全体の `[start, end)`。
    pub fn month_bounds(&self, y: i64, m: u32) -> Option<(Tick, Tick)> {
        let first = self.day_from_civil(y, m, 1)?;
        let n = self.days_in_month(y, m)? as i64;
        Some((self.day_start(first)?, self.day_start(first + n)?))
    }

    pub fn year_bounds(&self, y: i64) -> Option<(Tick, Tick)> {
        let a = self.day_from_civil(y, 1, 1)?;
        let b = self.day_from_civil(y + 1, 1, 1)?;
        Some((self.day_start(a)?, self.day_start(b)?))
    }

    /// 時刻表現を日へ適用した `[start, end)`。暦の 1 日の長さに比例させる。
    pub fn clock_bounds(&self, day: i64, clock: &Clock) -> Option<(Tick, Tick)> {
        let tpd = self.ticks_per_day()?;
        let (o, len) = clock.offset_and_len();
        let scale = |v: i64| (v as i128 * tpd as i128 / TICKS_PER_DAY as i128) as i64;
        let start = self.day_start(day)?.offset(scale(o));
        Some((start, start.offset(scale(len).max(1))))
    }

    /// 日付仕様の `[start, end)`。年や月の欠落は `reference` で補う。
    pub fn date_bounds(&self, d: &DateSpec, reference: Option<Tick>) -> Result<(Tick, Tick)> {
        if self.is_opaque() {
            return Err(Error::Incomparable(format!("calendar `{}` has no numeric mapping", self.key)));
        }
        let mut d = *d;
        if d.year.is_none() {
            let r = reference.ok_or_else(|| Error::Invalid("year missing and no reference time".into()))?;
            let (y, m, _) = self.civil_of(r).ok_or_else(|| Error::invalid("reference outside calendar"))?;
            d.year = Some(y);
            if d.month.is_none() && d.day.is_some() {
                d.month = Some(m);
            }
        }
        if let (CalendarKind::Gregorian { utc_offset_minutes }, _) = (&self.kind, ()) {
            return d.bounds_gregorian(*utc_offset_minutes);
        }
        let y = d.year.unwrap_or(1);
        let bad = || Error::invalid(format!("date outside calendar `{}`", self.key));
        match (d.month, d.day) {
            (None, _) => self.year_bounds(y).ok_or_else(bad),
            (Some(m), None) => {
                let (s, e) = self.month_bounds(y, m).ok_or_else(bad)?;
                let first = self.day_from_civil(y, m, 1).ok_or_else(bad)?;
                let n = self.days_in_month(y, m).ok_or_else(bad)? as i64;
                let part = |a: i64, b: i64| Ok((self.day_start(first + a).ok_or_else(bad)?, self.day_start(first + b).ok_or_else(bad)?));
                match d.month_part {
                    None => Ok((s, e)),
                    Some(MonthPart::Early) => part(0, n / 3),
                    Some(MonthPart::Mid) => part(n / 3, 2 * n / 3),
                    Some(MonthPart::Late) => part(2 * n / 3, n),
                }
            }
            (Some(m), Some(dd)) => {
                let day = self.day_from_civil(y, m, dd).ok_or_else(bad)?;
                match &d.clock {
                    Some(c) => self.clock_bounds(day, c).ok_or_else(bad),
                    None => Ok((self.day_start(day).ok_or_else(bad)?, self.day_start(day + 1).ok_or_else(bad)?)),
                }
            }
        }
    }

    /// 表示用の日付文字列。
    pub fn format(&self, t: Tick) -> String {
        if !t.is_finite() {
            return t.to_iso();
        }
        match &self.kind {
            CalendarKind::Gregorian { utc_offset_minutes } => {
                let c = CivilDateTime::from_tick(t, *utc_offset_minutes);
                if *utc_offset_minutes == 0 {
                    c.to_iso()
                } else {
                    let s = c.to_iso();
                    let m = utc_offset_minutes.abs();
                    let sign = if *utc_offset_minutes < 0 { '-' } else { '+' };
                    format!("{}{sign}{:02}:{:02}", s.trim_end_matches('Z'), m / 60, m % 60)
                }
            }
            CalendarKind::Uniform { .. } => match (self.day_of(t), self.ticks_per_day()) {
                (Some(day), Some(tpd)) => {
                    let (y, m, d) = self.civil_of_day(day).unwrap_or((0, 0, 0));
                    let rem = t.0 - self.day_start(day).map(|x| x.0).unwrap_or(0);
                    let hour = rem as i128 * 24 / tpd as i128;
                    format!("{}:{y}-{m:02}-{d:02}T{hour:02}h", self.key)
                }
                _ => format!("{}:{}", self.key, t.0),
            },
            CalendarKind::Opaque => format!("{}:?", self.key),
        }
    }
}

/// 表示精度を 1 時間単位に丸めるためのヘルパ（主に要約表示用）。
pub fn hours_between(a: Tick, b: Tick) -> Option<i64> {
    (a.is_finite() && b.is_finite()).then(|| (b.0 - a.0) / TICKS_PER_HOUR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_keys() {
        let c = CalendarFrame::builtin("gregorian+09:00").unwrap();
        assert_eq!(c.utc_offset_minutes(), 540);
        assert_eq!(c.key, "gregorian+09:00");
        assert!(CalendarFrame::builtin("shire").is_none());
    }

    #[test]
    fn uniform_calendar() {
        // 1 日 = 20 分、28 日 × 4 か月のゲーム内暦。
        let c = CalendarFrame {
            key: "valley".into(),
            name: "Valley".into(),
            axis: "valley-world".into(),
            kind: CalendarKind::Uniform {
                epoch: Tick(0),
                ticks_per_day: 20 * TICKS_PER_MINUTE,
                days_per_month: 28,
                months_per_year: 4,
                days_per_week: Some(7),
                first_year: 1,
            },
        };
        let d = DateSpec { year: Some(2), month: Some(1), day: Some(1), ..Default::default() };
        let (s, e) = c.date_bounds(&d, None).unwrap();
        assert_eq!(s.0, 112 * 20 * TICKS_PER_MINUTE);
        assert_eq!(e.0 - s.0, 20 * TICKS_PER_MINUTE);
        assert_eq!(c.civil_of(s), Some((2, 1, 1)));
    }
}
