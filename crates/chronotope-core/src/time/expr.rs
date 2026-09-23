//! A. TemporalExpression — 原表現と AST。解決結果とは独立に不変保存する。

use super::{TICKS_PER_DAY, TICKS_PER_HOUR, TICKS_PER_MINUTE, TICKS_PER_SECOND, Tick, days_from_civil, days_in_month, weekday_from_days};
use crate::{Error, ResourceId, Result};
use serde::{Deserialize, Serialize};

/// 時間の原表現。raw_text は解析に失敗しても必ず保存される。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalExpression {
    pub raw_text: String,
    pub ast: TimeAst,
    /// 解釈に使うカレンダー（例: `gregorian`, `gregorian+09:00`, 架空暦のキー）。
    pub calendar_frame: String,
}

impl TemporalExpression {
    /// 原文を解析して式を作る。解析できない場合も `Unknown` として原文を保持する。
    pub fn parse(raw: &str, calendar_frame: &str) -> Self {
        let ast = super::parse::parse_expression(raw).unwrap_or(TimeAst::Unparsed);
        TemporalExpression { raw_text: raw.to_string(), ast, calendar_frame: calendar_frame.to_string() }
    }

    pub fn strict(raw: &str, calendar_frame: &str) -> Result<Self> {
        let ast = super::parse::parse_expression(raw)?;
        Ok(TemporalExpression { raw_text: raw.to_string(), ast, calendar_frame: calendar_frame.to_string() })
    }

