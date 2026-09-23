//! 時間表現パーサ（日本語・英語・ISO 8601）。
//!
//! 対応例:
//! `2026-09-20 15:30` / `2026年9月頃` / `9月1日〜9月10日` / `数日前` / `先週火曜日` /
//! `月曜日の夕方` / `毎週金曜日25:30` / `A事件の3日前` / `Aより後、Bより前` / `紀元前300年` /
//! `1980年代` / `19世紀` / `9月上旬` / `circa 1204` / `3 days ago` / `last tuesday` / `不明`

use super::expr::*;
use crate::{Error, Result};

pub fn parse_expression(input: &str) -> Result<TimeAst> {
    // 幅だけ正規化した原文（大文字小文字は保持）と、解析用の小文字版。
    // ASCII の小文字化はバイト長を変えないため、両者のオフセットは一致する。
    let wide = normalize_width(input);
    let mut ast = parse_lowercased(&wide.to_ascii_lowercase())?;
    restore_anchor_case(&mut ast, &wide);
    Ok(ast)
}

fn parse_lowercased(s: &str) -> Result<TimeAst> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::Parse("empty temporal expression".into()));
    }
    if matches!(s, "不明" | "unknown" | "未詳" | "不詳" | "n/a" | "?") {
        return Ok(TimeAst::Unknown);
    }
    if let Some(ast) = parse_between(s)? {
        return Ok(ast);
    }
    if let Some((a, b)) = split_interval(s) {
        let start = parse_single(a.trim())?;
        let mut end = parse_single(b.trim())?;
        inherit_date_context(&start, &mut end);
        return Ok(TimeAst::Interval { start: Box::new(start), end: Box::new(end) });
    }
    parse_single(s)
}

/// 全角英数記号を半角へ（全角チルダ「～」は区間記号として残す）。
fn normalize_width(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{FF01}'..='\u{FF5D}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            '\u{3000}' => ' ',
            c => c,
        })
        .collect()
}

/// 小文字化して解析した名前付きアンカーを、原文の大文字小文字に戻す。
fn restore_anchor_case(ast: &mut TimeAst, original: &str) {
    let lower = original.to_ascii_lowercase();
    let fix = |a: &mut Anchor| {
        if let Anchor::Named(n) = a {
            if let Some(i) = lower.find(n.as_str()) {
                *n = original[i..i + n.len()].to_string();
            }
        }
    };
    match ast {
        TimeAst::Relative { anchor, .. } => fix(anchor),
        TimeAst::Between { after, before } => after.iter_mut().chain(before.iter_mut()).for_each(fix),
        TimeAst::Approx { inner } => restore_anchor_case(inner, original),
        TimeAst::Interval { start, end } => {
            restore_anchor_case(start, original);
            restore_anchor_case(end, original);
        }
        _ => {}
    }
}

/// 区間の区切り（〜, ~, " to ", から…まで, ISO の `/`）。
fn split_interval(s: &str) -> Option<(&str, &str)> {
    for sep in ["〜", "～", "~", " to ", " until ", "から"] {
        if let Some(i) = s.find(sep) {
            let (a, b) = (&s[..i], &s[i + sep.len()..]);
            let b = b.strip_suffix("まで").unwrap_or(b);
            if !a.trim().is_empty() && !b.trim().is_empty() {
                return Some((a, b));
            }
        }
    }
    // ISO 8601 interval: 2026-09-01/2026-09-10
    if let Some((a, b)) = s.split_once('/') {
        if a.contains('-') && b.contains('-') {
            return Some((a, b));
        }
    }
    // "2026-09-01 - 2026-09-10"（空白で囲まれたハイフン）
    s.split_once(" - ")
}

/// 「9月1日〜10日」の終端に開始側の年・月を引き継ぐ。
fn inherit_date_context(start: &TimeAst, end: &mut TimeAst) {
    if let (TimeAst::Date { date: s }, TimeAst::Date { date: e }) = (start, end) {
        if e.year.is_none() && s.year.is_some() {
            e.year = s.year;
            if e.month.is_none() && e.day.is_some() {
                e.month = s.month;
            }
        }
        if e.month.is_none() && e.day.is_some() && s.month.is_some() {
            e.month = s.month;
        }
    }
}

