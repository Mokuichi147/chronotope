//! 規則ベースの日本語抽出器（外部サービスを呼ばない）。
//!
//! - 実体: KB のラベルを辞書（gazetteer）として最長一致 + 接尾辞・文型のパターン
//!   （`〇〇氏` `〇〇大臣`、`主催した〇〇`、`〇〇駅` `〇〇県` …）
//! - 出来事: `〇〇が開かれ` `〇〇が発生` などの文型
//! - 時間: 時間表現らしい文字の連続から、エンジンの時間パーサで解析できる最長部分を採用（原文のまま渡す）
//! - 関係: 同じ文の中の出来事と場所・時間、`参加` `出席` などを含む文の人物
//! - 数値: `約1200人が参加` → 観測値 attendees
//!
//! 文型に合わない書き方は取りこぼす。精度が必要な場合は、同じ [`Extraction`] 形式を出力する
//! 別の抽出器（人手・ローカル LLM など）に置き換える。

use crate::schema::*;
use chronotope_core::ResourceId;
use chronotope_core::time::expr::{TemporalExpression, TimeAst};
use chronotope_core::vocab::type_id;
use chronotope_engine::KnowledgeBase;
use chronotope_engine::text::normalize_label;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Event,
    Place,
    Person,
    Organization,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Event => "E",
            Kind::Place => "P",
            Kind::Person => "H",
            Kind::Organization => "O",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GazEntry {
    pub id: ResourceId,
    pub label: String,
    pub kind: Kind,
}

/// KB のラベルから作る辞書。
#[derive(Debug, Default)]
pub struct Gazetteer {
    by_norm: HashMap<String, Vec<GazEntry>>,
    max_len: usize,
    /// 場所の親（located_in / inside / contains）。同名の別地域へのリンクを避けるのに使う。
    parents: HashMap<ResourceId, Vec<ResourceId>>,
}

const MAX_GAZ_LEN: usize = 32;

impl Gazetteer {
    pub fn from_kb(kb: &KnowledgeBase) -> Self {
        let s = kb.store();
        let (place, org, person) = (type_id("Place"), type_id("Organization"), type_id("Person"));
        let mut g = Gazetteer::default();
        for r in s.resources.values() {
            if s.resolve_id(r.id) != r.id {
                continue;
            }
            let cl = s.type_closure(r.types.iter().copied());
            let kind = if cl.contains(&place) {
                Kind::Place
            } else if cl.contains(&org) {
                Kind::Organization
            } else if cl.contains(&person) {
                Kind::Person
            } else {
                continue;
            };
            let label = r.label(Some("ja")).unwrap_or_default().to_string();
            for l in &r.labels {
                let n = normalize_label(&l.text);
                let len = n.chars().count();
                if !(2..=MAX_GAZ_LEN).contains(&len) {
                    continue;
                }
                g.max_len = g.max_len.max(len);
                let v = g.by_norm.entry(n).or_default();
                if !v.iter().any(|e| e.id == r.id) {
                    v.push(GazEntry { id: r.id, label: label.clone(), kind });
                }
            }
        }
        for key in ["located_in", "inside", "contains"] {
            let Some(pid) = s.predicate_by_key.get(key) else { continue };
            for aid in s.by_predicate.get(pid).into_iter().flatten() {
                let a = &s.assertions[aid];
                let (Some(o), true, true) = (a.object.as_resource(), a.status.is_live(), a.polarity == chronotope_core::model::Polarity::Affirmed) else {
                    continue;
                };
                let (child, parent) = if key == "contains" { (o, a.subject) } else { (a.subject, o) };
                g.parents.entry(s.resolve_id(child)).or_default().push(s.resolve_id(parent));
            }
        }
        g
    }

    /// `child` が `ancestor` の配下（祖先に含む）か。
    pub fn within(&self, child: ResourceId, ancestor: ResourceId) -> bool {
        let mut stack = vec![child];
        let mut seen = std::collections::HashSet::new();
        while let Some(x) = stack.pop() {
            if !seen.insert(x) || seen.len() > 64 {
                continue;
            }
            for p in self.parents.get(&x).into_iter().flatten() {
                if *p == ancestor {
                    return true;
                }
                stack.push(*p);
            }
        }
        false
    }

    /// 位置 `i` から始まる最長一致（長さ, 候補）。
    fn longest_at(&self, chars: &[char], i: usize) -> Option<(usize, &[GazEntry])> {
        let max = self.max_len.min(chars.len() - i);
        (2..=max).rev().find_map(|len| {
            let sub: String = chars[i..i + len].iter().collect();
            self.by_norm.get(&normalize_label(&sub)).map(|v| (len, v.as_slice()))
        })
    }
}

// ------------------------------------------------------------------ 文字種

fn is_kanji(c: char) -> bool {
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '々' | 'ヶ' | 'ヵ')
}

fn is_katakana(c: char) -> bool {
    matches!(c, '\u{30A1}'..='\u{30FA}' | 'ー' | '・')
}

fn is_word(c: char) -> bool {
    is_kanji(c) || is_katakana(c) || c.is_ascii_alphanumeric() || matches!(c, '\u{FF10}'..='\u{FF19}' | '\u{FF21}'..='\u{FF3A}' | '\u{FF41}'..='\u{FF5A}')
}

fn is_name_char(c: char) -> bool {
    is_kanji(c) || is_katakana(c)
}

const TIME_CHARS: &str =
    "〇一二三四五六七八九十百千年月日火水木金土時分秒半頃ごろ午前後曜週旬末初昨今明先来翌再毎朝昼夕夜深未方正紀元世代数の～〜~-/:：.治大昭和平成令旧暦";
/// パーサが西暦へ換算できる元号。
const MODERN_ERA_NAMES: &[&str] = &["明治", "大正", "昭和", "平成", "令和"];
/// 時間表現の意味を変える修飾（`来月10日` の `来月`）。これを切り落とした部分一致は採らない。
const RELATIVE_CHARS: &str = "来先昨翌今再毎";
/// 時間表現として採用するには、これらのいずれかを含む必要がある（`1200` などの数だけは除く）。
const TIME_MARKERS: &str = "年月日時分秒頃ごろ曜週旬昨今明先来毎/:：-";

fn is_time_char(c: char) -> bool {
    c.is_ascii_digit() || ('\u{FF10}'..='\u{FF19}').contains(&c) || TIME_CHARS.contains(c)
}

fn text(chars: &[char], s: usize, e: usize) -> String {
    chars[s..e].iter().collect()
}

fn find(chars: &[char], pat: &str, from: usize) -> Option<usize> {
    let p: Vec<char> = pat.chars().collect();
    (from..chars.len().saturating_sub(p.len() - 1)).find(|&i| chars[i..].starts_with(&p))
}

fn find_all(chars: &[char], pat: &str) -> Vec<usize> {
    let mut out = vec![];
    let mut i = 0;
    while let Some(p) = find(chars, pat, i) {
        out.push(p);
        i = p + 1;
    }
    out
}

fn run_before(chars: &[char], end: usize, pred: fn(char) -> bool) -> usize {
    let mut s = end;
    while s > 0 && pred(chars[s - 1]) {
        s -= 1;
    }
    s
}

fn run_after(chars: &[char], start: usize, pred: fn(char) -> bool) -> usize {
    let mut e = start;
    while e < chars.len() && pred(chars[e]) {
        e += 1;
    }
    e
}

/// 時間パーサで解析できる時間表現か。
pub fn valid_time(s: &str) -> bool {
    if !s.chars().any(|c| TIME_MARKERS.contains(c)) {
        return false;
    }
    matches!(TemporalExpression::strict(s, "gregorian").map(|e| e.ast), Ok(ast) if !matches!(ast, TimeAst::Unknown | TimeAst::Unparsed))
}

/// 日付の途中に挟まる注記（`（昭和60年）` `（月曜日）` `（大正時代）`）か。
fn is_annotation(inner: &str) -> bool {
    let t = inner.trim();
    let n = t.chars().count();
    let era_year = MODERN_ERA_NAMES.iter().any(|e| t.starts_with(e)) && t.ends_with('年') && n <= 8;
    let weekday = (n <= 3 && t.contains('曜')) || matches!(t, "月" | "火" | "水" | "木" | "金" | "土" | "日" | "祝");
    era_year || weekday || (t.ends_with("時代") && n <= 6)
}