    /// 他の Resource / 名前付き出来事への参照（依存関係）。
    pub fn anchors(&self) -> Vec<Anchor> {
        let mut out = Vec::new();
        self.ast.collect_anchors(&mut out);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum TimeAst {
    /// 暦日付（年・月・日・時刻は部分指定可）。年が無い場合は参照時刻の年を補う。
    Date { date: DateSpec },
    /// 「頃」「circa」。
    Approx { inner: Box<TimeAst> },
    /// 「9月1日〜9月10日」。
    Interval { start: Box<TimeAst>, end: Box<TimeAst> },
    /// 参照点（情報源の時刻 or 他の出来事）からの相対。`offset` は負で過去。
    Relative {
        anchor: Anchor,
        offset: OffsetRange,
        unit: Unit,
        #[serde(default)]
        clock: Option<Clock>,
    },
    /// 「先週火曜日」「月曜日の夕方」「先週」。`week_offset` が None なら参照点以前の直近、
    /// `weekday` が None なら週全体（月曜始まり）。
    Weekday {
        week_offset: Option<i32>,
        weekday: Option<u8>,
        #[serde(default)]
        clock: Option<Clock>,
    },
    /// 「毎週金曜日25:30」。
    Recurring {
        rule: Recurrence,
        #[serde(default)]
        clock: Option<Clock>,
    },
    /// 「Aより後、Bより前」。
    Between { after: Vec<Anchor>, before: Vec<Anchor> },
    /// 「不明」。明示的に分からないことが分かっている。
    Unknown,
    /// 解析できなかった原文。
    Unparsed,
}

impl TimeAst {
    fn collect_anchors(&self, out: &mut Vec<Anchor>) {
        match self {
            TimeAst::Approx { inner } => inner.collect_anchors(out),
            TimeAst::Interval { start, end } => {
                start.collect_anchors(out);
                end.collect_anchors(out);
            }
            TimeAst::Relative { anchor, .. } if !matches!(anchor, Anchor::Reference) => out.push(anchor.clone()),
            TimeAst::Between { after, before } => {
                out.extend(after.iter().cloned());
                out.extend(before.iter().cloned());
            }
            _ => {}
        }
    }

    /// 参照時刻（情報源の公開時刻など）が必要な式か。
    pub fn needs_reference(&self) -> bool {
        match self {
            TimeAst::Date { date } => date.year.is_none(),
            TimeAst::Approx { inner } => inner.needs_reference(),
            TimeAst::Interval { start, end } => start.needs_reference() || end.needs_reference(),
            TimeAst::Relative { anchor, .. } => matches!(anchor, Anchor::Reference),
            TimeAst::Weekday { .. } => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Anchor {
    /// 情報源の時刻（source_time、無ければ acquired_at）。
    Reference,
    /// 既知の Resource（Event）。
    Resource(ResourceId),
    /// 原文中の名前（「A事件」）。解決時にラベルで束縛する。
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OffsetRange {
    pub min: i64,
    pub max: i64,
}

impl OffsetRange {
    pub fn exact(n: i64) -> Self {
        OffsetRange { min: n, max: n }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

impl Unit {
    /// 概算の tick 長（Month / Year は平均）。
    pub fn approx_ticks(self) -> i64 {
        match self {
            Unit::Second => TICKS_PER_SECOND,
            Unit::Minute => TICKS_PER_MINUTE,
            Unit::Hour => TICKS_PER_HOUR,
            Unit::Day => TICKS_PER_DAY,
            Unit::Week => 7 * TICKS_PER_DAY,
            Unit::Month => 2_629_746_000,
            Unit::Year => 31_556_952_000,
        }
    }
}

/// 時刻表現。`hour` は 0..=47 を許容し、25:30 のような深夜表記を意味を保ったまま扱う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeOfDay {
    pub hour: u32,
    pub minute: u32,
    #[serde(default)]
    pub second: u32,
    /// 精度: 分・秒を明示したか。
    #[serde(default)]
    pub precision: ClockPrecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockPrecision {
    Hour,
    #[default]
    Minute,
    Second,
}

impl TimeOfDay {
    /// 25:30 → (day_offset = 1, 01:30)。
    pub fn normalize(&self) -> (i64, u32, u32, u32) {
        ((self.hour / 24) as i64, self.hour % 24, self.minute, self.second)
    }

    pub fn len_ticks(&self) -> i64 {
        match self.precision {
            ClockPrecision::Hour => TICKS_PER_HOUR,
            ClockPrecision::Minute => TICKS_PER_MINUTE,
            ClockPrecision::Second => TICKS_PER_SECOND,
        }
    }

    /// 日の開始からの tick（day_offset 込み）。
    pub fn offset_ticks(&self) -> i64 {
        self.hour as i64 * TICKS_PER_HOUR + self.minute as i64 * TICKS_PER_MINUTE + self.second as i64 * TICKS_PER_SECOND
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartOfDay {
    /// 未明 0-5
    Predawn,
    /// 朝 5-10
    Morning,
    /// 午前 0-12
    Am,
    /// 昼 11-14
    Noon,
    /// 午後 12-24
    Pm,
    /// 夕方 16-19
    Evening,
    /// 夜 18-24
    Night,
    /// 深夜 22-27（翌 3 時）
    LateNight,
}

impl PartOfDay {
    /// (開始時, 終了時) — 24 を超える値は翌日。
    pub fn hours(self) -> (u32, u32) {
        match self {
            PartOfDay::Predawn => (0, 5),
            PartOfDay::Morning => (5, 10),
            PartOfDay::Am => (0, 12),
            PartOfDay::Noon => (11, 14),
            PartOfDay::Pm => (12, 24),
            PartOfDay::Evening => (16, 19),
            PartOfDay::Night => (18, 24),
            PartOfDay::LateNight => (22, 27),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Clock {
    At(TimeOfDay),
    Part(PartOfDay),
}

impl Clock {
    /// (日の開始からのオフセット, 長さ)。
    pub fn offset_and_len(&self) -> (i64, i64) {
        match self {
            Clock::At(t) => (t.offset_ticks(), t.len_ticks()),
            Clock::Part(p) => {
                let (a, b) = p.hours();
                (a as i64 * TICKS_PER_HOUR, (b - a) as i64 * TICKS_PER_HOUR)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "freq", rename_all = "snake_case")]
pub enum Recurrence {
    Daily,
    Weekly { weekday: u8 },
    Monthly { day: u32 },
    Yearly { month: u32, day: u32 },
}

impl Recurrence {
    /// 1970-01-01 からの日数 `day` がこの規則に一致するか。
    pub fn matches_day(&self, day: i64) -> bool {
        match *self {
            Recurrence::Daily => true,
            Recurrence::Weekly { weekday } => weekday_from_days(day) == weekday,
            Recurrence::Monthly { day: d } => super::civil_from_days(day).2 == d,
            Recurrence::Yearly { month, day: d } => {
                let (_, m, dd) = super::civil_from_days(day);
                m == month && dd == d
            }
        }
    }
}

/// 月内の区分（上旬・中旬・下旬・末）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonthPart {
    Early,
    Mid,
    Late,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DateSpec {
    /// 天文学的年号（0 = 紀元前 1 年）。None は参照時刻から補う。
    #[serde(default)]
    pub year: Option<i64>,
    #[serde(default)]
    pub month: Option<u32>,
    #[serde(default)]
    pub day: Option<u32>,
    #[serde(default)]
    pub month_part: Option<MonthPart>,
    #[serde(default)]
    pub clock: Option<Clock>,
    /// 明示されたタイムゾーン（分）。
    #[serde(default)]
    pub utc_offset_minutes: Option<i32>,
}

impl DateSpec {
    pub fn validate(&self) -> Result<()> {
        if let Some(m) = self.month {
            if !(1..=12).contains(&m) {
                return Err(Error::Parse(format!("month out of range: {m}")));
            }
        }
        if let Some(d) = self.day {
            let max = match (self.year, self.month) {
                (Some(y), Some(m)) => days_in_month(y, m),
                _ => 31,
            };
            if d == 0 || d > max {
                return Err(Error::Parse(format!("day out of range: {d}")));
            }
        }
        if let Some(Clock::At(t)) = self.clock {
            if t.hour > 47 || t.minute > 59 || t.second > 60 {
                return Err(Error::Parse("time of day out of range".into()));
            }
        }
        Ok(())
    }

    /// グレゴリオ暦での `[start, end)`。`default_offset` は式にタイムゾーンが無い場合の既定値。
    pub fn bounds_gregorian(&self, default_offset: i32) -> Result<(Tick, Tick)> {
        self.validate()?;
        let y = self.year.ok_or_else(|| Error::Parse("year required".into()))?;
        let off = self.utc_offset_minutes.unwrap_or(default_offset) as i64 * TICKS_PER_MINUTE;
        let (d0, d1) = match (self.month, self.day) {
            (None, _) => (days_from_civil(y, 1, 1), days_from_civil(y + 1, 1, 1)),
            (Some(m), None) => {
                let first = days_from_civil(y, m, 1);
                let n = days_in_month(y, m) as i64;
                match self.month_part {
                    None => (first, first + n),
                    Some(MonthPart::Early) => (first, first + 10),
                    Some(MonthPart::Mid) => (first + 10, first + 20),
                    Some(MonthPart::Late) => (first + 20, first + n),
                }
            }
            (Some(m), Some(d)) => {
                let z = days_from_civil(y, m, d);
                (z, z + 1)
            }
        };
        let (s, e) = match (&self.clock, self.day) {
            (Some(c), Some(_)) => {
                let (o, len) = c.offset_and_len();
                let s = d0 * TICKS_PER_DAY + o;
                (s, s + len)
            }
            _ => (d0 * TICKS_PER_DAY, d1 * TICKS_PER_DAY),
        };
        Ok((Tick(s - off), Tick(e - off)))
    }
}