fn parse_between(s: &str) -> Result<Option<TimeAst>> {
    let has_ja = s.contains("より後") || s.contains("より前") || s.contains("以降") || s.contains("以前") || s.contains("の後") && s.contains('、');
    let has_en = s.starts_with("after ") || s.starts_with("before ");
    if !has_ja && !has_en {
        return Ok(None);
    }
    let mut after = Vec::new();
    let mut before = Vec::new();
    let parts: Vec<&str> = s.split(['、', ',', '，']).flat_map(|p| p.split(" and ")).map(str::trim).filter(|p| !p.is_empty()).collect();
    for part in parts {
        let (name, is_after) = if let Some(n) = part.strip_suffix("より後").or_else(|| part.strip_suffix("以降")).or_else(|| part.strip_suffix("の後")) {
            (n, true)
        } else if let Some(n) = part.strip_suffix("より前").or_else(|| part.strip_suffix("以前")).or_else(|| part.strip_suffix("の前")) {
            (n, false)
        } else if let Some(n) = part.strip_prefix("after ") {
            (n, true)
        } else if let Some(n) = part.strip_prefix("before ") {
            (n, false)
        } else {
            return Ok(None);
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Parse(format!("missing anchor name in `{part}`")));
        }
        // 「2020年以降」のように錨が日付の場合は区間として扱う。
        if let Ok(TimeAst::Date { date }) = parse_single(name) {
            if date.year.is_some() {
                let d = TimeAst::Date { date };
                let unbounded = TimeAst::Unknown;
                return Ok(Some(if is_after {
                    TimeAst::Interval { start: Box::new(d), end: Box::new(unbounded) }
                } else {
                    TimeAst::Interval { start: Box::new(unbounded), end: Box::new(d) }
                }));
            }
        }
        let anchor = Anchor::Named(name.to_string());
        if is_after { after.push(anchor) } else { before.push(anchor) }
    }
    Ok(Some(TimeAst::Between { after, before }))
}

fn parse_single(s: &str) -> Result<TimeAst> {
    let s = s.trim();
    if s.is_empty() || matches!(s, "不明" | "unknown" | "?") {
        return Ok(TimeAst::Unknown);
    }
    // 近似
    for suf in ["頃", "ごろ", "ころ", "前後", "くらい", "ぐらい", "あたり"] {
        if let Some(rest) = s.strip_suffix(suf) {
            if !rest.is_empty() {
                return Ok(TimeAst::Approx { inner: Box::new(parse_single(rest)?) });
            }
        }
    }
    for pre in ["circa ", "c. ", "ca. ", "c.", "ca.", "about ", "around ", "approx. ", "approximately ", "約", "おおよそ"] {
        if let Some(rest) = s.strip_prefix(pre) {
            if !rest.trim().is_empty() {
                return Ok(TimeAst::Approx { inner: Box::new(parse_single(rest.trim())?) });
            }
        }
    }
    if let Some(ast) = parse_recurring(s)? {
        return Ok(ast);
    }
    if let Some(ast) = parse_decade_century(s) {
        return Ok(ast);
    }
    if let Some(ast) = parse_relative_to_event(s)? {
        return Ok(ast);
    }
    if let Some(ast) = parse_relative_to_reference(s)? {
        return Ok(ast);
    }
    if let Some(ast) = parse_week_relative(s)? {
        return Ok(ast);
    }
    let date = parse_date(s)?;
    Ok(TimeAst::Date { date })
}

// ---------------------------------------------------------------- numbers

fn kanji_digit(c: char) -> Option<i64> {
    Some(match c {
        '〇' | '零' => 0,
        '一' => 1,
        '二' => 2,
        '三' => 3,
        '四' => 4,
        '五' => 5,
        '六' => 6,
        '七' => 7,
        '八' => 8,
        '九' => 9,
        _ => return None,
    })
}

/// 先頭の数値（算用数字 or 漢数字）を読む。
fn take_number(s: &str) -> Option<(i64, &str)> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let n = digits.parse().ok()?;
        return Some((n, &s[digits.len()..]));
    }
    // 漢数字（〜千まで）
    let mut total = 0i64;
    let mut cur = 0i64;
    let mut consumed = 0usize;
    for c in s.chars() {
        if let Some(d) = kanji_digit(c) {
            cur = cur * 10 + d;
        } else if c == '十' || c == '百' || c == '千' {
            let mul = match c {
                '十' => 10,
                '百' => 100,
                _ => 1000,
            };
            total += if cur == 0 { 1 } else { cur } * mul;
            cur = 0;
        } else {
            break;
        }
        consumed += c.len_utf8();
    }
    if consumed == 0 {
        return None;
    }
    Some((total + cur, &s[consumed..]))
}

/// 数量（`3`, `三`, `数` = 2..5, `a`/`an` = 1, `several`/`a few`）。
fn take_quantity(s: &str) -> Option<(OffsetRange, &str)> {
    if let Some(rest) = s.strip_prefix('数') {
        return Some((OffsetRange { min: 2, max: 5 }, rest));
    }
    for (w, r) in [
        ("several ", OffsetRange { min: 3, max: 7 }),
        ("a few ", OffsetRange { min: 2, max: 4 }),
        ("a couple of ", OffsetRange { min: 2, max: 2 }),
        ("an ", OffsetRange::exact(1)),
        ("a ", OffsetRange::exact(1)),
    ] {
        if let Some(rest) = s.strip_prefix(w) {
            return Some((r, rest));
        }
    }
    let (n, rest) = take_number(s)?;
    let rest = rest.trim_start();
    Some((OffsetRange::exact(n), rest))
}