/// `山川3年` `明治元年` のような元号の年 → Some(西暦へ正確に換算できるか)。
fn era_year(t: &str) -> Option<bool> {
    let cs: Vec<char> = t.trim().chars().collect();
    let (&last, body) = cs.split_last()?;
    if last != '年' {
        return None;
    }
    let num_start = body.iter().position(|c| is_digit(*c) || *c == '元')?;
    let (name, num) = body.split_at(num_start);
    let n: i64 = if num == ['元'] {
        1
    } else if (1..=2).contains(&num.len()) && num.iter().all(|c| is_digit(*c)) {
        num.iter().filter_map(|c| c.to_digit(10).or_else(|| char::from_u32(*c as u32 - 0xFEE0).and_then(|d| d.to_digit(10)))).fold(0, |a, d| a * 10 + d as i64)
    } else {
        return None;
    };
    if !(1..=4).contains(&name.len()) || !name.iter().all(|c| is_kanji(*c)) {
        return None;
    }
    let name: String = name.iter().collect();
    Some(MODERN_ERA_NAMES.contains(&name.as_str()) && !(name == "明治" && n <= 5))
}

/// `1500年` のような 3〜4 桁の西暦年だけか。
fn is_western_year(t: &str) -> bool {
    let t = t.trim();
    t.strip_suffix('年').is_some_and(|d| (3..=4).contains(&d.chars().count()) && d.chars().all(is_digit))
}

/// 末尾が元号の年（`山川3年`）なら (その開始位置, 近代の元号か)。
fn trailing_era_year(clean: &[char]) -> Option<(usize, bool)> {
    let end = clean.len();
    if clean.last() != Some(&'年') {
        return None;
    }
    let mut p = end - 1;
    while p > 0 && (is_digit(clean[p - 1]) || clean[p - 1] == '元') {
        p -= 1;
    }
    if p == end - 1 {
        return None;
    }
    let mut s = p;
    while s > 0 && p - s < 4 && is_kanji(clean[s - 1]) {
        s -= 1;
    }
    let modern = era_year(&clean[s..end].iter().collect::<String>())?;
    Some((s, modern))
}

fn is_digit(c: char) -> bool {
    c.is_ascii_digit() || ('\u{FF10}'..='\u{FF19}').contains(&c)
}

/// 前近代の元号の年（`〇〇6年` `〇〇６年` `〇〇元年`）を含むか。旧暦の月日を西暦と取り違えないよう、
/// この連続からは時間を採らない（西暦は多くの場合、直後の括弧 `（西暦の年月日）` に書かれていて、そちらを採る）。
/// 元号の年数は 2 桁以下なので、`〇〇時代初期1700年` のような 3 桁以上の年は対象外。
fn has_unsupported_era_year(clean: &[char], i: usize, j: usize) -> bool {
    let modern = |p: usize| {
        let before: String = clean[p.saturating_sub(2)..p].iter().collect();
        MODERN_ERA_NAMES.contains(&before.as_str())
    };
    (i..j).any(|p| {
        if p == 0 || !is_kanji(clean[p - 1]) || clean[p - 1] == '暦' || modern(p) {
            return false;
        }
        if clean[p] == '元' && clean.get(p + 1) == Some(&'年') {
            return true;
        }
        if !is_digit(clean[p]) || (p > 0 && is_digit(clean[p - 1])) {
            return false;
        }
        let q = run_after(clean, p, is_digit);
        q - p <= 2 && q < clean.len() && clean[q] == '年'
    })
}

/// 時間表現として採用できる候補か（解析結果の AST を返す）。
fn candidate_ast(s: &str) -> Option<TimeAst> {
    // 年月日・時刻などの語を含むか、ISO 風の日付（`2026-09-20`, `2026/9/20`）であること（`AB-12` などを除く）。
    let cjk_marker = s.chars().any(|c| "年月日時分秒頃ごろ曜週旬昨今明先来毎".contains(c));
    let iso_like = {
        let digits: Vec<&str> = s.split(['-', '/']).collect();
        digits.len() >= 2 && digits[0].len() == 4 && digits.iter().all(|x| !x.is_empty() && x.chars().take_while(|c| c.is_ascii_digit()).count() >= 1)
    };
    if !cjk_marker && !iso_like {
        return None;
    }
    let ast = TemporalExpression::strict(s, "gregorian").ok()?.ast;
    if matches!(ast, TimeAst::Unknown | TimeAst::Unparsed) {
        return None;
    }
    // 3 桁未満の年だけの表現（`2年`）は期間・回数・前近代の元号の年である可能性が高い。
    if let TimeAst::Date { date } = &ast {
        if date.year.is_some_and(|y| (0..100).contains(&y)) && !s.contains("紀元") && !s.contains("西暦") {
            return None;
        }
    }
    Some(ast)
}

/// 解析結果の最初の日付の (年, 月)。
fn year_month(ast: &TimeAst) -> Option<(i64, Option<u32>)> {
    match ast {
        TimeAst::Date { date } => date.year.map(|y| (y, date.month)),
        TimeAst::Approx { inner } => year_month(inner),
        TimeAst::Interval { start, .. } => year_month(start),
        _ => None,
    }
}

/// 年を持たない日付（`4月7日`）か。
fn lacks_year(ast: &TimeAst) -> bool {
    match ast {
        TimeAst::Date { date } => date.year.is_none() && date.year_offset.is_none() && date.month_offset.is_none(),
        TimeAst::Approx { inner } => lacks_year(inner),
        TimeAst::Interval { start, .. } => lacks_year(start),
        _ => false,
    }
}

/// 候補の直前に付いている暦の指定（`ユリウス暦` `グレゴリオ暦` `西暦` …）。
const CALENDAR_PREFIXES: &[&str] = &["ユリウス暦", "グレゴリオ暦", "西暦", "新暦"];

/// `[from, to)` にある時間表現（開始, 終了, 原文）。時間らしい文字の連続ごとに、解析できる最長部分を採る。
/// 日付の途中の注記は読み飛ばす（位置は元の本文に対応させる）。
pub fn find_times(chars: &[char], from: usize, to: usize) -> Vec<(usize, usize, String)> {
    let mut clean: Vec<char> = vec![];
    let mut orig: Vec<usize> = vec![];
    let mut k = from;
    while k < to {
        let c = chars[k];
        if matches!(c, '（' | '(') && k > from && is_time_char(chars[k - 1]) {
            if let Some(close) = (k + 1..to.min(k + 16)).find(|&x| matches!(chars[x], '）' | ')')) {
                let inner = text(chars, k + 1, close);
                match era_year(&inner) {
                    // `1945年（昭和20年）8月15日`: 近代の元号の注記は読み飛ばす。
                    Some(true) => {
                        k = close + 1;
                        continue;
                    }
                    // `1500年（山川3年）4月6日`: 後ろの月日は旧暦なので「旧暦」を挟む（`1500年旧暦4月6日`）。
                    Some(false) => {
                        clean.extend(['旧', '暦']);
                        orig.extend([k, k]);
                        k = close + 1;
                        continue;
                    }
                    None => {}
                }
                // `山川3年（1500年）4月`: 前近代の元号の年を、括弧内の西暦年の旧暦に置き換える（`旧暦1500年4月`）。
                // `昭和20年（1945年）8月15日` のように近代の元号なら、括弧を読み飛ばす。
                if is_western_year(&inner) {
                    if let Some((era_start, modern)) = trailing_era_year(&clean) {
                        if modern {
                            k = close + 1;
                            continue;
                        }
                        clean.truncate(era_start);
                        orig.truncate(era_start);
                        clean.extend(['旧', '暦']);
                        orig.extend([k, k]);
                        for (x, c) in chars.iter().enumerate().take(close).skip(k + 1) {
                            clean.push(*c);
                            orig.push(x);
                        }
                        k = close + 1;
                        continue;
                    }
                }
                if is_annotation(&inner) {
                    k = close + 1;
                    continue;
                }
            }
        }
        clean.push(c);
        orig.push(k);
        k += 1;
    }
    find_times_clean(&clean).into_iter().map(|(s, e, raw)| (orig[s], orig[e - 1] + 1, raw)).collect()
}

