//! 時間モデル。
//!
//! 仕様上の 3 層を分離している:
//! - A. [`expr::TemporalExpression`]: 原表現（raw_text / AST / calendar_frame）を不変保存する。
//! - B. [`range::ResolvedTemporal`]: 4 点境界の検索用 Projection（int64 UTA tick）。
//! - C. [`order::TemporalOrderGraph`]: 絶対時刻にアンカーできない Event の部分順序ラベル（アクセラレータ）。
//!
//! 区間関係は [`allen`] の 13 関係ビットマスクで表す。

pub mod allen;
pub mod calendar;
pub mod expr;
pub mod order;
pub mod parse;
pub mod range;
pub mod resolve;

use serde::{Deserialize, Serialize};
use std::fmt;

/// UTA (Universal Temporal Axis) tick。1 tick = 1 ミリ秒、エポックは 1970-01-01T00:00:00Z。
/// i64 で約 ±2.9 億年を表現できるため、歴史・地質年代・架空世界の長大な年表も扱える。
/// `i64::MIN` / `i64::MAX` は -∞ / +∞ の番兵として扱う。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tick(pub i64);

pub const TICKS_PER_SECOND: i64 = 1_000;
pub const TICKS_PER_MINUTE: i64 = 60 * TICKS_PER_SECOND;
pub const TICKS_PER_HOUR: i64 = 60 * TICKS_PER_MINUTE;
pub const TICKS_PER_DAY: i64 = 24 * TICKS_PER_HOUR;

impl Tick {
    pub const NEG_INF: Tick = Tick(i64::MIN);
    pub const POS_INF: Tick = Tick(i64::MAX);

    pub fn is_finite(self) -> bool {
        self != Tick::NEG_INF && self != Tick::POS_INF
    }

    /// 無限大を保ったまま加算する。
    pub fn offset(self, delta: i64) -> Tick {
        if !self.is_finite() {
            return self;
        }
        let v = self.0.saturating_add(delta);
        // 飽和で番兵と衝突しないよう 1 つ内側に寄せる。
        Tick(v.clamp(i64::MIN + 1, i64::MAX - 1))
    }

    pub fn from_unix_millis(ms: i64) -> Tick {
        Tick(ms)
    }

    pub fn from_civil(y: i64, m: u32, d: u32, h: u32, mi: u32, s: u32, utc_offset_minutes: i32) -> Tick {
        let days = days_from_civil(y, m, d);
        let local = days as i128 * TICKS_PER_DAY as i128
            + h as i128 * TICKS_PER_HOUR as i128
            + mi as i128 * TICKS_PER_MINUTE as i128
            + s as i128 * TICKS_PER_SECOND as i128
            - utc_offset_minutes as i128 * TICKS_PER_MINUTE as i128;
        Tick(local.clamp(i64::MIN as i128 + 1, i64::MAX as i128 - 1) as i64)
    }

    /// 現在時刻（システム時計）。
    pub fn now() -> Tick {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        Tick(d.as_millis() as i64)
    }

    /// ISO 8601 風の文字列（UTC）。負の年は天文学的年号（0 = 紀元前 1 年）。
    pub fn to_iso(self) -> String {
        if self == Tick::NEG_INF {
            return "-inf".into();
        }
        if self == Tick::POS_INF {
            return "+inf".into();
        }
        let c = CivilDateTime::from_tick(self, 0);
        c.to_iso()
    }

    /// ISO 8601 文字列（`2026-09-20`, `2026-09-20T15:30:00Z`, `2026-09-20T15:30+09:00` など）から変換。
    /// 精度が日以下の場合はその区間の開始時刻を返す。
    pub fn parse_iso(s: &str) -> crate::Result<Tick> {
        match s.trim() {
            "-inf" => return Ok(Tick::NEG_INF),
            "+inf" | "inf" => return Ok(Tick::POS_INF),
            _ => {}
        }
        let ast = parse::parse_expression(s)?;
        match ast {
            expr::TimeAst::Date { date: d } if d.year.is_some() => {
                let (start, _) = d.bounds_gregorian(0)?;
                Ok(start)
            }
            _ => Err(crate::Error::Parse(format!("not an absolute ISO date/time: `{s}`"))),
        }
    }
}

impl fmt::Debug for Tick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tick({})", self.to_iso())
    }
}

/// 先発グレゴリオ暦の日付 → 1970-01-01 からの日数（Howard Hinnant のアルゴリズム）。
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = m as i64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 1970-01-01 からの日数 → (年, 月, 日)。
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 曜日（0 = 月曜 … 6 = 日曜）。
pub fn weekday_from_days(z: i64) -> u8 {
    // 1970-01-01 は木曜日。
    (z + 3).rem_euclid(7) as u8
}

pub fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(y) => 29,
        2 => 28,
        _ => 0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilDateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub millis: u32,
}

impl CivilDateTime {
    pub fn from_tick(t: Tick, utc_offset_minutes: i32) -> Self {
        let local = t.0 as i128 + utc_offset_minutes as i128 * TICKS_PER_MINUTE as i128;
        let days = local.div_euclid(TICKS_PER_DAY as i128) as i64;
        let rem = local.rem_euclid(TICKS_PER_DAY as i128) as i64;
        let (year, month, day) = civil_from_days(days);
        CivilDateTime {
            year,
            month,
            day,
            hour: (rem / TICKS_PER_HOUR) as u32,
            minute: ((rem % TICKS_PER_HOUR) / TICKS_PER_MINUTE) as u32,
            second: ((rem % TICKS_PER_MINUTE) / TICKS_PER_SECOND) as u32,
            millis: (rem % TICKS_PER_SECOND) as u32,
        }
    }

    pub fn to_iso(&self) -> String {
        let y = if (0..=9999).contains(&self.year) { format!("{:04}", self.year) } else { format!("{:+07}", self.year) };
        let mut s = format!("{y}-{:02}-{:02}T{:02}:{:02}:{:02}", self.month, self.day, self.hour, self.minute, self.second);
        if self.millis != 0 {
            s.push_str(&format!(".{:03}", self.millis));
        }
        s.push('Z');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip() {
        for z in [-800_000i64, -1, 0, 1, 19_000, 20_716, 3_000_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        // 2026-09-20 は日曜日。
        assert_eq!(weekday_from_days(days_from_civil(2026, 9, 20)), 6);
    }

    #[test]
    fn tick_iso() {
        let t = Tick::from_civil(2026, 9, 20, 15, 30, 0, 9 * 60);
        assert_eq!(t.to_iso(), "2026-09-20T06:30:00Z");
        assert_eq!(Tick::parse_iso("2026-09-20T06:30:00Z").unwrap(), t);
        assert_eq!(Tick::parse_iso("2026-09-20T15:30+09:00").unwrap(), t);
    }
}