fn take_unit(s: &str) -> Option<(Unit, &str)> {
    const UNITS: &[(&str, Unit)] = &[
        ("秒", Unit::Second),
        ("分", Unit::Minute),
        ("時間", Unit::Hour),
        ("日間", Unit::Day),
        ("日", Unit::Day),
        ("週間", Unit::Week),
        ("週", Unit::Week),
        ("ヶ月", Unit::Month),
        ("ヵ月", Unit::Month),
        ("カ月", Unit::Month),
        ("か月", Unit::Month),
        ("箇月", Unit::Month),
        ("月", Unit::Month),
        ("年間", Unit::Year),
        ("年", Unit::Year),
        ("seconds", Unit::Second),
        ("second", Unit::Second),
        ("minutes", Unit::Minute),
        ("minute", Unit::Minute),
        ("hours", Unit::Hour),
        ("hour", Unit::Hour),
        ("days", Unit::Day),
        ("day", Unit::Day),
        ("weeks", Unit::Week),
        ("week", Unit::Week),
        ("months", Unit::Month),
        ("month", Unit::Month),
        ("years", Unit::Year),
        ("year", Unit::Year),
    ];
    UNITS.iter().find_map(|(w, u)| s.strip_prefix(w).map(|r| (*u, r)))
}

/// `3日前` / `数時間後` / `3 days ago` / `in 2 weeks` → (offset, unit)。
fn parse_offset(s: &str) -> Option<(OffsetRange, Unit)> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("in ") {
        let (q, r) = take_quantity(rest)?;
        let (u, r) = take_unit(r)?;
        return r.trim().is_empty().then_some((q, u));
    }
    let (q, r) = take_quantity(s)?;
    let (u, r) = take_unit(r)?;
    let r = r.trim();
    let neg = OffsetRange { min: -q.max, max: -q.min };
    match r {
        "前" | "ago" | "before" => Some((neg, u)),
        "後" | "later" | "after" | "from now" => Some((q, u)),
        _ => None,
    }
}

// ---------------------------------------------------------------- clock

fn parse_part_of_day(s: &str) -> Option<(Clock, &str)> {
    const PARTS: &[(&str, PartOfDay)] = &[
        ("未明", PartOfDay::Predawn),
        ("早朝", PartOfDay::Morning),
        ("朝", PartOfDay::Morning),
        ("午前中", PartOfDay::Am),
        ("午前", PartOfDay::Am),
        ("昼過ぎ", PartOfDay::Pm),
        ("昼", PartOfDay::Noon),
        ("午後", PartOfDay::Pm),
        ("夕方", PartOfDay::Evening),
        ("夕刻", PartOfDay::Evening),
        ("夜間", PartOfDay::Night),
        ("深夜", PartOfDay::LateNight),
        ("夜", PartOfDay::Night),
        ("early morning", PartOfDay::Predawn),
        ("morning", PartOfDay::Morning),
        ("afternoon", PartOfDay::Pm),
        ("evening", PartOfDay::Evening),
        ("late night", PartOfDay::LateNight),
        ("night", PartOfDay::Night),
    ];
    let s = s.trim_start();
    if let Some(r) = s.strip_prefix("正午").or_else(|| s.strip_prefix("noon")) {
        return Some((Clock::At(TimeOfDay { hour: 12, minute: 0, second: 0, precision: ClockPrecision::Minute }), r));
    }
    if let Some(r) = s.strip_prefix("midnight") {
        return Some((Clock::At(TimeOfDay { hour: 24, minute: 0, second: 0, precision: ClockPrecision::Minute }), r));
    }
    PARTS.iter().find_map(|(w, p)| s.strip_prefix(w).map(|r| (Clock::Part(*p), r)))
}