fn find_times_clean(chars: &[char]) -> Vec<(usize, usize, String)> {
    let to = chars.len();
    let mut out = vec![];
    let mut i = 0;
    // 前近代の元号の日付を読み飛ばした直後は、年の無い日付（`9月1日`）も同じ旧暦の日付なので採らない。
    let mut era_context = false;
    // 同じ文で直前に書かれた日付の (年, 月)。`2000年4月3日と17日` の `17日` に引き継ぐ。
    let mut context: Option<(i64, Option<u32>)> = None;
    while i < to {
        // 文の終わりと見出し（公開日の【…】）の終わりで文脈を切る。
        if matches!(chars[i], '。' | '\n' | '】') {
            era_context = false;
            context = None;
        }
        if !is_time_char(chars[i]) {
            i += 1;
            continue;
        }
        let j = run_after(chars, i, is_time_char);
        if has_unsupported_era_year(chars, i, j) {
            era_context = true;
            i = j;
            continue;
        }
        let mut found = None;
        'outer: for len in (2..=(j - i).min(32)).rev() {
            for s in i..=j - len {
                // `来月10日` から `10日` だけを採ると別の日付になってしまう。
                if text(chars, i, s).chars().any(|c| RELATIVE_CHARS.contains(c)) {
                    continue;
                }
                // 数の途中から始めない（`123日間` から `23日` を採らない）。
                if s > 0 && is_digit(chars[s - 1]) && is_digit(chars[s]) {
                    continue;
                }
                // 漢数字で始まる語の一部（`山川五月台` の `五月`、`海辺区三日前町` の `三日前`）は時間ではない。
                if "〇一二三四五六七八九十百千".contains(chars[s]) && s > 0 && is_name_char(chars[s - 1]) {
                    continue;
                }
                if chars[s + len - 1] == '月' && s + len < to && is_name_char(chars[s + len]) && !is_time_char(chars[s + len]) {
                    continue;
                }
                let sub = text(chars, s, s + len);
                if let Some(ast) = candidate_ast(&sub) {
                    found = Some((s, s + len, sub, ast));
                    break 'outer;
                }
            }
        }
        let Some((mut s, mut e, mut sub, ast)) = found else {
            i = j;
            continue;
        };
        if lacks_year(&ast) && era_context {
            i = e;
            continue;
        }
        if !lacks_year(&ast) {
            era_context = false;
        }
        // 直前の暦の指定を含める（`ユリウス暦1200年6月1日`）。
        if let Some(p) = CALENDAR_PREFIXES.iter().find(|p| {
            let n = p.chars().count();
            s >= n && text(chars, s - n, s) == **p
        }) {
            let n = p.chars().count();
            let with = text(chars, s - n, e);
            if candidate_ast(&with).is_some() {
                s -= n;
                sub = with;
            }
        }
        // `10日から12日まで` `28日 - 9月1日` は 1 つの区間として扱う。
        let mut k = e;
        while k < to && chars[k] == ' ' {
            k += 1;
        }
        // `4月7日と21日`（2 回に分けて行われた出来事）も区間として扱う。
        let conn =
            ["から", "〜", "～", "~", "-", "－", "—", "と"].iter().find(|c| chars[k..].starts_with(&c.chars().collect::<Vec<_>>())).map(|c| c.chars().count());
        if let Some(cl) = conn {
            let mut k2 = k + cl;
            while k2 < to && chars[k2] == ' ' {
                k2 += 1;
            }
            let j2 = run_after(chars, k2, is_time_char);
            // 相手側は解析できる最長の部分まで縮める（`23日の2回` → `23日`）。
            for end in (k2 + 1..=j2).rev() {
                let with_made = if chars[end..].starts_with(&['ま', 'で']) { end + 2 } else { end };
                let joined = format!("{sub}から{}", text(chars, k2, with_made));
                if candidate_ast(&joined).is_some() {
                    e = with_made;
                    sub = joined;
                    break;
                }
            }
        }
        // 年の無い日付は、同じ文の直前の日付から年（と月）を引き継ぐ。
        match candidate_ast(&sub) {
            Some(a) if lacks_year(&a) => {
                if let Some((y, m)) = context {
                    let prefix = CALENDAR_PREFIXES.iter().find(|p| sub.starts_with(**p)).copied().unwrap_or("");
                    let body = &sub[prefix.len()..];
                    let composed = if body.contains('月') {
                        format!("{prefix}{y}年{body}")
                    } else if let Some(m) = m {
                        format!("{prefix}{y}年{m}月{body}")
                    } else {
                        format!("{prefix}{y}年{body}")
                    };
                    if let Some(a) = candidate_ast(&composed) {
                        sub = composed;
                        // 補った日付も次の日付の文脈になる（`10月2日告示、20日投開票` の `20日` は 10 月）。
                        if let Some(ym) = year_month(&a) {
                            context = Some(ym);
                        }
                    }
                }
            }
            Some(a) => {
                if let Some(ym) = year_month(&a) {
                    context = Some(ym);
                }
            }
            None => {}
        }
        out.push((s, e, sub));
        i = e;
    }
    out
}

/// 時代名（`江戸時代前期` `大正時代`）。具体的な日付が無いときの手がかりとして使う。
pub fn find_periods(chars: &[char], from: usize, to: usize) -> Vec<(usize, usize, String)> {
    let mut out = vec![];
    for (name, ..) in chronotope_core::time::parse::JAPANESE_PERIODS {
        for p in find_all(&chars[..to], name) {
            if p < from {
                continue;
            }
            let e0 = p + name.chars().count();
            let e = ["前期", "中期", "後期", "初期", "初頭", "末期"]
                .iter()
                .find(|x| chars[e0..to].starts_with(&x.chars().collect::<Vec<_>>()))
                .map(|x| e0 + x.chars().count())
                .unwrap_or(e0);
            out.push((p, e, text(chars, p, e)));
        }
    }
    out.sort();
    out
}

/// 実体名の先頭になり得ない位置か（`第9回山川選挙` の `回山川選挙`、`2020年海辺市` の `年海辺市` など）。
fn bad_start(chars: &[char], s: usize) -> bool {
    "年回第号代".contains(chars[s]) || (s > 0 && is_digit(chars[s - 1]))
}

/// 見出し中の 4 桁の西暦年（`2020年〇〇市議会議員選挙` → 2020）。
fn title_year(label: &str) -> Option<i64> {
    let chars: Vec<char> = label.chars().collect();
    let p = find(&chars, "年", 0)?;
    let st = run_before(&chars, p, |c| c.is_ascii_digit());
    (p - st == 4).then(|| text(&chars, st, p).parse().ok()).flatten()
}

/// 時間表現の直後の語による、出来事の時間らしさ（`告示` の日付より `執行` `発生` の日付を優先する）。
fn time_salience(chars: &[char], end: usize) -> i32 {
    let after = text(chars, end, (end + 12).min(chars.len()));
    const GOOD: &[&str] = &["執行", "投票", "投開票", "発生", "行われ", "行なわ", "起き", "起こ", "勃発", "墜落", "開催", "実施", "開戦"];
    const BAD: &[&str] = &["告示", "公示", "表明", "辞任", "辞職", "失職", "死去", "解散", "発表", "決定", "任期", "逮捕", "判決", "発見"];
    let first = |ws: &[&str]| ws.iter().filter_map(|w| after.find(w)).min();
    match (first(GOOD), first(BAD)) {
        (Some(g), Some(b)) if b < g => -1,
        (Some(_), _) => 1,
        (None, Some(_)) => -1,
        _ => 0,
    }
}

fn sentences(chars: &[char]) -> Vec<(usize, usize)> {
    let mut out = vec![];
    let mut s = 0;
    for (i, c) in chars.iter().enumerate() {
        if matches!(c, '。' | '！' | '？' | '!' | '?' | '\n') {
            if i + 1 > s {
                out.push((s, i + 1));
            }
            s = i + 1;
        }
    }
    if s < chars.len() {
        out.push((s, chars.len()));
    }
    out
}

/// 冒頭の `【2026年9月21日】` のような見出し（範囲の終端と、中の日付）。
fn header(chars: &[char]) -> (usize, Option<String>) {
    if chars.first() != Some(&'【') {
        return (0, None);
    }
    let Some(end) = chars.iter().position(|c| *c == '】') else { return (0, None) };
    let t = find_times(chars, 1, end).into_iter().next().map(|x| x.2);
    (end + 1, t)
}

/// `約1,200人` `3万人` → (値, 概数か, 数の開始位置)。`end` は「人」の位置。
fn number_before(chars: &[char], end: usize) -> Option<(f64, bool, usize)> {
    let digit = |c: char| c.is_ascii_digit() || ('\u{FF10}'..='\u{FF19}').contains(&c) || c == ',' || c == '，';
    let mut e = end;
    let mut mult = 1.0;
    if e > 0 && chars[e - 1] == '万' {
        mult = 10_000.0;
        e -= 1;
    }
    let s = run_before(chars, e, digit);
    if s == e {
        return None;
    }
    let num: String = chars[s..e]
        .iter()
        .filter_map(|c| match c {
            '0'..='9' => Some(*c),
            '\u{FF10}'..='\u{FF19}' => char::from_u32(*c as u32 - 0xFF10 + '0' as u32),
            _ => None,
        })
        .collect();
    let v: f64 = num.parse().ok()?;
    let approx_words = ["約", "およそ", "推定", "少なくとも"];
    let approx = approx_words.iter().any(|w| {
        let n = w.chars().count();
        s >= n && text(chars, s - n, s) == *w
    });
    Some((v * mult, approx, s))
}

// ------------------------------------------------------------------ 抽出器

const PERSON_SUFFIXES: &[&str] =
    &["委員長", "容疑者", "被告", "選手", "監督", "大臣", "知事", "市長", "町長", "村長", "区長", "首相", "議員", "社長", "会長", "教授", "代表", "さん", "氏"];
const NOT_NAME: &[&str] = &["担当", "政府", "省", "庁", "委員", "協会", "会社", "部", "課", "局", "本部", "事務", "同", "元", "前", "新"];
/// 人名の末尾に来ない字（`山川県知事` の `山川県` などを人名にしない）。
const NOT_NAME_END: &[char] = &['県', '市', '町', '村', '府', '都', '区', '国', '郡', '党', '軍', '家', '氏'];
const EVENT_TRIGGERS: &[&str] =
    &["が開かれ", "が開催", "が行われ", "が発生", "が起き", "が起こ", "が実施", "が始ま", "が開幕", "が閉幕", "を開催", "を開い", "を実施"];
/// `〇〇があり` は一般的すぎるので、出来事らしい名詞のときだけ出来事とする。
const WEAK_EVENT_TRIGGERS: &[&str] = &["があり", "があった"];
const EVENT_NOUN_SUFFIXES: &[&str] = &[
    "行進",
    "デモ",
    "集会",
    "大会",
    "会議",
    "事故",
    "火災",
    "地震",
    "選挙",
    "式",
    "祭",
    "祭り",
    "展",
    "試合",
    "公演",
    "ライブ",
    "イベント",
    "事件",
    "衝突",
    "爆発",
    "噴火",
    "停電",
    "訓練",
];
/// 行政区画の接尾辞（分割の境目。`京都府京都市` を `京都` で切らないよう府・県を先に見る）。
const PREFECTURE_SUFFIXES: &[char] = &['県', '府', '都'];
/// 場所の候補に含まれていたら組織・施設名の一部とみなして場所にしない語。
const NOT_PLACE: &[&str] = &["警察", "警備", "鉄道", "会社", "協会", "大学", "学校", "本部", "組合", "銀行", "委員会", "政府", "旅客"];
const PARTICIPATION: &[&str] = &["参加", "出席", "姿を見せ", "訪れ", "登壇", "出演", "視察", "来場"];
const PLACE_SUFFIXES: &[(&str, &str)] = &[
    ("スタジアム", "Place"),
    ("競技場", "Place"),
    ("ホール", "Place"),
    ("空港", "TransportFacility"),
    ("神社", "Place"),
    ("公園", "Place"),
    ("広場", "Place"),
    ("球場", "Place"),
    ("会場", "Place"),
    ("駅", "Station"),
    ("港", "TransportFacility"),
    ("県", "Region"),
    ("市", "City"),
    ("区", "City"),
    ("町", "City"),
    ("村", "City"),
    ("城", "Building"),
    ("寺", "Building"),
    ("近海", "Place"),
    ("地方", "Place"),
    ("半島", "Place"),
    ("海峡", "Place"),
    ("沖", "Place"),
    ("島", "Place"),
    ("湾", "Place"),
    ("灘", "Place"),
    ("郡", "Region"),
    ("峠", "Place"),
    ("岳", "Place"),
    ("山", "Place"),
];
const COUNT_METRICS: &[(&str, &str)] = &[
    ("が参加", "attendees"),
    ("が出席", "attendees"),
    ("が来場", "attendees"),
    ("が集ま", "attendees"),
    ("が死亡", "deaths"),
    ("がけが", "injuries"),
    ("が負傷", "injuries"),
];

#[derive(Debug, Clone)]
struct Mention {
    reference: String,
    kind: Kind,
    start: usize,
    end: usize,
}

#[derive(Default)]
struct Builder {
    entities: Vec<EntityMention>,
    by_key: HashMap<(Kind, String), String>,
    mentions: Vec<Mention>,
    taken: Vec<(usize, usize)>,
    counters: HashMap<Kind, usize>,
}

impl Builder {
    fn overlaps(&self, s: usize, e: usize) -> bool {
        self.taken.iter().any(|&(a, b)| s < b && a < e)
    }

    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        kind: Kind,
        types: &[&str],
        label: &str,
        chars: &[char],
        s: usize,
        e: usize,
        description: Option<String>,
        resource: Option<String>,
    ) -> String {
        self.taken.push((s, e));
        let key = (kind, normalize_label(label));
        let reference = match self.by_key.get(&key) {
            Some(r) => r.clone(),
            None => {
                let n = self.counters.entry(kind).or_insert(0);
                *n += 1;
                let r = format!("{}{}", kind.prefix(), n);
                self.entities.push(EntityMention {
                    reference: r.clone(),
                    types: types.iter().map(|t| t.to_string()).collect(),
                    label: label.to_string(),
                    mention: Some(text(chars, s, e)),
                    span: Some([s, e]),
                    description,
                    resource,
                });
                self.by_key.insert(key, r.clone());
                r
            }
        };
        self.mentions.push(Mention { reference: reference.clone(), kind, start: s, end: e });
        reference
    }
}

/// 主題文の出来事らしい名詞の末尾。
const TOPIC_EVENT_SUFFIXES: &[&str] = &[
    "戦い",
    "合戦",
    "の乱",
    "の役",
    "の変",
    "の陣",
    "攻め",
    "侵攻",
    "包囲戦",
    "攻防戦",
    "海戦",
    "戦争",
    "紛争",
    "一揆",
    "騒動",
    "地震",
    "津波",
    "噴火",
    "火災",
    "大火",
    "災害",
    "豪雨",
    "水害",
    "台風",
    "事件",
    "事故",
    "選挙",
    "テロ",
    "大会",
    "博覧会",
    "渇水",
    "飢饉",
    "冷害",
    "大雪",
    "雪害",
    "雪崩",
    "洪水",
    "高潮",
    "竜巻",
    "土石流",
    "山崩れ",
    "崩落",
    "沈没",
    "転覆",
    "脱線",
    "爆発",
    "ハイジャック",
    "暴動",
    "反乱",
    "蜂起",
    "流行",
    "心中",
    "空襲",
    "虐殺",
    "襲撃",
    "焼失",
    "決壊",
];
/// 主題文が出来事を述べていることを示す語。
const TOPIC_EVENT_CUES: &[&str] =
    &["発生", "行われ", "行なわ", "執行", "起き", "起こ", "墜落", "衝突", "勃発", "開催", "開かれ", "投票", "続いた", "襲った", "見舞われ"];

/// 冒頭の主題（`〇〇（読み）は、` `〇〇とは、`）が出来事なら (ラベル, 主題の終端, 本文の開始)。
fn topic_event(chars: &[char], sents: &[(usize, usize)]) -> Option<(String, usize, usize)> {
    let (_, first_end) = *sents.first()?;
    let limit = first_end.min(120);
    let head: String = chars[..limit].iter().collect();
    let (pos, marker) = ["とは、", "とは", "は、", "は,"].iter().filter_map(|m| head.find(m).map(|p| (p, *m))).min_by_key(|(p, _)| *p)?;
    let topic_str = &head[..pos];
    // 読み仮名などの括弧を除く
    let label: String = match topic_str.find(['（', '(']) {
        Some(p) => topic_str[..p].to_string(),
        None => topic_str.to_string(),
    };
    let label = label.trim().to_string();
    let n = label.chars().count();
    if !(2..=40).contains(&n) || label.contains(['、', '。']) {
        return None;
    }
    let sentence: String = chars[..first_end].iter().collect();
    let is_event = TOPIC_EVENT_SUFFIXES.iter().any(|s| label.ends_with(s)) || TOPIC_EVENT_CUES.iter().any(|c| sentence.contains(c));
    if !is_event {
        return None;
    }
    let topic_end = topic_str.chars().count();
    let body = topic_end + marker.chars().count();
    Some((label, n.min(topic_end), body))
}

pub struct RuleExtractor {
    gazetteer: Gazetteer,
}

pub const RULE_EXTRACTOR_NAME: &str = "chronotope-rules";