/// `15:30`, `25:30`, `15:30:05`, `15時30分`, `午後3時半`, `3pm`, `3:30 pm`。
fn parse_clock(s: &str) -> Option<(Clock, &str)> {
    let s = s.trim_start();
    let (pm, s2) = if let Some(r) = s.strip_prefix("午後") {
        (Some(true), r)
    } else if let Some(r) = s.strip_prefix("午前") {
        (Some(false), r)
    } else {
        (None, s)
    };
    if let Some((h, rest)) = take_number(s2) {
        let mut hour = h as u32;
        let mut minute = 0;
        let mut second = 0;
        let mut precision = ClockPrecision::Hour;
        let mut rest = rest;
        let mut matched = false;
        if let Some(r) = rest.strip_prefix(':') {
            let (m, r) = take_number(r)?;
            minute = m as u32;
            precision = ClockPrecision::Minute;
            rest = r;
            if let Some(r) = rest.strip_prefix(':') {
                let (sec, r) = take_number(r)?;
                second = sec as u32;
                precision = ClockPrecision::Second;
                rest = r;
            }
            matched = true;
        } else if let Some(r) = rest.strip_prefix('時') {
            rest = r;
            matched = true;
            if let Some(r) = rest.strip_prefix('半') {
                minute = 30;
                precision = ClockPrecision::Minute;
                rest = r;
            } else if let Some((m, r)) = take_number(rest) {
                if let Some(r) = r.strip_prefix('分') {
                    minute = m as u32;
                    precision = ClockPrecision::Minute;
                    rest = r;
                    if let Some((sec, r)) = take_number(rest) {
                        if let Some(r) = r.strip_prefix('秒') {
                            second = sec as u32;
                            precision = ClockPrecision::Second;
                            rest = r;
                        }
                    }
                }
            }
        }
        let trimmed = rest.trim_start();
        let (en_pm, rest2) = if let Some(r) = trimmed.strip_prefix("pm").or_else(|| trimmed.strip_prefix("p.m.")) {
            (Some(true), r)
        } else if let Some(r) = trimmed.strip_prefix("am").or_else(|| trimmed.strip_prefix("a.m.")) {
            (Some(false), r)
        } else {
            (None, rest)
        };
        if en_pm.is_some() {
            matched = true;
            rest = rest2;
        }
        if !matched {
            return None;
        }
        if let Some(p) = pm.or(en_pm) {
            if hour == 12 {
                hour = if p { 12 } else { 0 };
            } else if p && hour < 12 {
                hour += 12;
            }
        }
        if hour > 47 || minute > 59 || second > 60 {
            return None;
        }
        return Some((Clock::At(TimeOfDay { hour, minute, second, precision }), rest));
    }
    parse_part_of_day(s)
}

/// 日付の後ろに続く時刻部分（`の夕方`, ` 15:30`, `T15:30Z`）。残りが空でなければエラー。
fn parse_trailing_clock(s: &str) -> Result<(Option<Clock>, Option<i32>)> {
    let s = s.trim();
    if s.is_empty() {
        return Ok((None, None));
    }
    let s = s.strip_prefix('の').or_else(|| s.strip_prefix('t')).or_else(|| s.strip_prefix("at ")).unwrap_or(s).trim_start();
    let (clock, rest) = parse_clock(s).ok_or_else(|| Error::Parse(format!("unrecognized time of day `{s}`")))?;
    let (tz, rest) = parse_tz(rest);
    if !rest.trim().is_empty() {
        return Err(Error::Parse(format!("unexpected trailing text `{}`", rest.trim())));
    }
    Ok((Some(clock), tz))
}

fn parse_tz(s: &str) -> (Option<i32>, &str) {
    let t = s.trim_start();
    if let Some(r) = t.strip_prefix('z').or_else(|| t.strip_prefix("utc")).or_else(|| t.strip_prefix("gmt")) {
        return (Some(0), r);
    }
    if let Some(r) = t.strip_prefix("jst") {
        return (Some(540), r);
    }
    if let Some(sign) = t.chars().next().filter(|c| *c == '+' || *c == '-') {
        let body = &t[1..];
        let digits: String = body.chars().filter(|c| c.is_ascii_digit() || *c == ':').take(5).collect();
        let clean: String = digits.chars().filter(|c| c.is_ascii_digit()).collect();
        if clean.len() == 4 || clean.len() == 2 {
            let h: i32 = clean[..2].parse().unwrap_or(0);
            let m: i32 = if clean.len() == 4 { clean[2..].parse().unwrap_or(0) } else { 0 };
            let v = (h * 60 + m) * if sign == '-' { -1 } else { 1 };
            return (Some(v), &body[digits.len()..]);
        }
    }
    (None, s)
}

// ---------------------------------------------------------------- weekdays

fn take_weekday(s: &str) -> Option<(u8, &str)> {
    const JA: &[(char, u8)] = &[('月', 0), ('火', 1), ('水', 2), ('木', 3), ('金', 4), ('土', 5), ('日', 6)];
    let s = s.trim_start();
    let mut chars = s.chars();
    if let Some(c) = chars.next() {
        if let Some((_, w)) = JA.iter().find(|(k, _)| *k == c) {
            let rest = chars.as_str();
            if let Some(r) = rest.strip_prefix("曜日").or_else(|| rest.strip_prefix("曜")) {
                return Some((*w, r));
            }
        }
    }
    const EN: &[(&str, u8)] = &[
        ("monday", 0),
        ("tuesday", 1),
        ("wednesday", 2),
        ("thursday", 3),
        ("friday", 4),
        ("saturday", 5),
        ("sunday", 6),
        ("mon", 0),
        ("tue", 1),
        ("wed", 2),
        ("thu", 3),
        ("fri", 4),
        ("sat", 5),
        ("sun", 6),
    ];
    EN.iter().find_map(|(w, d)| s.strip_prefix(w).map(|r| (*d, r)))
}

fn parse_recurring(s: &str) -> Result<Option<TimeAst>> {
    let (rule, rest) = if let Some(r) = s.strip_prefix("毎週").or_else(|| s.strip_prefix("every ")) {
        if let Some((w, r)) = take_weekday(r) {
            (Recurrence::Weekly { weekday: w }, r)
        } else if let Some(r) = r.strip_prefix("day") {
            (Recurrence::Daily, r)
        } else {
            return Err(Error::Parse(format!("unsupported recurrence `{s}`")));
        }
    } else if let Some(r) = s.strip_prefix("毎日").or_else(|| s.strip_prefix("daily")) {
        (Recurrence::Daily, r)
    } else if let Some(r) = s.strip_prefix("毎月") {
        let (d, r) = take_number(r).ok_or_else(|| Error::Parse("毎月 needs a day".into()))?;
        let r = r.strip_prefix('日').unwrap_or(r);
        (Recurrence::Monthly { day: d as u32 }, r)
    } else if let Some(r) = s.strip_prefix("毎年") {
        let (m, r) = take_number(r).ok_or_else(|| Error::Parse("毎年 needs a date".into()))?;
        let r = r.strip_prefix('月').ok_or_else(|| Error::Parse("毎年 needs 月".into()))?;
        let (d, r) = take_number(r).ok_or_else(|| Error::Parse("毎年 needs a day".into()))?;
        let r = r.strip_prefix('日').unwrap_or(r);
        (Recurrence::Yearly { month: m as u32, day: d as u32 }, r)
    } else {
        return Ok(None);
    };
    let (clock, _) = parse_trailing_clock(rest)?;
    Ok(Some(TimeAst::Recurring { rule, clock }))
}

fn parse_decade_century(s: &str) -> Option<TimeAst> {
    let year_range = |a: i64, b: i64| TimeAst::Interval {
        start: Box::new(TimeAst::Date { date: DateSpec { year: Some(a), ..Default::default() } }),
        end: Box::new(TimeAst::Date { date: DateSpec { year: Some(b), ..Default::default() } }),
    };
    if let Some(r) = s.strip_suffix("年代").or_else(|| s.strip_suffix('s')) {
        let (n, rest) = take_number(r)?;
        if rest.is_empty() && n % 10 == 0 {
            return Some(year_range(n, n + 9));
        }
    }
    if let Some(r) = s.strip_suffix("世紀") {
        let (bc, r) = match r.strip_prefix("紀元前") {
            Some(x) => (true, x),
            None => (false, r),
        };
        let (n, rest) = take_number(r)?;
        if rest.is_empty() && n > 0 {
            return Some(if bc { year_range(1 - n * 100, 1 - ((n - 1) * 100 + 1)) } else { year_range((n - 1) * 100 + 1, n * 100) });
        }
    }
    None
}

fn parse_relative_to_event(s: &str) -> Result<Option<TimeAst>> {
    // 日本語: 「A事件の3日前」
    let mut search_from = s.len();
    while let Some(i) = s[..search_from].rfind('の') {
        let name = s[..i].trim();
        let rest = &s[i + 'の'.len_utf8()..];
        if !name.is_empty() {
            if let Some((offset, unit)) = parse_offset(rest) {
                if !is_reference_word(name) {
                    return Ok(Some(TimeAst::Relative { anchor: Anchor::Named(name.to_string()), offset, unit, clock: None }));
                }
            }
        }
        search_from = i;
    }
    // 英語: "3 days before the incident"
    for (kw, sign) in [(" before ", -1i64), (" after ", 1)] {
        if let Some(i) = s.find(kw) {
            let (q, name) = (&s[..i], s[i + kw.len()..].trim());
            if let Some((qr, r)) = take_quantity(q) {
                if let Some((unit, r)) = take_unit(r) {
                    if r.trim().is_empty() && !name.is_empty() {
                        let offset = if sign < 0 { OffsetRange { min: -qr.max, max: -qr.min } } else { qr };
                        return Ok(Some(TimeAst::Relative { anchor: Anchor::Named(name.to_string()), offset, unit, clock: None }));
                    }
                }
            }
        }
    }
    Ok(None)
}

fn is_reference_word(s: &str) -> bool {
    matches!(s, "今日" | "本日" | "昨日" | "明日" | "今" | "現在")
}