impl RuleExtractor {
    pub fn new(gazetteer: Gazetteer) -> Self {
        RuleExtractor { gazetteer }
    }

    pub fn from_kb(kb: &KnowledgeBase) -> Self {
        Self::new(Gazetteer::from_kb(kb))
    }

    pub fn extract(&self, doc: &Document) -> Extraction {
        let chars: Vec<char> = doc.text.chars().collect();
        let (header_end, header_time) = header(&chars);
        let sents = sentences(&chars);
        let sent_of = |pos: usize| sents.iter().position(|&(s, e)| s <= pos && pos < e).unwrap_or(0);
        let mut b = Builder::default();

        // 1. 主催者（`主催した〇〇` / `〇〇が主催`）
        let mut organizers = vec![];
        for p in find_all(&chars, "主催した").into_iter().chain(find_all(&chars, "主催する")) {
            let s = p + 4;
            let e = run_after(&chars, s, is_word);
            if (2..=20).contains(&(e - s)) && !b.overlaps(s, e) {
                let label = text(&chars, s, e);
                organizers.push(b.add(Kind::Organization, &["Organization"], &label, &chars, s, e, None, None));
            }
        }
        for p in find_all(&chars, "が主催") {
            let s = run_before(&chars, p, is_word);
            if (2..=20).contains(&(p - s)) && !b.overlaps(s, p) {
                let label = text(&chars, s, p);
                organizers.push(b.add(Kind::Organization, &["Organization"], &label, &chars, s, p, None, None));
            }
        }

        // 2. 人物（`〇〇氏` `〇〇大臣` …）
        for suf in PERSON_SUFFIXES {
            for p in find_all(&chars, suf) {
                let s = run_before(&chars, p, is_name_char);
                let name = text(&chars, s, p);
                let len = p - s;
                if !(2..=6).contains(&len)
                    || NOT_NAME.iter().any(|w| name.contains(w))
                    || name.contains("議会")
                    || name.ends_with(NOT_NAME_END)
                    || bad_start(&chars, s)
                    || b.overlaps(s, p)
                {
                    continue;
                }
                let role = (!matches!(*suf, "氏" | "さん")).then(|| suf.to_string());
                b.add(Kind::Person, &["Person"], &name, &chars, s, p, role, None);
            }
        }

        // 3. 辞書（KB の既存ラベル）による最長一致
        // 語の途中（`新湾岸国際空港` の `湾岸`）や、直後に行政区画の字が続く位置（`浜辺町` の `浜辺`）では採らない。
        let admin_end = |c: char| "県府都道市区町村郡国".contains(c);
        let inside_word = |s: usize, e: usize| {
            let prev_word = s > 0 && is_name_char(chars[s - 1]) && !admin_end(chars[s - 1]) && chars[s - 1] != '現';
            let next_word = e < chars.len() && is_name_char(chars[e]);
            let next_admin = e < chars.len() && admin_end(chars[e]);
            (prev_word && next_word) || next_admin
        };
        let mut i = 0;
        while i < chars.len() {
            match self.gazetteer.longest_at(&chars, i) {
                Some((len, entries)) if !b.overlaps(i, i + len) && !inside_word(i, i + len) => {
                    let kind = entries[0].kind;
                    // 同名の候補が入れ子（`京都` → 京都府 ⊃ 京都市）なら、どちらを指しても誤りにならない外側を採る。
                    let outermost = entries.iter().find(|o| entries.iter().all(|e| e.id == o.id || self.gazetteer.within(e.id, o.id)));
                    let resource = outermost.map(|e| e.id.to_string());
                    let label = match outermost {
                        Some(e) => e.label.clone(),
                        None => text(&chars, i, i + len),
                    };
                    let types: &[&str] = match kind {
                        Kind::Place => &["Place"],
                        Kind::Organization => &["Organization"],
                        _ => &["Person"],
                    };
                    b.add(kind, types, &label, &chars, i, i + len, None, resource);
                    i += len;
                }
                _ => i += 1,
            }
        }

        // 4. 接尾辞による場所（`〇〇駅` `〇〇県` …、辞書に無いもの）
        let mut place_parents: Vec<(String, String, usize, usize)> = vec![];
        let mut i = 0;
        while i < chars.len() {
            if !is_name_char(chars[i]) {
                i += 1;
                continue;
            }
            // 年・回などの助数詞から始めない（`2020年丸石山` は `丸石山` から探す）。
            if bad_start(&chars, i) {
                // `2020年丸石山` の `年` などは 1 字だけ飛ばし、数の直後の語（`2県` `3人`）は語ごと飛ばす。
                i = if "年回第号代".contains(chars[i]) { i + 1 } else { run_after(&chars, i, is_name_char) };
                continue;
            }
            // 既に辞書で一致した部分（`海辺市浜辺町` の `海辺市`）は飛ばして、その後ろから探す。
            if let Some(&(_, te)) = b.taken.iter().find(|&&(a, z)| a <= i && i < z) {
                i = te;
                continue;
            }
            let run_end = run_after(&chars, i, is_name_char);
            let e = b.taken.iter().filter(|&&(a, _)| a > i && a < run_end).map(|&(a, _)| a).min().unwrap_or(run_end);
            // 連続の先頭から、場所の接尾辞で終わる最長の部分（`東京駅丸の内口` → `東京駅`）。
            let found = (i + 2..=e.min(i + 20)).rev().find_map(|k| {
                let cand = text(&chars, i, k);
                PLACE_SUFFIXES
                    .iter()
                    // `山` は `登山` `鉱山` などを避けるため 3 文字以上（`丸石山`）に限る
                    .find(|(suf, _)| cand.ends_with(suf) && cand.chars().count() > suf.chars().count() && (*suf != "山" || cand.chars().count() >= 3))
                    .map(|(_, ty)| (k, cand, *ty))
            });
            // `居城〇〇城` `通称・〇〇` の前置きは地名に含めない
            let found = found.map(|(k, cand, ty)| {
                match ["居城", "通称・", "本拠"].iter().find(|p| cand.starts_with(*p) && cand.chars().count() > p.chars().count() + 1) {
                    Some(p) => {
                        let n = p.chars().count();
                        (k, cand.chars().skip(n).collect::<String>(), ty, n)
                    }
                    None => (k, cand, ty, 0),
                }
            });
            if let Some((k, cand, ty, skip)) = found {
                let i = i + skip;
                // `室町時代` `江戸幕府` の `室町` `江戸`、`三代目市兵衛` の `三代目市` などは地名ではない。
                let after = text(&chars, k, (k + 2).min(chars.len()));
                let not_place = NOT_PLACE.iter().any(|w| cand.contains(w))
                    || ["時代", "幕府", "政権", "様式"].iter().any(|w| after.starts_with(w))
                    || cand.contains("代目")
                    || cand.starts_with(|c: char| "一二三四五六七八九十".contains(c))
                    || bad_start(&chars, i)
                    || cand.starts_with('現');
                if !b.overlaps(i, k) && !not_place {
                    // `石川県輪島市` → `石川県` ⊃ `輪島市`
                    let cc: Vec<char> = cand.chars().collect();
                    let split = if cand.starts_with("北海道") && cc.len() > 4 {
                        Some(3)
                    } else {
                        PREFECTURE_SUFFIXES.iter().find_map(|suf| {
                            let pos = cc.iter().position(|c| c == suf)?;
                            (pos >= 1 && pos + 2 < cc.len()).then_some(pos + 1)
                        })
                    };
                    match split {
                        Some(cut) => {
                            let outer: String = cc[..cut].iter().collect();
                            let inner: String = cc[cut..].iter().collect();
                            let o = b.add(Kind::Place, &["Region"], &outer, &chars, i, i + cut, None, None);
                            let n = b.add(Kind::Place, &[ty], &inner, &chars, i + cut, k, None, None);
                            place_parents.push((n, o, i, k));
                        }
                        None => {
                            b.add(Kind::Place, &[ty], &cand, &chars, i, k, None, None);
                        }
                    }
                }
            }
            i = e;
        }

        // 同じ文の場所のうち、出来事の場所には最も具体的なもの（分割した外側の県などは除く）を使う。
        let outers: Vec<String> = place_parents.iter().map(|(_, o, ..)| o.clone()).collect();
        let resource_of = |entities: &[EntityMention], r: &str| -> Option<ResourceId> {
            entities.iter().find(|e| e.reference == r).and_then(|e| e.resource.as_deref()).and_then(|x| x.parse().ok())
        };
        // 出来事の場所を選ぶ。
        // - `min_start` 以降（主題文なら「は、」の後）を優先し、`現在の〇〇` `（現：〇〇）` と書き添えられた地名を最優先する。
        // - `山川県海辺市` のように続けて書かれた地名は細かい方へ進むが、既存の地名同士では KB 上の配下関係があるときだけ進む
        //   （`海辺市本町` を別県の `本町` にしない）。辞書に無い地名へ進んだ場合は、直前の地名の配下として記録する。
        let place_in =
            |mentions: &[Mention], entities: &[EntityMention], si: usize, pos: usize, min_start: usize| -> Option<(Mention, Vec<(String, String)>)> {
                let cands: Vec<&Mention> = mentions.iter().filter(|m| m.kind == Kind::Place && sent_of(m.start) == si).collect();
                // 続けて書かれた地名（`山川県海辺市`）と `AのB`（`海辺市の丸石山`）を同じ連なりとみなす。
                let next_in_chain = |e: usize| cands.iter().copied().find(|n| n.start == e || (chars.get(e) == Some(&'の') && n.start == e + 1));
                let specific: Vec<&Mention> = cands.iter().copied().filter(|m| !outers.contains(&m.reference)).collect();
                let pool = if specific.is_empty() { cands.clone() } else { specific };
                let body: Vec<&Mention> = pool.iter().copied().filter(|m| m.start >= min_start).collect();
                let pool = if body.is_empty() { pool } else { body };
                // `〇〇で` `〇〇において` のように場所を示す助詞が続く地名（続けて書かれた地名の末尾から判定）を優先する。
                let chain_end = |m: &Mention| {
                    let mut e = m.end;
                    while let Some(n) = next_in_chain(e) {
                        e = n.end;
                    }
                    e
                };
                let locative = |m: &Mention| {
                    let e = chain_end(m);
                    let after = text(&chars, e, (e + 16).min(chars.len()));
                    // 続く細かい地名（`…海辺町浜辺付近）で`）は読み飛ばす
                    let mut after = after.trim_start_matches(|c: char| is_name_char(c) || matches!(c, '）' | ')')).to_string();
                    for w in ["付近", "周辺", "一帯", "近海", "沖", "上空", "内", "の"] {
                        if let Some(r) = after.strip_prefix(w) {
                            after = r.to_string();
                        }
                    }
                    ["で", "において", "にて", "にある", "を震源", "を震央", "に上陸", "を中心"].iter().any(|w| after.starts_with(w))
                };
                let loc: Vec<&Mention> = pool.iter().copied().filter(|m| locative(m)).collect();
                let pool = if loc.is_empty() { pool } else { loc };
                // その中では `現在の〇〇` `（現：〇〇）` と書き添えられた現代の地名を優先する。
                let modern: Vec<&Mention> = pool
                    .iter()
                    .copied()
                    .filter(|m| {
                        ["現在の", "現在", "現：", "現:", "現・", "現 ", "（現", "(現"].iter().any(|w| {
                            let n = w.chars().count();
                            m.start >= n && text(&chars, m.start - n, m.start) == *w
                        })
                    })
                    .collect();
                let pool = if modern.is_empty() { pool } else { modern };
                let mut m = pool.into_iter().min_by_key(|m| (m.start > pos, m.start)).cloned()?;
                let mut last_linked = resource_of(entities, &m.reference).map(|r| (r, m.reference.clone()));
                let mut sub = vec![];
                while let Some(next) = next_in_chain(m.end) {
                    match (resource_of(entities, &next.reference), &last_linked) {
                        (Some(nr), Some((lr, _))) if !self.gazetteer.within(nr, *lr) => break,
                        (Some(nr), _) => last_linked = Some((nr, next.reference.clone())),
                        (None, Some((_, lref))) => sub.push((next.reference.clone(), lref.clone())),
                        (None, None) => {}
                    }
                    m = (*next).clone();
                }
                Some((m, sub))
            };

        // 5. 出来事
        // (参照, 位置, 時間・場所を探し始める位置)
        let mut events: Vec<(String, usize, usize)> = vec![];
        // 5a. 主題文（`〇〇（読み）は、…で行われた戦い` `〇〇とは、…発生した地震である`）
        let topic = topic_event(&chars, &sents);
        let has_topic = topic.is_some();
        if let Some((label, end, body)) = topic {
            let r = b.add(Kind::Event, &["Event"], &label, &chars, 0, end, None, None);
            events.push((r, 0, body));
        }
        // 5b. 文型（`〇〇が開かれ` …）
        let mut ev_marks = vec![];
        for trig in EVENT_TRIGGERS {
            for p in find_all(&chars, trig) {
                let s = run_before(&chars, p, is_word);
                if (2..=20).contains(&(p - s)) && !b.overlaps(s, p) && !valid_time(&text(&chars, s, p)) {
                    ev_marks.push((s, p));
                }
            }
        }
        for trig in WEAK_EVENT_TRIGGERS {
            for p in find_all(&chars, trig) {
                let s = run_before(&chars, p, is_word);
                let noun = text(&chars, s, p);
                if (2..=20).contains(&(p - s)) && !b.overlaps(s, p) && EVENT_NOUN_SUFFIXES.iter().any(|x| noun.ends_with(x)) {
                    ev_marks.push((s, p));
                }
            }
        }
        ev_marks.sort();
        ev_marks.dedup();
        for (s, p) in ev_marks {
            let noun = text(&chars, s, p);
            let si = sent_of(s);
            // 主題文の出来事と同じ文の中の言い換え（`…で地震が発生`）は別の出来事にしない。
            if events.iter().any(|(_, pos, _)| sent_of(*pos) == si) {
                continue;
            }
            // 主題の出来事がある段落では、短い一般名詞（`投票` `火災`）はその出来事の一部とみなす。
            if has_topic && noun.chars().count() <= 4 {
                continue;
            }
            let place = place_in(&b.mentions, &b.entities, si, s, 0);
            let label = match &place {
                Some((m, _)) => {
                    let pl = b.entities.iter().find(|x| x.reference == m.reference).map(|x| x.label.clone()).unwrap_or_default();
                    format!("{pl}の{noun}")
                }
                None => noun.clone(),
            };
            let r = b.add(Kind::Event, &["Event"], &label, &chars, s, p, None, None);
            events.push((r, s, sents[si].0));
        }

        // 6. 主張
        let mut claims = vec![];
        for (inner, outer, s, e) in &place_parents {
            claims.push(ClaimMention {
                subject: inner.clone(),
                predicate: "located_in".into(),
                object: ObjectMention::Ref { reference: outer.clone() },
                span: Some([*s, *e]),
                confidence: Some(0.8),
            });
        }
        let nearest_event = |pos: usize| -> Option<String> {
            events.iter().filter(|(_, s, _)| *s <= pos).max_by_key(|(_, s, _)| *s).or(events.first()).map(|(r, ..)| r.clone())
        };
        for (ev, s, body) in &events {
            let si = sent_of(*body);
            let (ss, se) = sents[si];
            let is_topic = *s == 0 && *body > 0;
            let label = b.entities.iter().find(|e| &e.reference == ev).map(|e| e.label.clone()).unwrap_or_default();
            // 時間の探し方: 本文（主題文なら「は、」の後）→ 見出し（`2020年山川県知事選挙`）
            // → 主題文なら段落の後続の文 → 時代名（`江戸時代前期`）。各段では出来事らしい日付を優先する。
            let best = |c: Vec<(usize, usize, String)>| {
                c.into_iter().enumerate().max_by_key(|(i, (_, e, _))| (time_salience(&chars, *e), -(*i as i64))).map(|(_, t)| t)
            };
            let body_from = (*body).max(ss).max(header_end);
            let mut chosen = best(find_times(&chars, body_from, se)).map(|t| (t, 0.8));
            // 主題文の日付が出来事の日付らしくない（`…の辞職に伴い執行`）なら、
            // 後続の文で出来事らしい語が続く日付（`〇月〇日投開票`）を優先する。
            if is_topic && chosen.as_ref().is_some_and(|((_, e, _), _)| time_salience(&chars, *e) < 0) {
                if let Some(t) = sents.iter().skip(si + 1).flat_map(|&(a, z)| find_times(&chars, a, z)).find(|(_, e, _)| time_salience(&chars, *e) > 0) {
                    chosen = Some((t, 0.7));
                }
            }
            // 主題文に日付が無ければ、後続の文の日付を使う。ただし別の出来事の日付を拾わないよう、
            // 出来事らしい語が続く日付か、見出しと同じ年の日付に限る（`2020年〇〇選挙は、…。…2020年5月3日に投票`）。
            if chosen.is_none() && is_topic {
                let ty = title_year(&label);
                chosen = sents
                    .iter()
                    .skip(si + 1)
                    .flat_map(|&(a, z)| find_times(&chars, a, z))
                    .find(|(_, e, raw)| time_salience(&chars, *e) > 0 || ty.is_some_and(|y| raw.contains(&format!("{y}年"))))
                    .map(|t| (t, 0.7));
            }
            if chosen.is_none() {
                chosen = best(find_times(&chars, ss.max(header_end), *body)).map(|t| (t, 0.7));
            }
            if chosen.is_none() && is_topic {
                chosen = sents.iter().skip(si + 1).find_map(|&(a, z)| best(find_times(&chars, a, z))).map(|t| (t, 0.6));
            }
            if chosen.is_none() {
                let scope_end = if is_topic { chars.len() } else { se };
                chosen = find_periods(&chars, ss, scope_end).into_iter().next().map(|t| (t, 0.4));
            }
            if let Some(((ts, te, mut raw), conf)) = chosen {
                // 年の無い日付は見出しの年で補う（`2019年〇〇選挙は、…4月7日に投票` → `2019年4月7日`）。
                if TemporalExpression::strict(&raw, "gregorian").is_ok_and(|e| lacks_year(&e.ast)) {
                    if let Some(y) = title_year(&label) {
                        raw = format!("{y}年{raw}");
                    }
                }
                claims.push(ClaimMention {
                    subject: ev.clone(),
                    predicate: "occurred_at".into(),
                    object: ObjectMention::Time { time: raw, calendar: None },
                    span: Some([ts, te]),
                    confidence: Some(conf),
                });
            }
            // 主題文に場所が無ければ、段落の後続の文から探す（`…地震である。震源は山川県沖。`）。
            let place = place_in(&b.mentions, &b.entities, si, *s, *body).or_else(|| {
                if !is_topic {
                    return None;
                }
                (si + 1..sents.len()).find_map(|sj| place_in(&b.mentions, &b.entities, sj, sents[sj].0, sents[sj].0))
            });
            if let Some((m, sub)) = place {
                claims.push(ClaimMention {
                    subject: ev.clone(),
                    predicate: "took_place_at".into(),
                    object: ObjectMention::Ref { reference: m.reference.clone() },
                    span: Some([m.start, m.end]),
                    confidence: Some(0.7),
                });
                for (child, parent) in sub {
                    let c = ClaimMention {
                        subject: child,
                        predicate: "located_in".into(),
                        object: ObjectMention::Ref { reference: parent },
                        span: Some([m.start, m.end]),
                        confidence: Some(0.6),
                    };
                    if !claims.contains(&c) {
                        claims.push(c);
                    }
                }
            }
        }
        for org in &organizers {
            let m = b.mentions.iter().find(|m| &m.reference == org).cloned();
            if let (Some(m), Some(ev)) = (m, nearest_event(usize::MAX)) {
                claims.push(ClaimMention {
                    subject: ev,
                    predicate: "organized_by".into(),
                    object: ObjectMention::Ref { reference: org.clone() },
                    span: Some([m.start, m.end]),
                    confidence: Some(0.7),
                });
            }
        }
        for m in b.mentions.iter().filter(|m| m.kind == Kind::Person) {
            let (ss, se) = sents[sent_of(m.start)];
            let sentence = text(&chars, ss, se);
            if PARTICIPATION.iter().any(|w| sentence.contains(w)) {
                if let Some(ev) = nearest_event(m.start) {
                    claims.push(ClaimMention {
                        subject: m.reference.clone(),
                        predicate: "participated_in".into(),
                        object: ObjectMention::Ref { reference: ev },
                        span: Some([ss, se]),
                        confidence: Some(0.6),
                    });
                }
            }
        }
        claims.dedup();

        // 7. 人数の観測値
        let mut observations = vec![];
        for (pat, metric) in COUNT_METRICS {
            for p in find_all(&chars, &format!("人{pat}")) {
                if let (Some((v, approx, s)), Some(ev)) = (number_before(&chars, p), nearest_event(p)) {
                    observations.push(ObservationMention {
                        target: ev,
                        metric: metric.to_string(),
                        value: v,
                        unit: Some("{person}".into()),
                        approximate: approx,
                        span: Some([s, p + 1]),
                    });
                }
            }
        }

        Extraction {
            extractor: ExtractorInfo {
                name: RULE_EXTRACTOR_NAME.into(),
                model: None,
                model_version: Some(env!("CARGO_PKG_VERSION").into()),
                schema_version: SCHEMA_VERSION.into(),
            },
            source_time: header_time,
            entities: b.entities,
            claims,
            observations,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_time_expressions_in_text() {
        let chars: Vec<char> = "【2026年9月21日】20日午後3時半ごろ、約1200人が参加した。先週火曜日にも".chars().collect();
        let t: Vec<String> = find_times(&chars, 0, chars.len()).into_iter().map(|x| x.2).collect();
        assert_eq!(t, vec!["2026年9月21日", "20日午後3時半ごろ", "先週火曜日"]);
    }

    #[test]
    fn keeps_relative_modifiers_and_joins_intervals() {
        let chars: Vec<char> = "来月10日から12日まで開催。昨日の夕方には集会".chars().collect();
        let t: Vec<String> = find_times(&chars, 0, chars.len()).into_iter().map(|x| x.2).collect();
        assert_eq!(t, vec!["来月10日から12日まで", "昨日の夕方"]);
        // 解析できない相対表現は、修飾を落とした部分（`10日`）を採らずに捨てる。
        let chars: Vec<char> = "翌週10日に".chars().collect();
        assert!(find_times(&chars, 0, chars.len()).is_empty());
    }

    /// 架空の地名だけの KB。`本町` は別の府にある同名の町、`湾岸` `浜辺` は語の途中に現れる別名。
    fn gazetteer_kb() -> (KnowledgeBase, HashMap<&'static str, String>) {
        use chronotope_core::model::Principal;
        let mut kb = KnowledgeBase::in_memory(chronotope_engine::KbConfig::default());
        let mut ids = HashMap::new();
        let w = |kb: &mut KnowledgeBase, body: serde_json::Value| kb.write(&Principal::curator("t"), serde_json::from_value(body).unwrap()).unwrap();
        for (key, types, aliases) in [
            ("山川県", "Region", vec![]),
            ("海辺市", "City", vec![]),
            ("谷原府", "Region", vec![]),
            ("本町", "City", vec![]),
            ("湾岸都", "Region", vec!["湾岸"]),
            ("浜辺市", "City", vec!["浜辺"]),
        ] {
            let r =
                w(&mut kb, serde_json::json!({ "op": "create_resource", "resource": { "types": [types], "label": key, "lang": "ja", "aliases": aliases } }));
            ids.insert(key, r["id"].as_str().unwrap().to_string());
        }
        for (c, p) in [("海辺市", "山川県"), ("本町", "谷原府")] {
            w(
                &mut kb,
                serde_json::json!({ "op": "propose_assertion", "subject": ids[c], "predicate": "located_in", "object": { "resource": ids[p] }, "status": "accepted" }),
            );
        }
        (kb, ids)
    }

    fn doc(t: &str) -> Document {
        serde_json::from_value(serde_json::json!({ "text": t })).unwrap()
    }

    fn place_of(x: &Extraction) -> EntityMention {
        let c = x.claims.iter().find(|c| c.predicate == "took_place_at").unwrap();
        let ObjectMention::Ref { reference } = &c.object else { panic!() };
        x.entities.iter().find(|e| &e.reference == reference).unwrap().clone()
    }

    #[test]
    fn place_chains_respect_the_kb_hierarchy() {
        let (kb, ids) = gazetteer_kb();
        let ex = RuleExtractor::from_kb(&kb);
        // 同名の別地域（谷原府の本町）へは進まない
        let x = ex.extract(&doc("2020年3月3日に山川県海辺市本町で事件が発生した。"));
        assert_eq!(place_of(&x).resource.as_deref(), Some(ids["海辺市"].as_str()));
        // 語の途中の別名（新湾岸国際空港の「湾岸」、浜辺町の「浜辺」）にはリンクしない
        let x = ex.extract(&doc("2020年1月1日に新湾岸国際空港で事故が発生した。"));
        assert!(x.entities.iter().all(|e| e.resource.as_deref() != Some(ids["湾岸都"].as_str())));
        let x = ex.extract(&doc("2020年4月4日に海辺市浜辺町で事件が発生した。"));
        assert!(x.entities.iter().all(|e| e.resource.as_deref() != Some(ids["浜辺市"].as_str())));
        // 辞書に無い細かい地名へ進んだら、直前の既知の地名の配下として記録する
        let p = place_of(&x);
        assert_eq!(p.label, "浜辺町");
        let located = x.claims.iter().find(|c| c.predicate == "located_in" && c.subject == p.reference).unwrap();
        assert!(matches!(&located.object, ObjectMention::Ref { reference } if x.entities.iter().any(|e| &e.reference == reference && e.label == "海辺市")));
    }

    #[test]
    fn prefers_places_marked_by_locative_particles() {
        let (kb, ids) = gazetteer_kb();
        let x = RuleExtractor::from_kb(&kb).extract(&doc("〇〇戦争は、谷原府（現在の湾岸都）などが加わり、山川県海辺市で行われた戦いである。"));
        assert_eq!(place_of(&x).resource.as_deref(), Some(ids["海辺市"].as_str()));
        // 組織名の一部は場所にしない
        let x = RuleExtractor::from_kb(&kb).extract(&doc("山川旅客鉄道の〇〇線で2020年5月1日に事故が発生した。"));
        assert!(x.entities.iter().all(|e| !e.label.contains("旅客鉄道")), "{:?}", x.entities);
    }

    #[test]
    fn era_and_shogunate_names_are_not_places() {
        let x = RuleExtractor::new(Gazetteer::default()).extract(&doc("〇〇の戦いは、室町時代に江戸幕府が三代目市兵衛と戦った合戦である。"));
        let labels: Vec<&str> = x.entities.iter().filter(|e| e.types.iter().any(|t| t != "Event")).map(|e| e.label.as_str()).collect();
        assert!(labels.iter().all(|l| !["室町", "江戸幕府", "三代目市"].contains(l)), "{labels:?}");
    }

    #[test]
    fn lunisolar_dates_keep_their_month() {
        let times = |t: &str| -> Vec<String> {
            let chars: Vec<char> = t.chars().collect();
            find_times(&chars, 0, chars.len()).into_iter().map(|x| x.2).collect()
        };
        assert_eq!(times("〇〇の戦いは、1500年（山川3年）4月6日に行われた。"), vec!["1500年旧暦4月6日"]);
        assert_eq!(times("〇〇の戦いは、山川3年（1500年）4月に行われた。"), vec!["旧暦1500年4月"]);
        assert_eq!(times("山川2年（1500年）から山川3年（1501年）にかけて"), vec!["旧暦1500年から旧暦1501年"]);
        assert_eq!(times("1945年（昭和20年）8月15日"), vec!["1945年8月15日"]);
        assert_eq!(times("昭和20年（1945年）8月15日"), vec!["昭和20年8月15日"]);
    }

    #[test]
    fn later_dates_inherit_year_and_month() {
        let times = |t: &str| -> Vec<String> {
            let chars: Vec<char> = t.chars().collect();
            find_times(&chars, 0, chars.len()).into_iter().map(|x| x.2).collect()
        };
        assert_eq!(times("2000年4月3日と17日の2回に分けて投票"), vec!["2000年4月3日から17日"]);
        assert_eq!(times("2000年11月15日の任期満了に伴い、10月2日告示、20日投開票"), vec!["2000年11月15日", "2000年10月2日", "2000年10月20日"]);
        assert_eq!(times("2000年5月20日に告示され、6月3日投開票された"), vec!["2000年5月20日", "2000年6月3日"]);
        assert_eq!(times("ユリウス暦1200年6月1日、グレゴリオ暦6月8日"), vec!["ユリウス暦1200年6月1日", "グレゴリオ暦1200年6月8日"]);
        // 文が変われば引き継がない
        assert_eq!(times("2000年4月3日に告示。17日に投票"), vec!["2000年4月3日", "17日"]);
        // 語の一部の漢数字は時間ではない
        assert!(times("山川五月台で").is_empty());
        assert!(times("海辺区三日前町二丁目").is_empty());
    }

    #[test]
    fn counters_are_not_names_or_places() {
        let x = RuleExtractor::new(Gazetteer::default()).extract(&doc("2020年山川市議会議員選挙は、第9回山川選挙の日に投票が行われた選挙である。"));
        let labels: Vec<&str> = x.entities.iter().map(|e| e.label.as_str()).collect();
        assert!(labels.iter().all(|l| !l.starts_with('回') && !l.starts_with('年')), "{labels:?}");
        assert_eq!(x.entities.iter().filter(|e| e.types == ["Event"]).count(), 1, "`投票` is part of the topic event");
    }

    #[test]
    fn nested_and_possessive_places() {
        let (kb, ids) = gazetteer_kb();
        let ex = RuleExtractor::from_kb(&kb);
        // `海辺市の丸石山`: 辞書に無い山を、既知の市の配下として記録する
        let x = ex.extract(&doc("2020年5月5日に山川県海辺市の丸石山で事故が発生した。"));
        let p = place_of(&x);
        assert_eq!(p.label, "丸石山");
        let located = x.claims.iter().find(|c| c.predicate == "located_in" && c.subject == p.reference).unwrap();
        assert!(
            matches!(&located.object, ObjectMention::Ref { reference } if x.entities.iter().any(|e| &e.reference == reference && e.resource.as_deref() == Some(ids["海辺市"].as_str())))
        );
    }

    #[test]
    fn prefers_the_modern_place_name() {
        let (kb, ids) = gazetteer_kb();
        let x = RuleExtractor::from_kb(&kb).extract(&doc("〇〇の戦いは、1500年1月1日に谷原府（現在の山川県海辺市）で行われた合戦である。"));
        assert_eq!(place_of(&x).resource.as_deref(), Some(ids["海辺市"].as_str()));
    }

    #[test]
    fn splits_prefecture_and_city() {
        let doc: Document = serde_json::from_value(serde_json::json!({ "text": "石川県輪島市で地震が発生した。" })).unwrap();
        let x = RuleExtractor::new(Gazetteer::default()).extract(&doc);
        let labels: Vec<&str> = x.entities.iter().map(|e| e.label.as_str()).collect();
        assert!(labels.contains(&"石川県") && labels.contains(&"輪島市"), "{labels:?}");
        let located = x.claims.iter().find(|c| c.predicate == "located_in").unwrap();
        let label = |r: &str| x.entities.iter().find(|e| e.reference == r).unwrap().label.clone();
        assert_eq!(label(&located.subject), "輪島市");
        let ev = x.entities.iter().find(|e| e.types == ["Event"]).unwrap();
        assert_eq!(ev.label, "輪島市の地震", "the most specific place is used for the event");
        assert!(matches!(&located.object, ObjectMention::Ref { reference } if label(reference) == "石川県"));
    }

    #[test]
    fn extracts_news_article_without_gazetteer() {
        let doc = Document {
            text: "【2026年9月21日】20日午後3時半ごろ、東京駅丸の内口の広場で防災イベントが開かれ、主催した東京都によると約1200人が参加した。会場には防災担当の山田花子大臣も姿を見せた。".into(),
            url: None,
            title: None,
            published: None,
            acquired_at: None,
            license: None,
            calendar: None,
            lang: None,
            kind: None,
            origin: None,
        };
        let x = RuleExtractor::new(Gazetteer::default()).extract(&doc);
        assert_eq!(x.source_time.as_deref(), Some("2026年9月21日"));
        let label = |r: &str| x.entities.iter().find(|e| e.reference == r).map(|e| e.label.clone()).unwrap();
        let ev = x.entities.iter().find(|e| e.types == ["Event"]).unwrap();
        assert_eq!(ev.label, "東京駅の防災イベント");
        let pred = |p: &str| x.claims.iter().find(|c| c.predicate == p).unwrap();
        assert!(matches!(&pred("occurred_at").object, ObjectMention::Time { time, .. } if time == "20日午後3時半ごろ"));
        assert!(matches!(&pred("took_place_at").object, ObjectMention::Ref { reference } if label(reference) == "東京駅"));
        assert!(matches!(&pred("organized_by").object, ObjectMention::Ref { reference } if label(reference) == "東京都"));
        assert_eq!(label(&pred("participated_in").subject), "山田花子");
        assert!(x.entities.iter().all(|e| e.label != "防災担当"), "role words are not names");
        assert_eq!(x.observations.len(), 1);
        assert_eq!(x.observations[0].value, 1200.0);
        assert!(x.observations[0].approximate);
    }
}