fn parse_relative_to_reference(s: &str) -> Result<Option<TimeAst>> {
    const DAY_WORDS: &[(&str, i64)] = &[
        ("一昨日", -2),
        ("おととい", -2),
        ("昨日", -1),
        ("きのう", -1),
        ("今日", 0),
        ("本日", 0),
        ("きょう", 0),
        ("明日", 1),
        ("あした", 1),
        ("明後日", 2),
        ("あさって", 2),
        ("the day before yesterday", -2),
        ("yesterday", -1),
        ("today", 0),
        ("tomorrow", 1),
    ];
    const OTHER_WORDS: &[(&str, i64, Unit)] = &[
        ("一昨年", -2, Unit::Year),
        ("去年", -1, Unit::Year),
        ("昨年", -1, Unit::Year),
        ("今年", 0, Unit::Year),
        ("来年", 1, Unit::Year),
        ("先月", -1, Unit::Month),
        ("今月", 0, Unit::Month),
        ("来月", 1, Unit::Month),
        ("last year", -1, Unit::Year),
        ("this year", 0, Unit::Year),
        ("next year", 1, Unit::Year),
        ("last month", -1, Unit::Month),
        ("this month", 0, Unit::Month),
        ("next month", 1, Unit::Month),
    ];
    for (w, d) in DAY_WORDS {
        if let Some(rest) = s.strip_prefix(w) {
            let (clock, _) = parse_trailing_clock(rest)?;
            return Ok(Some(TimeAst::Relative { anchor: Anchor::Reference, offset: OffsetRange::exact(*d), unit: Unit::Day, clock }));
        }
    }
    for (w, n, u) in OTHER_WORDS {
        if s == *w {
            return Ok(Some(TimeAst::Relative { anchor: Anchor::Reference, offset: OffsetRange::exact(*n), unit: *u, clock: None }));
        }
    }
    if let Some((offset, unit)) = parse_offset(s) {
        return Ok(Some(TimeAst::Relative { anchor: Anchor::Reference, offset, unit, clock: None }));
    }
    Ok(None)
}

fn parse_week_relative(s: &str) -> Result<Option<TimeAst>> {
    const WEEKS: &[(&str, i32)] = &[("先々週", -2), ("先週", -1), ("今週", 0), ("再来週", 2), ("来週", 1), ("last ", -1), ("this ", 0), ("next ", 1)];
    for (w, off) in WEEKS {
        if let Some(rest) = s.strip_prefix(w) {
            let rest = rest.strip_prefix('の').unwrap_or(rest);
            if let Some((wd, r)) = take_weekday(rest) {
                let (clock, _) = parse_trailing_clock(r)?;
                return Ok(Some(TimeAst::Weekday { week_offset: Some(*off), weekday: Some(wd), clock }));
            }
            if rest.trim().is_empty() || rest.trim() == "week" {
                return Ok(Some(TimeAst::Weekday { week_offset: Some(*off), weekday: None, clock: None }));
            }
        }
    }
    if let Some((wd, r)) = take_weekday(s) {
        let (clock, _) = parse_trailing_clock(r)?;
        return Ok(Some(TimeAst::Weekday { week_offset: None, weekday: Some(wd), clock }));
    }
    Ok(None)
}

// ---------------------------------------------------------------- dates

fn month_from_en(s: &str) -> Option<(u32, &str)> {
    const M: &[(&str, u32)] = &[
        ("january", 1),
        ("february", 2),
        ("march", 3),
        ("april", 4),
        ("may", 5),
        ("june", 6),
        ("july", 7),
        ("august", 8),
        ("september", 9),
        ("october", 10),
        ("november", 11),
        ("december", 12),
        ("jan", 1),
        ("feb", 2),
        ("mar", 3),
        ("apr", 4),
        ("jun", 6),
        ("jul", 7),
        ("aug", 8),
        ("sept", 9),
        ("sep", 9),
        ("oct", 10),
        ("nov", 11),
        ("dec", 12),
    ];
    M.iter().find_map(|(w, m)| s.strip_prefix(w).map(|r| (*m, r.strip_prefix('.').unwrap_or(r))))
}

fn parse_date(input: &str) -> Result<DateSpec> {
    let mut s = input.trim();
    let mut bc = false;
    for pre in ["紀元前", "bc ", "bce ", "b.c. "] {
        if let Some(r) = s.strip_prefix(pre) {
            bc = true;
            s = r.trim_start();
        }
    }
    for suf in [" bc", " bce", " b.c.", "bc", "bce"] {
        if let Some(r) = s.strip_suffix(suf) {
            if r.chars().last().is_some_and(|c| c.is_ascii_digit() || c == '年') {
                bc = true;
                s = r.trim_end();
            }
        }
    }
    for pre in ["西暦", "ad ", "a.d. ", "紀元"] {
        if let Some(r) = s.strip_prefix(pre) {
            s = r.trim_start();
        }
    }
    let mut spec = if let Some(d) = try_iso(s)? {
        d
    } else if let Some(d) = try_japanese(s)? {
        d
    } else if let Some(d) = try_english(s)? {
        d
    } else {
        return Err(Error::Parse(format!("unrecognized temporal expression `{input}`")));
    };
    if bc {
        if let Some(y) = spec.year {
            spec.year = Some(1 - y);
        }
    }
    spec.validate()?;
    Ok(spec)
}

/// ISO 8601 風（`2026`, `2026-09`, `2026-09-20`, `2026-09-20T15:30:00+09:00`, `2026/9/20 15:30`）。
fn try_iso(s: &str) -> Result<Option<DateSpec>> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let ylen = body.chars().take_while(|c| c.is_ascii_digit()).count();
    if ylen == 0 {
        return Ok(None);
    }
    let after_year = &body[ylen..];
    let sep = after_year.chars().next();
    if !(after_year.is_empty() || matches!(sep, Some('-' | '/' | '.' | 't' | ' '))) {
        return Ok(None);
    }
    if ylen < 3 && !after_year.is_empty() {
        // `9/20` のような年なし表記は扱わない（曖昧）。
        return Ok(None);
    }
    let year: i64 = body[..ylen].parse().map_err(|_| Error::Parse("bad year".into()))?;
    let year = if neg { -year } else { year };
    let mut spec = DateSpec { year: Some(year), ..Default::default() };
    let mut rest = after_year;
    if let Some(sep @ ('-' | '/' | '.')) = rest.chars().next() {
        let r = &rest[1..];
        let (m, r) = take_number(r).ok_or_else(|| Error::Parse("bad month".into()))?;
        spec.month = Some(m as u32);
        rest = r;
        if rest.starts_with(sep) {
            let (d, r) = take_number(&rest[1..]).ok_or_else(|| Error::Parse("bad day".into()))?;
            spec.day = Some(d as u32);
            rest = r;
        }
    }
    if !rest.is_empty() {
        if spec.day.is_none() {
            return Ok(None);
        }
        let (clock, tz) = parse_trailing_clock(rest)?;
        spec.clock = clock;
        spec.utc_offset_minutes = tz;
    }
    Ok(Some(spec))
}

/// 日本語表記（`2026年9月20日 15時30分`, `9月20日の夕方`, `2026年9月上旬`, `20日`）。
fn try_japanese(s: &str) -> Result<Option<DateSpec>> {
    if !(s.contains('年') || s.contains('月') || s.contains('日')) {
        return Ok(None);
    }
    let mut spec = DateSpec::default();
    let mut rest = s;
    if let Some((n, r)) = take_number(rest) {
        if let Some(r2) = r.strip_prefix('年') {
            spec.year = Some(n);
            rest = r2;
        }
    }
    if let Some((n, r)) = take_number(rest) {
        if let Some(r2) = r.strip_prefix('月') {
            spec.month = Some(n as u32);
            rest = r2;
            for (w, p) in [
                ("上旬", MonthPart::Early),
                ("初め", MonthPart::Early),
                ("初旬", MonthPart::Early),
                ("中旬", MonthPart::Mid),
                ("下旬", MonthPart::Late),
                ("末", MonthPart::Late),
            ] {
                if let Some(r3) = rest.strip_prefix(w) {
                    spec.month_part = Some(p);
                    rest = r3;
                }
            }
        }
    }
    if let Some((n, r)) = take_number(rest) {
        if let Some(r2) = r.strip_prefix('日') {
            spec.day = Some(n as u32);
            rest = r2;
            // 「20日(日)」「20日（月）」の曜日注記は無視する。
            let t = rest.trim_start();
            if let Some(r3) = t.strip_prefix('(').or_else(|| t.strip_prefix('（')) {
                if let Some(i) = r3.find([')', '）']) {
                    let close = r3[i..].chars().next().map(char::len_utf8).unwrap_or(1);
                    rest = &r3[i + close..];
                }
            }
        }
    }
    if spec.year.is_none() && spec.month.is_none() && spec.day.is_none() {
        return Ok(None);
    }
    let (clock, tz) = parse_trailing_clock(rest)?;
    spec.clock = clock;
    spec.utc_offset_minutes = tz;
    Ok(Some(spec))
}

/// 英語表記（`September 20, 2026`, `20 Sep 2026`, `Sep 2026`）。
fn try_english(s: &str) -> Result<Option<DateSpec>> {
    let mut spec = DateSpec::default();
    let rest;
    if let Some((m, r)) = month_from_en(s) {
        spec.month = Some(m);
        let r = r.trim_start();
        let (a, r) = match take_number(r) {
            Some(x) => x,
            None if r.is_empty() => return Ok(Some(spec)),
            None => return Ok(None),
        };
        let r = r.strip_prefix(',').unwrap_or(r).trim_start();
        if a > 31 {
            spec.year = Some(a);
            rest = r;
        } else {
            spec.day = Some(a as u32);
            match take_number(r) {
                Some((y, r2)) => {
                    spec.year = Some(y);
                    rest = r2;
                }
                None => rest = r,
            }
        }
    } else if let Some((d, r)) = take_number(s) {
        let r = r.trim_start();
        let Some((m, r)) = month_from_en(r) else { return Ok(None) };
        spec.day = Some(d as u32);
        spec.month = Some(m);
        let r = r.trim_start();
        match take_number(r) {
            Some((y, r2)) => {
                spec.year = Some(y);
                rest = r2;
            }
            None => rest = r,
        }
    } else {
        return Ok(None);
    }
    let rest = rest.trim_start().strip_prefix(',').unwrap_or(rest);
    let (clock, tz) = parse_trailing_clock(rest)?;
    spec.clock = clock;
    spec.utc_offset_minutes = tz;
    Ok(Some(spec))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> TimeAst {
        parse_expression(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn absolute_dates() {
        match p("2026-09-20 15:30") {
            TimeAst::Date { date } => {
                assert_eq!((date.year, date.month, date.day), (Some(2026), Some(9), Some(20)));
                assert!(matches!(date.clock, Some(Clock::At(TimeOfDay { hour: 15, minute: 30, .. }))));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(p("2026年9月頃"), TimeAst::Approx { .. }));
        assert!(matches!(p("2026年9月20日 15時30分"), TimeAst::Date { .. }));
        assert!(matches!(p("September 20, 2026"), TimeAst::Date { date: DateSpec { day: Some(20), .. } }));
        match p("紀元前300年") {
            TimeAst::Date { date } => assert_eq!(date.year, Some(-299)),
            o => panic!("{o:?}"),
        }
        assert!(matches!(p("1980年代"), TimeAst::Interval { .. }));
        assert!(matches!(p("9月上旬"), TimeAst::Date { date: DateSpec { month_part: Some(MonthPart::Early), .. } }));
        assert!(matches!(p("circa 1204"), TimeAst::Approx { .. }));
        assert!(matches!(p("２０２６年９月２０日"), TimeAst::Date { .. }));
    }

    #[test]
    fn intervals_inherit_context() {
        match p("2026年9月1日〜10日") {
            TimeAst::Interval { end, .. } => match *end {
                TimeAst::Date { date } => assert_eq!((date.year, date.month, date.day), (Some(2026), Some(9), Some(10))),
                o => panic!("{o:?}"),
            },
            o => panic!("{o:?}"),
        }
        assert!(matches!(p("9月1日〜9月10日"), TimeAst::Interval { .. }));
    }

    #[test]
    fn relative_forms() {
        assert!(matches!(p("数日前"), TimeAst::Relative { anchor: Anchor::Reference, offset: OffsetRange { min: -5, max: -2 }, unit: Unit::Day, .. }));
        assert!(matches!(p("3 days ago"), TimeAst::Relative { offset: OffsetRange { min: -3, max: -3 }, .. }));
        assert!(matches!(p("先週火曜日"), TimeAst::Weekday { week_offset: Some(-1), weekday: Some(1), .. }));
        assert!(matches!(p("月曜日の夕方"), TimeAst::Weekday { week_offset: None, weekday: Some(0), clock: Some(Clock::Part(PartOfDay::Evening)) }));
        match p("A事件の3日前") {
            TimeAst::Relative { anchor: Anchor::Named(n), offset, unit: Unit::Day, .. } => {
                assert_eq!(n, "A事件");
                assert_eq!(offset, OffsetRange::exact(-3));
            }
            o => panic!("{o:?}"),
        }
        assert!(matches!(p("昨日の夜"), TimeAst::Relative { clock: Some(_), .. }));
    }

    #[test]
    fn recurring_and_between() {
        match p("毎週金曜日25:30") {
            TimeAst::Recurring { rule: Recurrence::Weekly { weekday: 4 }, clock: Some(Clock::At(t)) } => {
                assert_eq!(t.normalize(), (1, 1, 30, 0));
            }
            o => panic!("{o:?}"),
        }
        match p("Aより後、Bより前") {
            TimeAst::Between { after, before } => {
                assert_eq!(after, vec![Anchor::Named("A".into())]);
                assert_eq!(before, vec![Anchor::Named("B".into())]);
            }
            o => panic!("{o:?}"),
        }
        assert!(matches!(p("不明"), TimeAst::Unknown));
    }
}
