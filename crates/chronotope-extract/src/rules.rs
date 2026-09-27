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
use chronotope_core::model::LabelKind;
use chronotope_core::time::Tick;
use chronotope_core::time::calendar::CalendarFrame;
use chronotope_core::time::expr::{TemporalExpression, TimeAst};
use chronotope_core::time::resolve::{AnchorResult, ResolveContext, resolve};

const TICKS_PER_DAY: i64 = chronotope_core::time::TICKS_PER_DAY;
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
    Work,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Event => "E",
            Kind::Place => "P",
            Kind::Person => "H",
            Kind::Organization => "O",
            Kind::Work => "W",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GazEntry {
    pub id: ResourceId,
    pub label: String,
    pub kind: Kind,
    /// 優先ラベルとして一致した（別名・旧称だけの一致ではない）。
    pub preferred: bool,
}

/// KB のラベルから作る辞書。
#[derive(Debug, Default)]
pub struct Gazetteer {
    by_norm: HashMap<String, Vec<GazEntry>>,
    /// 行政区画の字を省いた呼び方（`海辺市` → `海辺`、`山川国` → `山川`）。ラベルと重ならないものだけ。
    stems: HashMap<String, Vec<GazEntry>>,
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
            if kind == Kind::Place {
                // 別名（旧称など）からは作らない（旧称 `大海市` から `大海` を作るような誤りを避ける）
                for l in r.labels.iter().filter(|l| l.kind == LabelKind::Preferred && l.lang.as_deref().is_none_or(|x| x == "ja")) {
                    if let Some(stem) = l.text.strip_suffix(['市', '町', '村', '国']).filter(|x| stem_ok(x)) {
                        let v = g.stems.entry(normalize_label(stem)).or_default();
                        if !v.iter().any(|e| e.id == r.id) {
                            v.push(GazEntry { id: r.id, label: label.clone(), kind, preferred: false });
                        }
                    }
                }
            }
            for l in &r.labels {
                let n = normalize_label(&l.text);
                let len = n.chars().count();
                if !(2..=MAX_GAZ_LEN).contains(&len) {
                    continue;
                }
                g.max_len = g.max_len.max(len);
                let v = g.by_norm.entry(n).or_default();
                let preferred = l.kind == LabelKind::Preferred;
                match v.iter_mut().find(|e| e.id == r.id) {
                    Some(e) => e.preferred |= preferred,
                    None => v.push(GazEntry { id: r.id, label: label.clone(), kind, preferred }),
                }
            }
        }
        let by_norm = &g.by_norm;
        g.stems.retain(|k, _| !by_norm.contains_key(k));
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

    pub fn has_label(&self, label: &str) -> bool {
        self.by_norm.contains_key(&normalize_label(label))
    }

    /// 位置 `i` から始まる最長一致（長さ, 候補）。優先ラベルで一致する候補があれば、別名だけの一致
    /// （旧称が他の自治体の名前と同じ場合など）は除く。
    /// 省略形（[`Gazetteer::stems`]）での一致なら 3 つ目が `true`。
    fn longest_at(&self, chars: &[char], i: usize) -> Option<(usize, Vec<&GazEntry>, bool)> {
        let max = self.max_len.min(chars.len() - i);
        (2..=max).rev().find_map(|len| {
            if chars[i].is_whitespace() || chars[i + len - 1].is_whitespace() {
                return None;
            }
            let key = normalize_label(&chars[i..i + len].iter().collect::<String>());
            if let Some(v) = self.by_norm.get(&key) {
                let preferred: Vec<&GazEntry> = v.iter().filter(|e| e.preferred).collect();
                return Some((len, if preferred.is_empty() { v.iter().collect() } else { preferred }, false));
            }
            self.stems.get(&key).map(|v| (len, v.iter().collect(), true))
        })
    }
}

/// 省略形として辞書に入れてよいか（漢字・片仮名 2 字以上で、元号・時代名と重ならない）。
fn stem_ok(stem: &str) -> bool {
    stem.chars().count() >= 2
        && stem.chars().all(is_name_char)
        && !MODERN_ERA_NAMES.contains(&stem)
        && !chronotope_core::time::parse::JAPANESE_PERIODS.iter().any(|(p, ..)| p.strip_suffix("時代") == Some(stem))
}

/// 省略形の地名の直後に続いてよい語（`海辺近傍` `海辺間`）。助詞・記号が続く場合も可。
const STEM_FOLLOW: &[&str] = &["近傍", "付近", "周辺", "近郊", "一帯", "方面", "城下", "市街", "地区", "間"];
/// 省略形の地名がこれに続く場合は出来事の名前の一部（`海辺の戦い`）とみなす。
const STEM_EVENT: &[&str] = &["の戦", "の乱", "の変", "の陣", "の役"];

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
    "〇一二三四五六七八九十百千年月日火水木金土時分秒半頃ごころ午前後曜週旬末初昨今明先来翌再毎朝昼夕夜深未方正紀元世代数の～〜~-/:：.治大昭和平成令旧暦";
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

/// `10/14` のような年の無い `月/日`。
fn is_month_day(s: &str) -> bool {
    s.split_once('/').is_some_and(|(m, d)| {
        let ok = |x: &str| (1..=2).contains(&x.len()) && x.chars().all(|c| c.is_ascii_digit());
        ok(m) && ok(d) && m.parse::<u32>().is_ok_and(|m| (1..=12).contains(&m)) && d.parse::<u32>().is_ok_and(|d| (1..=31).contains(&d))
    })
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
        // 注記の括弧（`1991年〈平成3年〉4月8日` の山括弧も含む）
        if matches!(c, '（' | '(' | '〈') && k > from && is_time_char(chars[k - 1]) {
            if let Some(close) = (k + 1..to.min(k + 16)).find(|&x| matches!(chars[x], '）' | ')' | '〉')) {
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
                // 数の途中から始めない（`123日間` から `23日` を採らない）。時刻の途中（`10：00` の `00`）も同じ。
                if s > 0 && (is_digit(chars[s - 1]) || matches!(chars[s - 1], ':' | '：')) && is_digit(chars[s]) {
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
                // 告知の `【10/14】` `（9/3・午前）` `11/25開催` のように、括弧や区切りに挟まれた `月/日`
                let month_day = is_month_day(&sub) && {
                    let prev_ok = s == 0 || "【（(［[】 　・「".contains(chars[s - 1]);
                    let next_ok = s + len >= to || "（(】]・～〜開【 　①②③④⑤⑥⑦⑧⑨まか」".contains(chars[s + len]);
                    prev_ok && next_ok
                };
                let ast = if month_day { TemporalExpression::strict(&sub, "gregorian").ok().map(|e| e.ast) } else { candidate_ast(&sub) };
                if let Some(ast) = ast {
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

/// 報道の見出しから、角括弧の見出し語（`【速報】` `【山川県】`）と末尾の出典表記を除く。
fn headline(title: &str) -> String {
    let mut t = title.trim();
    while let Some(rest) = t.strip_prefix('【').and_then(|r| r.split_once('】')).map(|(_, r)| r.trim_start()) {
        t = rest;
    }
    t.trim().to_string()
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
    // 括弧・鉤括弧の中（`『走れ!丸石』` `（まるいし!）`）の感嘆符・疑問符では区切らない
    let mut depth = 0i32;
    for (i, c) in chars.iter().enumerate() {
        match c {
            '『' | '「' | '（' | '(' | '〈' | '《' | '“' => depth += 1,
            '』' | '」' | '）' | ')' | '〉' | '》' | '”' => depth = (depth - 1).max(0),
            _ => {}
        }
        // 題名の中の句点（`『海辺、その後。』`）でも区切らない
        let quoted = depth > 0 && chars[s..i].iter().rev().find(|c| matches!(c, '『' | '「' | '“' | '（' | '(')).is_some_and(|c| matches!(c, '『' | '「' | '“'));
        let end = match c {
            '\n' => true,
            '。' => !quoted,
            '！' | '？' | '!' | '?' => depth == 0,
            _ => false,
        };
        if end {
            if c == &'\n' {
                depth = 0;
            }
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
const NOT_NAME_END: &[char] = &['県', '市', '町', '村', '府', '都', '区', '国', '郡', '党', '軍', '家', '氏', '院'];
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
/// 地名の直後に続くと、組織・言語・通貨などの名前の一部になる語（`山川新聞` `山川銀行` `山川人` `山川語`）。
const ORG_AFTER: &[&str] = &[
    "新聞", "経済新聞", "日報", "新報", "放送", "テレビ", "銀行", "大学", "高校", "高等学校", "中学", "小学校", "プロ野球", "リーグ", "シリーズ", "代表",
    "航空", "鉄道", "電力", "ガス", "人", "語", "円", "ドル", "選手", "協会", "連盟", "球団", "ハム",
];
const NOT_PLACE: &[&str] = &["警察", "警備", "鉄道", "会社", "協会", "大学", "学校", "本部", "組合", "銀行", "委員会", "政府", "旅客", "選挙区"];
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
    ("停留場", "Station"),
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
                    // 本文に現れない名前（報道の見出し）は位置を持たない
                    mention: (s < e).then(|| text(chars, s, e)),
                    span: (s < e).then_some([s, e]),
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
    "の戦",
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
    "大戦",
    "抗争",
    "戦役",
    "作戦",
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
    "遭難",
    "法難",
    "崩れ",
    "低気圧",
];
/// 主題文が出来事を述べていることを示す語。
const TOPIC_EVENT_CUES: &[&str] =
    &["発生", "行われ", "行なわ", "執行", "起き", "起こ", "墜落", "衝突", "勃発", "開催", "開かれ", "投票", "続いた", "襲った", "襲撃", "見舞われ"];

/// 冒頭の主題（`〇〇（読み）は、` `〇〇とは、` `『〇〇』は、`）。
struct TopicHead {
    label: String,
    /// 主題の終端（読みの括弧を含む）と本文の開始
    end: usize,
    body: usize,
    /// 読みの括弧の中（`（やまだ たろう、1950年1月1日 - ）` の内側）
    paren: Option<(usize, usize)>,
    /// `『〇〇』` と書かれた題名
    quoted: bool,
}

fn topic_head(chars: &[char], sents: &[(usize, usize)]) -> Option<TopicHead> {
    let (_, first_end) = *sents.first()?;
    let limit = first_end.min(160);
    let head: String = chars[..limit].iter().collect();
    // `〇〇（読み）は戦国時代の…` のように、読みの括弧の直後なら読点の無い `は` `では` も主題の区切りとする
    let (pos, marker) = ["とは、", "とは", "は、", "は,", "）は", "）では", ")は", "』は"]
        .iter()
        .filter_map(|m| head.find(m).map(|p| (p, *m)))
        .map(|(p, m)| match m.strip_prefix(['）', ')', '』']) {
            Some(rest) => (p + m.len() - rest.len(), rest),
            None => (p, m),
        })
        // `〇〇選挙は2019年…` のように、出来事を表す語の直後の `は`
        .chain(TOPIC_EVENT_SUFFIXES.iter().filter_map(|s| head.find(&format!("{s}は")).map(|p| (p + s.len(), "は"))))
        // `〇〇は2001年7月1日に公開された…` のように、日付が直後に続く `は`
        .chain(head_ha_digit(&head).map(|i| (i, "は")))
        .min_by_key(|(p, _)| *p)?;
    let topic_str = head[..pos].trim_end();
    // 読み仮名などの括弧を除く
    let (label, paren) = match topic_str.find(['（', '(']) {
        Some(p) => {
            let open = topic_str[..p].chars().count();
            let close = topic_str.chars().count().saturating_sub(1);
            (topic_str[..p].to_string(), (close > open + 1 && matches!(chars[close], '）' | ')')).then_some((open + 1, close)))
        }
        None => (topic_str.to_string(), None),
    };
    let label = label.trim();
    let quoted = label.starts_with('『') && label.ends_with('』');
    let label = label.trim_start_matches('『').trim_end_matches('』').trim().to_string();
    let n = label.chars().count();
    // 鉤括弧の題名は読点を含んでよい（`『おかえり、海辺』`）
    if !(1..=60).contains(&n) || (!quoted && (!(2..=40).contains(&n) || label.contains(['、', '。']))) {
        return None;
    }
    let topic_end = topic_str.chars().count();
    Some(TopicHead { label, end: topic_end, body: topic_end + marker.chars().count(), paren, quoted })
}

/// 括弧の外で、直後に数字が続く `は` のバイト位置（`〇〇は2001年…`。`（あるいは2001年…）` の中は除く）。
fn head_ha_digit(head: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in head.char_indices() {
        match c {
            '（' | '(' | '『' | '「' | '〈' => depth += 1,
            '）' | ')' | '』' | '」' | '〉' => depth = (depth - 1).max(0),
            'は' if depth == 0 && head[i + c.len_utf8()..].starts_with(|d: char| d.is_ascii_digit()) => return Some(i),
            _ => {}
        }
    }
    None
}

/// 冒頭の主題が出来事なら (ラベル, 主題の終端, 本文の開始)。
fn topic_event(chars: &[char], sents: &[(usize, usize)]) -> Option<(String, usize, usize)> {
    let h = topic_head(chars, sents)?;
    let sentence: String = chars[..sents[0].1].iter().collect();
    // 述語の名詞（`…丸石戦争の第二の合戦である。` `…描かれた架空の戦争。`）が出来事を表す場合も出来事とする
    let predicate = sentence.trim_end_matches(['。', '.']);
    let predicate = ["である", "だ", "のこと", "の名称", "の総称"].iter().fold(predicate, |p, w| p.strip_suffix(w).unwrap_or(p));
    let is_event = TOPIC_EVENT_SUFFIXES.iter().any(|s| h.label.ends_with(s) || predicate.ends_with(s)) || TOPIC_EVENT_CUES.iter().any(|c| sentence.contains(c));
    is_event.then(|| (h.label.clone(), h.label.chars().count().min(h.end), h.body))
}

/// 出来事以外の主題の種類。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Subject {
    /// 読みの括弧に生年月日（`（やまだ たろう、1950年1月1日 - ）`）がある人物
    Person,
    /// 公開・発売などが書かれた作品（`『〇〇』は、1954年に公開された日本映画`）
    Work(&'static str),
    /// 所在が書かれた施設（`〇〇駅（〇〇えき）は、山川県海辺市にある…駅`）
    Facility(&'static str),
    /// 企業・学校・団体（`丸石株式会社は、山川県海辺市に本社を置く…` `丸石高等学校は、…にある…`）
    Organization,
}

/// 組織の名前の終わり・始まり（`丸石株式会社` `海辺高等学校`）。
const ORG_LABEL_SUFFIXES: &[&str] = &[
    "会社", "銀行", "工業", "製作所", "商事", "商会", "電機", "電鉄", "ホールディングス", "グループ", "学校", "大学", "高校", "学園", "学院",
    "協会", "財団", "組合", "新聞社", "放送", "病院",
];
const ORG_LABEL_PREFIXES: &[&str] = &["株式会社", "有限会社", "合同会社"];
/// 組織の所在を表す語（`山川県海辺市に本社を置く`）。
const ORG_LOCATED_CUES: &[&str] = &["に本社を置", "に本社があ", "に本社のあ", "に本店のあ", "に本社を構え", "に本拠を置", "に本店を置", "に本部を置", "に本部があ"];
/// 後ろに組織の所在地が続く語（`本社を山川県海辺市に置く` `本社所在地は山川県海辺市`）。
const ORG_PLACE_AFTER: &[&str] = &["本社所在地は", "本社所在地：", "本社を", "本社は", "本部は", "本店は", "本拠地は"];
/// 設立・開業などを表す語（日付に続く句に書かれる）。
const FOUND_CUES: &[&str] = &["設立", "創業", "創立", "開校", "創設", "開学", "発足", "開業", "創刊", "建立", "築城", "創建", "開設"];
/// 死没を表す語（`山川県海辺市で死去`）。
const DEATH_CUES: &[&str] = &["で死去", "にて死去", "で逝去", "にて逝去", "で没", "にて没", "で亡くな", "にて亡くな", "で死亡", "で永眠"];
/// 主題の施設の名前の終わり（接尾辞の地名に加えて）。
const FACILITY_SUFFIXES: &[(&str, &str)] = &[
    ("大社", "Building"), ("神宮", "Building"), ("八幡宮", "Building"), ("天満宮", "Building"), ("宮", "Building"), ("院", "Building"),
    ("堂", "Building"), ("塔", "Building"), ("橋", "Building"), ("ダム", "Building"), ("峰", "Place"), ("湖", "Place"), ("川", "Place"),
];

/// 作品の公開・発売などを表す語（日付の直後に続く）。
const RELEASE_CUES: &[&str] = &["公開", "発売", "放送", "放映", "刊行", "発行", "初演", "封切", "配信", "上映", "配給"];
/// 作品の種類を表す語と型。
const WORK_TYPES: &[(&str, &str)] = &[
    ("映画", "Movie"), ("ゲーム", "Game"), ("小説", "Book"), ("漫画", "Book"), ("書籍", "Book"), ("絵本", "Book"), ("ライトノベル", "Book"),
    ("アニメ", "Series"), ("ドラマ", "Series"), ("番組", "Series"),
];
/// 作品の種類を表す語（`2001年の日本映画` `1985年製作の作品`）。
const WORK_NOUNS: &[&str] = &["映画", "作品", "ドラマ", "アニメ", "ビデオ", "シネマ"];
/// 施設の所在を表す語。
const LOCATED_CUES: &[&str] = &["にある", "にあった", "に位置する", "に所在する", "に存在した", "に設置されて", "に置かれ", "に建つ", "に鎮座", "にまたが", "にそびえ", "に聳え"];
/// 生没年の区切り。
const LIFE_DASH: &[char] = &['-', '－', '–', '—', '〜', '～', '―'];

fn topic_subject(chars: &[char], sents: &[(usize, usize)], h: &TopicHead) -> Option<Subject> {
    let (_, first_end) = sents[0];
    let body = text(chars, h.body.min(first_end), first_end);
    // `『〇〇大戦争』` のように鉤括弧で書かれた題名は、出来事らしい語で終わっても作品とする
    if !h.quoted && TOPIC_EVENT_SUFFIXES.iter().any(|s| h.label.ends_with(s)) {
        return None;
    }
    if let Some((a, z)) = h.paren {
        if life_span(chars, a, z).is_some() {
            return Some(Subject::Person);
        }
    }
    let times = find_times(chars, h.body.min(first_end), first_end);
    let released = times.iter().any(|(_, e, _)| release_follows(chars, *e));
    let made = WORK_NOUNS.iter().any(|w| body.contains(w)) && times.iter().any(|(_, e, _)| made_follows(chars, *e));
    if h.quoted || released || made {
        // 種類は文の後ろの方に書かれた語で決める（`〇〇のゲームを原作とするテレビアニメ` はアニメ）
        let ty = WORK_TYPES
            .iter()
            .filter_map(|(w, ty)| body.rfind(w).map(|p| (p + w.len(), *ty)))
            .max_by_key(|(p, _)| *p)
            .map_or("Work", |(_, ty)| ty);
        // `劇場版アニメ` `劇場用アニメ` は映画
        let ty = if ["劇場版", "劇場用", "劇場作品", "劇場公開"].iter().any(|w| body.contains(w)) { "Movie" } else { ty };
        return Some(Subject::Work(ty));
    }
    let org_label = ORG_LABEL_SUFFIXES.iter().any(|w| h.label.ends_with(w)) || ORG_LABEL_PREFIXES.iter().any(|w| h.label.starts_with(w));
    if org_label || ORG_LOCATED_CUES.iter().any(|c| body.contains(c)) {
        return Some(Subject::Organization);
    }
    if LOCATED_CUES.iter().any(|c| body.contains(c)) {
        let ends = |suf: &str| h.label.ends_with(suf) && h.label.chars().count() > suf.chars().count();
        if let Some((_, ty)) = PLACE_SUFFIXES.iter().chain(FACILITY_SUFFIXES).find(|(suf, _)| ends(suf)) {
            return Some(Subject::Facility(ty));
        }
    }
    None
}

/// 日付に続く句に設立・開業などが書かれているか（`1950年に設立` `1950年創業`）。
fn founded_follows(chars: &[char], e: usize) -> bool {
    let clause = clause_after(chars, e);
    FOUND_CUES.iter().any(|c| clause.contains(c))
}

/// 本文中の時間表現（開始位置, 終了位置, 原文）。
type TimeSpan = (usize, usize, String);

/// 読みの括弧の中の生没年月日（`1950年1月1日 - 2020年2月2日` `1950年1月1日 - `）。
/// 範囲として一続きに読まれた場合も、区切りの前後に分ける。
fn life_span(chars: &[char], a: usize, z: usize) -> Option<(TimeSpan, Option<TimeSpan>)> {
    let times = find_times(chars, a, z);
    let (s, e, raw) = times.first()?.clone();
    // 時間の抽出は `1950年1月1日 - 2020年2月2日` を `…から…` に整える
    let split = raw.find(LIFE_DASH).map(|k| (k, raw[k..].chars().next().map_or(1, char::len_utf8))).or_else(|| raw.find("から").map(|k| (k, "から".len())));
    if let Some((k, n)) = split {
        let (l, r) = (raw[..k].trim(), raw[k + n..].trim());
        if !valid_time(l) {
            return None;
        }
        let birth = (s, s + l.chars().count(), l.to_string());
        let death = (!r.is_empty() && valid_time(r)).then(|| (e - r.chars().count(), e, r.to_string()));
        return Some((birth, death));
    }
    // 区切りが日付の後に続く（`1950年1月1日 - ）`）。間の注記（`1850年1月1日（山川3年12月1日） - `）は読み飛ばす。
    let mut k = e;
    loop {
        while k < z && (chars[k].is_whitespace() || matches!(chars[k], '）' | ')' | '〉')) {
            k += 1;
        }
        match chars.get(k) {
            Some(&open @ ('（' | '(' | '〈')) if k < z => match matching_close(chars, k, z.min(k + 60), open) {
                Some(c) => k = c + 1,
                None => break,
            },
            _ => break,
        }
    }
    if !(k < z && LIFE_DASH.contains(&chars[k])) {
        return None;
    }
    // 没年月日は区切りより後ろの最初の日付（間の注記の別説の日付は採らない）
    let death = times.into_iter().find(|t| t.0 > k);
    Some(((s, e, raw), death))
}

/// 時間表現が指す区間（UTA tick の開始・終了）。相対表現は `reference` を基準にする。
fn resolve_range(raw: &str, reference: Option<Tick>, calendar: &CalendarFrame) -> Option<(i64, i64)> {
    let expr = TemporalExpression::parse(raw, &calendar.key);
    let ctx = ResolveContext { reference, calendar, lookup: &|_| AnchorResult::NotFound };
    let r = resolve(&expr, &ctx).ok()?;
    Some((r.range.earliest_start.0, r.range.latest_end.0))
}

/// 報道の日付が記事の日付に近いとみなす日数。
const NEWS_WINDOW_DAYS: i64 = 14;

/// 報道の本文の日付のうち、記事の日付に最も近いもの（先の日付は 3 倍遠いとみなす）と、その距離（日）。
fn news_time(times: &[(usize, usize, String)], reference: Option<Tick>, calendar: &CalendarFrame) -> Option<(TimeSpan, i64)> {
    let r = reference?.0;
    let day = crate::rules::TICKS_PER_DAY;
    times
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let (a, z) = resolve_range(&t.2, Some(Tick(r)), calendar)?;
            let dist = if z < r {
                r - z
            } else if a > r + day {
                (a - r - day).saturating_mul(3)
            } else {
                0
            };
            Some(((dist / day, i), t))
        })
        .min_by_key(|(k, _)| *k)
        .map(|((d, _), t)| (t.clone(), d))
}

/// 日付に続く句（読点・句点まで。注記の括弧は読み飛ばす）。
fn clause_after(chars: &[char], e: usize) -> String {
    let mut k = e;
    if matches!(chars.get(k), Some('（' | '(' | '〈')) {
        if let Some(c) = (k + 1..chars.len().min(k + 16)).find(|&x| matches!(chars[x], '）' | ')' | '〉')) {
            k = c + 1;
        }
    }
    let end = (k..chars.len().min(k + 30)).find(|&x| matches!(chars[x], '、' | '。' | '，')).unwrap_or(chars.len().min(k + 30));
    text(chars, k, end)
}

/// `open` の位置の括弧に対応する閉じ括弧（同じ種類の入れ子を数える）。
fn matching_close(chars: &[char], open_at: usize, to: usize, open: char) -> Option<usize> {
    let is_open = |c: char| if open == '〈' { c == '〈' } else { matches!(c, '（' | '(') };
    let is_close = |c: char| if open == '〈' { c == '〉' } else { matches!(c, '）' | ')') };
    let mut depth = 0;
    for (x, &c) in chars.iter().enumerate().take(to).skip(open_at) {
        if is_open(c) {
            depth += 1;
        } else if is_close(c) {
            depth -= 1;
            if depth == 0 {
                return Some(x);
            }
        }
    }
    None
}

/// 日付に続く句に公開・発売などが書かれているか（`2001年7月1日に山川映画の配給で公開された`）。
fn release_follows(chars: &[char], e: usize) -> bool {
    let clause = clause_after(chars, e);
    RELEASE_CUES.iter().any(|c| clause.contains(c))
}

/// 日付に続く句に制作・製作や作品の種類が書かれているか（`1985年製作の映画` `1990年に制作された` `2001年の山川・海辺の合作映画`）。
fn made_follows(chars: &[char], e: usize) -> bool {
    let clause = clause_after(chars, e);
    clause.contains("制作") || clause.contains("製作") || (clause.starts_with('の') && WORK_NOUNS.iter().any(|w| clause.contains(w)))
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

    /// 人物・作品・施設の主題の実体と、その生没年月日・出生地・公開日・所在の主張。
    fn subject_claims(&self, chars: &[char], sents: &[(usize, usize)], h: &TopicHead, subject: Subject, b: &mut Builder) -> Vec<ClaimMention> {
        let label_end = h.label.chars().count().min(h.end);
        let (kind, ty) = match subject {
            Subject::Person => (Kind::Person, "Person"),
            Subject::Work(ty) => (Kind::Work, ty),
            Subject::Facility(ty) => (Kind::Place, ty),
            Subject::Organization => (Kind::Organization, "Organization"),
        };
        // 接尾辞などで既に地名として拾った主題（`〇〇駅`）はその実体を使う
        let existing = b.mentions.iter().find(|m| m.kind == kind && m.start == 0 && m.end == label_end).map(|m| m.reference.clone());
        let me = match existing {
            Some(r) => {
                if let Some(e) = b.entities.iter_mut().find(|e| e.reference == r) {
                    e.types = vec![ty.to_string()];
                }
                r
            }
            None => b.add(kind, &[ty], &h.label, chars, 0, label_end, None, None),
        };
        let (_, first_end) = sents[0];
        let time_claim = |pred: &str, (s, e, raw): (usize, usize, String)| ClaimMention {
            subject: me.clone(),
            predicate: pred.into(),
            object: ObjectMention::Time { time: raw, calendar: None },
            span: Some([s, e]),
            confidence: Some(0.8),
        };
        let mut claims = vec![];
        // 位置 `p` の直前（読点・句点を挟まない）に書かれた地名のうち、最も細かい既存の地名。無ければ最後の地名。
        let place_before = |p: usize, b: &Builder| -> Option<String> {
            let near: Vec<&Mention> = b
                .mentions
                .iter()
                .filter(|m| m.kind == Kind::Place && m.reference != me && m.end <= p && m.start + 30 >= p)
                .filter(|m| !chars[m.end..p].iter().any(|c| "、。，．,.".contains(*c)))
                .collect();
            let res = |m: &Mention| b.entities.iter().find(|e| e.reference == m.reference).and_then(|e| e.resource.as_deref()?.parse::<ResourceId>().ok());
            let linked: Vec<(&Mention, ResourceId)> = near.iter().filter_map(|m| Some((*m, res(m)?))).collect();
            // 前から順に、直前に採った地名の配下にあるものだけ細かい方へ進む（`山川県海辺市浜辺町` の `浜辺町` が別の市の町なら海辺市）
            let mut sorted = linked.clone();
            sorted.sort_by_key(|(m, _)| m.start);
            let mut cur: Option<(&Mention, ResourceId)> = None;
            for (m, r) in sorted {
                match cur {
                    Some((_, cr)) if !self.gazetteer.within(r, cr) => {}
                    _ => cur = Some((m, r)),
                }
            }
            cur.map(|(m, _)| m.reference.clone()).or_else(|| near.last().map(|m| m.reference.clone()))
        };
        // 位置 `p` の直後から続けて書かれた地名のうち、最も細かい既存の地名（`山川県海辺市浜辺町に置く` → 海辺市）
        let place_after = |p: usize, b: &Builder| -> Option<String> {
            let mut chain: Vec<&Mention> = vec![];
            let mut e = p;
            while let Some(m) = b.mentions.iter().filter(|m| m.kind == Kind::Place && m.reference != me).find(|m| m.start == e) {
                chain.push(m);
                e = m.end;
            }
            let res = |m: &Mention| b.entities.iter().find(|x| x.reference == m.reference).and_then(|x| x.resource.as_deref()?.parse::<ResourceId>().ok());
            let mut cur: Option<(&Mention, ResourceId)> = None;
            for m in &chain {
                if let Some(r) = res(m) {
                    match cur {
                        Some((_, cr)) if !self.gazetteer.within(r, cr) => {}
                        _ => cur = Some((m, r)),
                    }
                }
            }
            cur.map(|(m, _)| m.reference.clone()).or_else(|| chain.last().map(|m| m.reference.clone()))
        };
        let place_claim = |pred: &str, r: String, p: usize| ClaimMention {
            subject: me.clone(),
            predicate: pred.into(),
            object: ObjectMention::Ref { reference: r },
            span: Some([p, p]),
            confidence: Some(0.7),
        };
        match subject {
            Subject::Person => {
                if let Some((birth, death)) = h.paren.and_then(|(a, z)| life_span(chars, a, z)) {
                    claims.push(time_claim("birth_date", birth));
                    if let Some(d) = death {
                        claims.push(time_claim("death_date", d));
                    }
                }
                // `山川県海辺市出身` `海辺市生まれ`（段落の最初の 2 文まで）
                // 出身地（育った土地のこともある）より出生地（`生まれ`）を優先する
                let scope = sents.get(3).map_or(chars.len(), |s| s.1);
                let first = |ws: &[&str]| ws.iter().filter_map(|w| find(chars, w, h.body).filter(|p| *p < scope)).min();
                let cue = first(&["生まれ", "で生まれ", "に生まれ", "で誕生"]).or_else(|| first(&["出身"]));
                if let Some(p) = cue {
                    let p = if chars[..p].ends_with(&['で']) || chars[..p].ends_with(&['に']) { p - 1 } else { p };
                    if let Some(r) = place_before(p, b) {
                        claims.push(place_claim("birth_place", r, p));
                    }
                }
                // `山川県海辺市の病院で死去`
                if let Some(p) = first(DEATH_CUES) {
                    if let Some(r) = place_before(p, b) {
                        claims.push(place_claim("death_place", r, p));
                    }
                }
            }
            Subject::Work(_) => {
                // 公開・発売などが続く日付（段落の最初の 3 文まで）。無ければ `2009年の日本映画` `2013年制作` の年。
                let scope = sents.get(2).map_or(chars.len(), |s| s.1);
                let times = find_times(chars, h.body.min(first_end), scope);
                // 公開・放送などが直後の句に書かれた日付か、同じ文の後ろに書かれた年を含む日付のうち最初のもの
                // （`2001年4月1日から2002年3月31日まで、〇〇系列で毎週土曜17:00 - 17:30に放送された` の放送期間）。
                // 毎週の放送枠（`毎週土曜17:00`）は除く。
                let sent_end = |p: usize| sents.iter().find(|&&(a, z)| a <= p && p < z).map_or(chars.len(), |s| s.1);
                let released = times
                    .iter()
                    .filter(|(_, _, raw)| !raw.starts_with('毎'))
                    .find(|(_, e, raw)| {
                        release_follows(chars, *e)
                            || (raw.contains('年') && RELEASE_CUES.iter().any(|c| text(chars, *e, sent_end(*e)).contains(c)))
                    })
                    .cloned();
                let made = || times.iter().filter(|(s, ..)| *s < first_end).find(|(_, e, _)| made_follows(chars, *e));
                if let Some(t) = released.or_else(|| made().cloned()) {
                    claims.push(time_claim("publication_date", t));
                }
            }
            Subject::Organization => {
                // 本社の記述は段落の後ろの方にも書かれる（`…。略称は〇〇。本社は山川県海辺市にあった。`）
                let para_end = (0..chars.len()).find(|&i| chars[i] == '\n').unwrap_or(chars.len());
                let scope = para_end.max(first_end);
                if let Some(t) = find_times(chars, h.body.min(first_end), scope).into_iter().find(|(_, e, _)| founded_follows(chars, *e)) {
                    claims.push(time_claim("inception", t));
                }
                let cue = ORG_LOCATED_CUES.iter().chain(LOCATED_CUES).filter_map(|w| find(chars, w, h.body).filter(|p| *p < scope)).min();
                // `本社を山川県海辺市に置く` `本社所在地は山川県海辺市` `本社は山川県海辺市にあった` は語の後ろの地名
                let after_cue = ORG_PLACE_AFTER.iter().filter_map(|w| find(chars, w, h.body).filter(|p| *p < scope).map(|p| p + w.chars().count())).min();
                let place = match (cue, after_cue) {
                    (Some(p), a) if a.is_none_or(|a| p < a) => place_before(p, b).map(|r| (r, p)),
                    (_, Some(a)) => place_after(a, b).map(|r| (r, a)),
                    _ => None,
                };
                if let Some((r, p)) = place {
                    claims.push(place_claim("located_in", r, p));
                }
            }
            Subject::Facility(_) => {
                // 手がかりの語を前から順に試し、直前に地名がある最初のもの
                let mut cues: Vec<usize> = LOCATED_CUES.iter().flat_map(|w| find_all(chars, w)).filter(|p| *p >= h.body && *p < first_end).collect();
                cues.sort_unstable();
                if let Some((r, p)) = cues.into_iter().find_map(|p| place_before(p, b).map(|r| (r, p))) {
                    claims.push(place_claim("located_in", r, p));
                }
            }
        }
        claims
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
                // `山川県海辺市長浜` の `山川県海辺` + `市長`、`海辺市長` の `海辺`（既知の市の長）は人名ではない。
                let office_of_place = suf.chars().next().is_some_and(|c| "市町村区".contains(c) && self.gazetteer.has_label(&format!("{name}{c}")));
                if !(2..=6).contains(&len)
                    || NOT_NAME.iter().any(|w| name.contains(w))
                    || name.contains(['県', '府', '都'])
                    || office_of_place
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

        // 主題が人物・組織・作品なら、その名前（`山川 太郎` `海辺創元社`）の中の地名は拾わない
        let head = topic_head(&chars, &sents);
        let subject = head.as_ref().and_then(|h| topic_subject(&chars, &sents, h));
        if let (Some(h), Some(Subject::Person | Subject::Organization | Subject::Work(_))) = (&head, subject) {
            b.taken.push((0, h.label.chars().count().min(h.end)));
        }

        // 3. 辞書（KB の既存ラベル）による最長一致
        // 語の途中（`新湾岸国際空港` の `湾岸`）、直後に行政区画の字が続く位置（`浜辺町` の `浜辺`）、
        // 長い町村名の末尾（`丸石浜辺町` の `浜辺町`）では採らない。`湾岸都中町` の `湾岸都` のように、
        // 行政区画の字で終わる一致の後に次の地名が続くのは構わない。
        let admin_end = |c: char| "県府都道市区町村郡国".contains(c);
        let inside_word = |s: usize, e: usize| {
            let ends_admin = admin_end(chars[e - 1]);
            // `現・湾岸都中町` `山川・谷原県` の `・` は区切りとみなす（`丸石・浜辺地区` の `浜辺` は語の途中）
            let prev_word = s > 0
                && is_name_char(chars[s - 1])
                && !admin_end(chars[s - 1])
                && !"現旧".contains(chars[s - 1])
                && !(chars[s - 1] == '・' && ends_admin);
            let next_word = e < chars.len() && is_name_char(chars[e]);
            let next_admin = e < chars.len() && admin_end(chars[e]) && !ends_admin;
            // 英字・片仮名の名前は語の途中（`Seaside` の `side`、`マリンバ` の `マリ`）で採らない
            let same_script = |a: char, b: char| (a.is_ascii_alphanumeric() && b.is_ascii_alphanumeric()) || (is_katakana(a) && is_katakana(b) && b != '・' && a != '・');
            let script_inside = (s > 0 && same_script(chars[s], chars[s - 1])) || (e < chars.len() && same_script(chars[e - 1], chars[e]));
            // `湾岸時間` `東部夏時間` のような時間帯
            let followed_by = |ws: &[&str]| ws.iter().any(|w| text(&chars, e, (e + w.chars().count()).min(chars.len())) == *w);
            let time_zone = followed_by(&["時間", "標準時", "夏時間"]);
            // 組織・言語・通貨などの名前の一部（`山川新聞` `山川人` `山川円`）
            let org_name = followed_by(ORG_AFTER);
            // 都道府県名は前後に語が続いても採る（`第9回大会山川県大会` の `山川県`）
            let prefecture = "都道府県".contains(chars[e - 1]);
            (prev_word && next_word && !prefecture) || next_admin || (prev_word && "町村".contains(chars[e - 1])) || script_inside || time_zone || org_name
        };
        let mut ambiguous: Vec<(String, Vec<(ResourceId, String)>)> = vec![];
        let mut i = 0;
        while i < chars.len() {
            // 省略形（`海辺市` を `海辺` と書く）は、助詞・記号か `近傍` `間` などが続く場合だけ採る。
            let starts_with = |e: usize, w: &str| text(&chars, e, (e + w.chars().count()).min(chars.len())) == w;
            let stem_alone = |s: usize, e: usize| {
                let prev_ok = s == 0 || !is_name_char(chars[s - 1]) || admin_end(chars[s - 1]) || "現旧・".contains(chars[s - 1]);
                let next_ok = e >= chars.len() || !is_name_char(chars[e]) || STEM_FOLLOW.iter().any(|w| starts_with(e, w));
                prev_ok && next_ok && !STEM_EVENT.iter().any(|w| starts_with(e, w))
            };
            match self.gazetteer.longest_at(&chars, i) {
                Some((len, entries, stem)) if !b.overlaps(i, i + len) && !inside_word(i, i + len) && (!stem || stem_alone(i, i + len)) => {
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
                    let r = b.add(kind, types, &label, &chars, i, i + len, None, resource);
                    if outermost.is_none() && kind == Kind::Place {
                        ambiguous.push((r, entries.iter().map(|e| (e.id, e.label.clone())).collect()));
                    }
                    i += len;
                }
                _ => i += 1,
            }
        }
        // 同名の地名（別の都市にもある `中央区` など）は、同じ文書で一意に決まった地名の配下にある候補が
        // 1 つだけならそれを採る。
        let known: Vec<ResourceId> = b
            .mentions
            .iter()
            .filter(|m| m.kind == Kind::Place)
            .filter_map(|m| b.entities.iter().find(|e| e.reference == m.reference)?.resource.as_deref()?.parse().ok())
            .collect();
        for (r, cands) in &ambiguous {
            let inside: Vec<&(ResourceId, String)> = cands.iter().filter(|(c, _)| known.iter().any(|k| k != c && self.gazetteer.within(*c, *k))).collect();
            if let [(c, label)] = inside.as_slice() {
                if let Some(e) = b.entities.iter_mut().find(|e| &e.reference == r) {
                    e.resource = Some(c.to_string());
                    e.label = label.clone();
                }
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
                // 片仮名語の途中（`丸石ホールディングス` の `丸石ホール`）や、首長の職名（`海辺市長` の `海辺市`）で終わるものも地名ではない
                let in_word = k < chars.len() && is_katakana(chars[k - 1]) && is_katakana(chars[k]) && chars[k] != '・';
                let office = k < chars.len() && chars[k] == '長' && "市町村区".contains(chars[k - 1]);
                // `同市` `同県` は前の地名を指す（後で照応を解く）
                let anaphor = cand.starts_with('同') && cand.chars().count() == 2;
                // `山川山地` `山川山脈` の途中の `山` で切らない
                let range = cand.ends_with('山') && ["地", "脈", "系"].iter().any(|w| after.starts_with(w));
                let not_place = NOT_PLACE.iter().any(|w| cand.contains(w))
                    || range
                    || anaphor
                    || in_word
                    || office
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
                        // 都府県の名前として無理のない長さのときだけ分ける（`丸石田園都市線` を `丸石田園都` にしない）
                        PREFECTURE_SUFFIXES.iter().find_map(|suf| {
                            let pos = cc.iter().position(|c| c == suf)?;
                            let outer: String = cc[..=pos].iter().collect();
                            let plausible = match suf {
                                '都' => outer == "東京都",
                                '府' => outer == "京都府" || outer == "大阪府",
                                _ => (2..=3).contains(&pos),
                            };
                            (plausible && pos + 2 < cc.len()).then_some(pos + 1)
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

        // `同市` `同県` `同国` は、それより前の最も近い同じ種類の地名を指す
        for p in find_all(&chars, "同") {
            let Some(&c) = chars.get(p + 1) else { continue };
            if !"市県町村区国".contains(c) || b.overlaps(p, p + 2) || chars.get(p + 2).is_some_and(|x| is_kanji(*x) && !"内外".contains(*x)) {
                continue;
            }
            let ante = b
                .mentions
                .iter()
                .filter(|m| m.kind == Kind::Place && m.end <= p)
                .filter_map(|m| b.entities.iter().find(|e| e.reference == m.reference).map(|e| (m.end, e)))
                .filter(|(_, e)| e.label.ends_with(c))
                .max_by_key(|(end, _)| *end)
                .map(|(_, e)| (e.label.clone(), e.types.clone(), e.resource.clone()));
            if let Some((label, types, resource)) = ante {
                let types: Vec<&str> = types.iter().map(String::as_str).collect();
                b.add(Kind::Place, &types, &label, &chars, p, p + 2, None, resource);
            }
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
                // 辞書に無い場所は、括弧で添えられた既存の地名の配下として記録する。
                // `丸石空港（山川県海辺市）` → 括弧内の最も細かい既存の地名、`海辺市（丸石島）` → 括弧の直前の地名。
                if resource_of(entities, &m.reference).is_none() && !sub.iter().any(|(c, _)| *c == m.reference) {
                    let open = |c: Option<&char>| matches!(c, Some('（' | '('));
                    let mut parent = None;
                    if open(chars.get(m.end)) {
                        let mut p = m.end + 1;
                        if let Some(w) = ["現在の", "現・", "現：", "現:", "現"].iter().find(|w| text(&chars, p, (p + w.chars().count()).min(chars.len())) == **w) {
                            p += w.chars().count();
                        }
                        let mut n = cands.iter().copied().find(|n| n.start == p);
                        while let Some(x) = n {
                            if resource_of(entities, &x.reference).is_some() {
                                parent = Some(x.reference.clone());
                            }
                            n = next_in_chain(x.end);
                        }
                    }
                    if parent.is_none() && m.start > 0 && open(chars.get(m.start - 1)) {
                        if let Some(before) = cands.iter().copied().find(|n| n.end + 1 == m.start) {
                            parent = if resource_of(entities, &before.reference).is_some() {
                                Some(before.reference.clone())
                            } else {
                                // 直前の地名が辞書に無い（`海辺市の丸石空港（丸石港）`）なら、その連なりの既存の地名
                                let mut best = None;
                                let mut n = cands.iter().copied().filter(|n| n.end <= before.start).max_by_key(|n| n.end);
                                while let Some(x) = n.filter(|x| chain_end(x) >= before.start) {
                                    if resource_of(entities, &x.reference).is_some() {
                                        best = Some(x.reference.clone());
                                    }
                                    n = cands.iter().copied().filter(|n| n.end <= x.start).max_by_key(|n| n.end);
                                }
                                best
                            };
                        }
                    }
                    if let Some(p) = parent.filter(|p| *p != m.reference) {
                        sub.push((m.reference.clone(), p));
                    }
                }
                Some((m, sub))
            };

        // 5. 出来事
        // (参照, 位置, 時間・場所を探し始める位置)
        let mut events: Vec<(String, usize, usize)> = vec![];
        // 5a. 主題文（`〇〇（読み）は、…で行われた戦い` `〇〇とは、…発生した地震である`）。
        // 人物・作品・施設の主題（生年月日・公開日・所在が書かれたもの）なら出来事にはしない。
        let mut subject_claims = vec![];
        if let (Some(h), Some(subject)) = (&head, subject) {
            subject_claims = self.subject_claims(&chars, &sents, h, subject, &mut b);
        }
        let topic = if subject.is_some() { None } else { topic_event(&chars, &sents) };
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
        // 5c. 報道（見出しの日付か公開日時がある文書）では、本文の最初の文を主な出来事の文とする。
        // その文に文型の出来事が無ければ、見出しを主な出来事にする。
        let news_doc = !has_topic && (header_time.is_some() || doc.published.is_some());
        let lead = sents.iter().position(|&(a, z)| z > header_end && text(&chars, a.max(header_end), z).trim().chars().count() > 5).filter(|_| news_doc);
        let mut headline_event = None;
        if let Some(li) = lead.filter(|li| !ev_marks.iter().any(|(s, _)| sent_of(*s) == *li)) {
            if let Some(title) = doc.title.as_deref().map(headline).filter(|t| (2..=80).contains(&t.chars().count())) {
                let start = sents[li].0.max(header_end);
                let r = b.add(Kind::Event, &["Event"], &title, &chars, start, start, None, None);
                headline_event = Some(r.clone());
                events.push((r, start, start));
            }
        }
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
        let mut claims = subject_claims;
        for (inner, outer, s, e) in &place_parents {
            claims.push(ClaimMention {
                subject: inner.clone(),
                predicate: "located_in".into(),
                object: ObjectMention::Ref { reference: outer.clone() },
                span: Some([*s, *e]),
                confidence: Some(0.8),
            });
        }
        let calendar = CalendarFrame::builtin(&doc.calendar()).unwrap_or_else(CalendarFrame::gregorian);
        let news_reference = doc
            .published
            .as_deref()
            .and_then(|p| Tick::parse_iso(p).ok())
            .or_else(|| header_time.as_deref().and_then(|h| resolve_range(h, None, &calendar)).map(|(a, _)| Tick(a)));
        // 見出しの日付（`【4/12開催】` `丸石広場 2024/10/14（月曜）`）
        let title_time = doc.title.as_deref().and_then(|t| {
            let tc: Vec<char> = t.chars().collect();
            find_times(&tc, 0, tc.len()).into_iter().map(|(_, _, raw)| raw).find(|raw| raw.contains(['日', '/']))
        });
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
            // 報道の最初の文の出来事は、その文の日付のうち記事の日付に最も近いもの
            // （`先月3日に始まった工事について…9日発表` の `9日`）。先の予定の日付は後回しにする。
            let is_news_lead = lead == Some(si);
            if is_news_lead {
                if let Some((t, dist)) = news_time(&find_times(&chars, body_from, se), news_reference, &calendar) {
                    chosen = Some((t, 0.8));
                    // 最初の文の日付が記事の日付から離れている（背景の出来事や先の予定）なら、
                    // 続く 2 文にある記事の日付に近い日付（`…12日に発表した`）を使う。
                    if dist > NEWS_WINDOW_DAYS {
                        let later: Vec<_> = sents.iter().skip(si + 1).take(2).flat_map(|&(a, z)| find_times(&chars, a, z)).collect();
                        if let Some((t2, _)) = news_time(&later, news_reference, &calendar).filter(|(_, d)| *d <= NEWS_WINDOW_DAYS) {
                            chosen = Some((t2, 0.6));
                        }
                    }
                }
            }
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
            // 本文の最初の文に月まで書かれた日付が無ければ、見出しの日付を使う（告知の `【4/12開催】丸石講座`）。
            // 本文の `今日の予定` `1日講座` `毎年5月…` のような月の無い・繰り返しの表現より、見出しの日付を採る。
            let explicit = |raw: &str| (raw.contains('月') || raw.contains('/')) && !raw.starts_with('毎');
            if is_news_lead && !chosen.as_ref().is_some_and(|((_, _, raw), _)| explicit(raw)) && title_time.as_deref().is_some_and(explicit) {
                if let Some(t) = &title_time {
                    chosen = Some(((usize::MAX, usize::MAX, t.clone()), 0.6));
                }
            }
            // 報道の最初の文に日付が無ければ、記事の日付までに起きたこととする（`2020年5月10日以前`）。
            // 過去のことを伝える文（`…した。`）に限る。告知（`…を開催します。`）や予定（`…へ` `…する予定`）は先のことなので付けない。
            if chosen.is_none() && is_news_lead {
                let lead_text = text(&chars, ss, se);
                let past = lead_text.trim_end().trim_end_matches(['。', '」', '）']).ends_with('た');
                let future = !past
                    || ["予定", "見通し", "方針", "見込み"].iter().any(|w| lead_text.contains(w))
                    || doc.title.as_deref().is_some_and(|t| t.trim_end().ends_with('へ'));
                let dated = header_time.clone().map(|h| (h, 0, header_end)).or_else(|| doc.published.as_deref().and_then(|p| p.get(..10)).map(|d| (d.to_string(), 0, 0)));
                if let (false, Some((d, a, z))) = (future, dated) {
                    chosen = Some(((a, z, format!("{d}以前")), 0.4));
                }
            }
            if let Some(((ts, te, mut raw), conf)) = chosen {
                // 年の無い日付は見出しの年で補う（`2019年〇〇選挙は、…4月7日に投票` → `2019年4月7日`）。
                // 報道の見出しの年は記事の日付とは限らないので、報道では本文の日付（公開日時が基準）のままにする。
                if !news_doc && TemporalExpression::strict(&raw, "gregorian").is_ok_and(|e| lacks_year(&e.ast)) {
                    if let Some(y) = title_year(&label) {
                        raw = format!("{y}年{raw}");
                    }
                }
                claims.push(ClaimMention {
                    subject: ev.clone(),
                    predicate: "occurred_at".into(),
                    object: ObjectMention::Time { time: raw, calendar: None },
                    // 見出しから採った日付は本文中の位置を持たない
                    span: (ts != usize::MAX).then_some([ts, te]),
                    confidence: Some(conf),
                });
            }
            // 主題文に場所が無ければ、段落の後続の文から探す（`…地震である。震源は山川県沖。`）。
            // 報道の最初の文の出来事も、その文に場所が無ければ続く 2 文から探す（`…が開催される。会場は山川県の丸石ホール。`）。
            let place = place_in(&b.mentions, &b.entities, si, *s, *body).or_else(|| {
                let until = if is_topic {
                    sents.len()
                } else if is_news_lead {
                    (si + 3).min(sents.len())
                } else {
                    return None;
                };
                (si + 1..until).find_map(|sj| place_in(&b.mentions, &b.entities, sj, sents[sj].0, sents[sj].0))
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
        // 見出しから作った出来事に時間も場所も付かなければ、出来事を伝える文書ではない（コラム・紹介記事など）とみなして作らない
        if let Some(r) = headline_event {
            if !claims.iter().any(|c| c.subject == r && matches!(c.predicate.as_str(), "occurred_at" | "took_place_at")) {
                b.entities.retain(|e| e.reference != r);
                b.mentions.retain(|m| m.reference != r);
                events.retain(|(e, ..)| *e != r);
                claims.retain(|c| c.subject != r);
            }
        }
        let nearest_event = |pos: usize| -> Option<String> {
            events.iter().filter(|(_, s, _)| *s <= pos).max_by_key(|(_, s, _)| *s).or(events.first()).map(|(r, ..)| r.clone())
        };
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
    /// 地名（ラベル, 型, 別名）と配下関係（子, 親）から KB を作る。同じラベルの地名は `ラベル#2` のように区別する。
    fn kb_with(places: &[(&'static str, &str, Vec<&str>)], parents: &[(&str, &str)]) -> (KnowledgeBase, HashMap<&'static str, String>) {
        use chronotope_core::model::Principal;
        let mut kb = KnowledgeBase::in_memory(chronotope_engine::KbConfig::default());
        let mut ids = HashMap::new();
        let w = |kb: &mut KnowledgeBase, body: serde_json::Value| kb.write(&Principal::curator("t"), serde_json::from_value(body).unwrap()).unwrap();
        for (key, types, aliases) in places {
            let label = key.split('#').next().unwrap();
            let r = w(&mut kb, serde_json::json!({ "op": "create_resource", "resource": { "types": [types], "label": label, "lang": "ja", "aliases": aliases } }));
            ids.insert(*key, r["id"].as_str().unwrap().to_string());
        }
        for (c, p) in parents {
            w(
                &mut kb,
                serde_json::json!({ "op": "propose_assertion", "subject": ids[c], "predicate": "located_in", "object": { "resource": ids[p] }, "status": "accepted" }),
            );
        }
        (kb, ids)
    }

    fn gazetteer_kb() -> (KnowledgeBase, HashMap<&'static str, String>) {
        kb_with(
            &[
                ("山川県", "Region", vec![]),
                ("海辺市", "City", vec![]),
                ("谷原府", "Region", vec![]),
                ("本町", "City", vec![]),
                ("湾岸都", "Region", vec!["湾岸"]),
                ("浜辺市", "City", vec!["浜辺"]),
            ],
            &[("海辺市", "山川県"), ("本町", "谷原府")],
        )
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

    fn places(x: &Extraction) -> Vec<(String, Option<String>)> {
        x.entities.iter().filter(|e| e.types != ["Event"]).map(|e| (e.label.clone(), e.resource.clone())).collect()
    }

    #[test]
    fn prefers_preferred_labels_and_document_context() {
        let (kb, ids) = kb_with(
            &[
                ("山川県", "Region", vec![]),
                ("谷原県", "Region", vec![]),
                ("海辺市", "City", vec![]),
                // 旧称として別の市の名前を持つ
                ("丸石市", "City", vec!["海辺市"]),
                ("中町", "City", vec![]),
                ("中町#2", "City", vec![]),
            ],
            &[("中町", "山川県"), ("中町#2", "谷原県")],
        );
        let x = RuleExtractor::from_kb(&kb);
        let p = place_of(&x.extract(&doc("2020年5月1日、海辺市で火災があった。")));
        assert_eq!(p.resource.as_deref(), Some(ids["海辺市"].as_str()), "an alias does not compete with a preferred label");
        // 同名の `中町` は、同じ文書の `山川県` の配下にある方
        let p = place_of(&x.extract(&doc("山川県の事件。2020年5月1日、中町で火災があった。")));
        assert_eq!(p.resource.as_deref(), Some(ids["中町"].as_str()));
        let p = place_of(&x.extract(&doc("2020年5月1日、中町で火災があった。")));
        assert_eq!(p.resource, None, "without context the homonym stays unresolved");
    }

    #[test]
    fn place_names_without_their_suffix() {
        let (kb, ids) = kb_with(&[("海辺市", "City", vec![]), ("浜辺町", "City", vec![])], &[]);
        let x = RuleExtractor::from_kb(&kb);
        let p = place_of(&x.extract(&doc("2020年5月1日、海辺近傍で火災があった。")));
        assert_eq!((p.label.as_str(), p.resource.as_deref()), ("海辺市", Some(ids["海辺市"].as_str())));
        for t in ["2020年5月1日、丸海辺で火災があった。", "2020年5月1日、海辺の戦いがあった。", "2020年5月1日、海辺時代の遺跡で火災があった。"] {
            assert!(places(&x.extract(&doc(t))).iter().all(|(_, r)| r.as_deref() != Some(ids["海辺市"].as_str())), "{t}");
        }
        // 長い町名の末尾（`丸石浜辺町` の `浜辺町`）は既存の町にしない
        let got = places(&x.extract(&doc("2020年5月1日、丸石浜辺町で火災があった。")));
        assert!(got.iter().all(|(_, r)| r.as_deref() != Some(ids["浜辺町"].as_str())), "{got:?}");
    }

    #[test]
    fn mayor_suffix_after_a_place_is_not_a_person() {
        let (kb, _) = gazetteer_kb();
        let x = RuleExtractor::from_kb(&kb).extract(&doc("2020年5月1日、山川県海辺市長浜付近で地震があった。"));
        assert!(x.entities.iter().all(|e| e.types != ["Person"]), "{:?}", x.entities);
        assert_eq!(place_of(&x).label, "海辺市");
    }

    #[test]
    fn parenthesized_places_become_parents() {
        let (kb, ids) = gazetteer_kb();
        let rx = RuleExtractor::from_kb(&kb);
        // 括弧内の地名に `で` などが続く場合はそちらを出来事の場所とするので、ここでは続かない書き方にする
        for t in ["丸石号事故は、2020年5月1日に丸石空港（山川県海辺市）に着陸しようとした機体の事故である。", "2020年5月1日、現・山川県海辺市の丸石空港で事故が起きた。"] {
            let x = rx.extract(&doc(t));
            let p = place_of(&x);
            assert_eq!(p.label, "丸石空港", "{t}");
            let parent = x.claims.iter().find(|c| c.predicate == "located_in" && c.subject == p.reference).expect(t);
            let ObjectMention::Ref { reference } = &parent.object else { panic!() };
            let e = x.entities.iter().find(|e| &e.reference == reference).unwrap();
            assert_eq!(e.resource.as_deref(), Some(ids["海辺市"].as_str()), "{t}");
        }
    }

    #[test]
    fn topic_sentences_without_a_comma() {
        let x = RuleExtractor::new(Gazetteer::default());
        for (t, label, time) in [
            ("丸石峠の戦い（まるいしとうげのたたかい）は戦国時代の1560年5月1日に行われた合戦。", "丸石峠の戦い", "1560年5月1日"),
            ("2020年海辺市長選挙は2020年4月5日に執行された海辺市の市長選挙である。", "2020年海辺市長選挙", "2020年4月5日"),
            ("丸石原の戦（まるいしはら の いくさ）とは、1530年7月6日に起きた戦。", "丸石原の戦", "1530年7月6日"),
            ("丸石屋遭難（まるいしやそうなん）は、1866年3月9日に宿泊客が襲撃された事件。", "丸石屋遭難", "1866年3月9日"),
        ] {
            let x = x.extract(&doc(t));
            let ev = x.entities.iter().find(|e| e.types == ["Event"]).unwrap_or_else(|| panic!("{t}"));
            assert_eq!(ev.label, label);
            let occ = x.claims.iter().find(|c| c.subject == ev.reference && c.predicate == "occurred_at").unwrap_or_else(|| panic!("{t}"));
            assert!(matches!(&occ.object, ObjectMention::Time { time: tt, .. } if tt == time), "{t}: {:?}", occ.object);
        }
    }

    fn claim<'a>(x: &'a Extraction, subject: &str, pred: &str) -> Option<&'a ObjectMention> {
        x.claims.iter().find(|c| c.subject == subject && c.predicate == pred).map(|c| &c.object)
    }

    fn time_of(o: Option<&ObjectMention>) -> Option<String> {
        match o {
            Some(ObjectMention::Time { time, .. }) => Some(time.clone()),
            _ => None,
        }
    }

    fn label_of(x: &Extraction, o: Option<&ObjectMention>) -> Option<String> {
        match o {
            Some(ObjectMention::Ref { reference }) => x.entities.iter().find(|e| &e.reference == reference).map(|e| e.label.clone()),
            _ => None,
        }
    }

    #[test]
    fn news_headline_is_the_main_event() {
        let (kb, _) = gazetteer_kb();
        let rx = RuleExtractor::from_kb(&kb);
        let d: Document = serde_json::from_value(serde_json::json!({
            "title": "【速報】丸石祭の来場者が過去最多に",
            "text": "【2020年5月10日】 先月3日に始まった改修工事が終わり、9日、丸石祭が開かれた。7月20日には次回の日程も発表される。会場は山川県海辺市の丸石公園。"
        }))
        .unwrap();
        let x = rx.extract(&d);
        // 最初の文に文型の出来事（`丸石祭が開かれ`）があればそれを使う
        let ev = x.entities.iter().find(|e| e.types == ["Event"]).unwrap();
        assert_eq!(ev.label, "丸石祭");
        // 記事の日付に最も近い日付（背景の `先月3日` や先の `7月20日` ではない）
        assert_eq!(time_of(claim(&x, &ev.reference, "occurred_at")).as_deref(), Some("9日"));
        // 最初の文に場所が無ければ続く文から
        assert_eq!(label_of(&x, claim(&x, &ev.reference, "took_place_at")).as_deref(), Some("丸石公園"));

        // 文型の出来事が無ければ見出し（角括弧の見出し語を除く）を主な出来事にする
        let d: Document = serde_json::from_value(serde_json::json!({
            "title": "【山川県】丸石祭が中止に",
            "published": "2020-05-10T09:00:00+09:00",
            "text": "山川県海辺市は9日、丸石祭の中止を発表した。"
        }))
        .unwrap();
        let x = rx.extract(&d);
        let ev = x.entities.iter().find(|e| e.types == ["Event"]).unwrap();
        assert_eq!(ev.label, "丸石祭が中止に");
        assert_eq!(time_of(claim(&x, &ev.reference, "occurred_at")).as_deref(), Some("9日"));
        assert_eq!(label_of(&x, claim(&x, &ev.reference, "took_place_at")).as_deref(), Some("海辺市"));
    }

    #[test]
    fn person_work_and_facility_topics() {
        let (kb, _) = gazetteer_kb();
        let rx = RuleExtractor::from_kb(&kb);
        let x = rx.extract(&doc("山田 花子（やまだ はなこ、1950年〈昭和25年〉1月2日 - 2020年3月4日）は、日本の俳優。山川県出身（海辺市生まれ）。"));
        let p = x.entities.iter().find(|e| e.types == ["Person"]).unwrap();
        assert_eq!(p.label, "山田 花子");
        assert_eq!(time_of(claim(&x, &p.reference, "birth_date")).as_deref(), Some("1950年1月2日"));
        assert_eq!(time_of(claim(&x, &p.reference, "death_date")).as_deref(), Some("2020年3月4日"));
        // 出身地より出生地
        assert_eq!(label_of(&x, claim(&x, &p.reference, "birth_place")).as_deref(), Some("海辺市"));

        for (t, time) in [
            ("『丸石の夏、海辺の秋』（まるいしのなつ）は、2001年7月1日に山川映画の配給で公開された日本映画。", "2001年7月1日"),
            ("『丸石大戦争』は、1985年製作の日本映画。", "1985年"),
        ] {
            let x = rx.extract(&doc(t));
            let w = x.entities.iter().find(|e| e.types == ["Movie"]).unwrap_or_else(|| panic!("{t}: {:?}", x.entities));
            assert_eq!(time_of(claim(&x, &w.reference, "publication_date")).as_deref(), Some(time), "{t}");
        }

        let x = rx.extract(&doc("丸石駅（まるいしえき）は、山川県海辺市本町にある、丸石鉄道の駅である。"));
        let st = x.entities.iter().find(|e| e.types == ["Station"]).unwrap();
        assert_eq!(st.label, "丸石駅");
        // `本町` は別の府の町なので、所在は海辺市
        assert_eq!(label_of(&x, claim(&x, &st.reference, "located_in")).as_deref(), Some("海辺市"));
    }

    #[test]
    fn fictional_topics_and_brackets() {
        let x = RuleExtractor::new(Gazetteer::default());
        let e = x.extract(&doc("丸石ヌイン（Maruishi Nuin）は、小説『星の丸石』における丸石戦争の第二の合戦である。"));
        assert!(e.entities.iter().any(|e| e.types == ["Event"] && e.label == "丸石ヌイン"), "{:?}", e.entities);
        // 題名の中の `!` `。` では文を区切らない
        let e = x.extract(&doc("『止まるな!丸石。』（とまるな）は、2022年に公開された日本映画。"));
        assert!(e.entities.iter().any(|e| e.label == "止まるな!丸石。"), "{:?}", e.entities);
    }

    #[test]
    fn time_zones_and_words_are_not_places() {
        let (kb, _) = kb_with(&[("海辺国", "Country", vec!["マリ"]), ("湾岸国", "Country", vec![])], &[]);
        let x = RuleExtractor::from_kb(&kb).extract(&doc("2020年5月1日、湾岸時間の午後、マリンバ奏者が講演した。"));
        assert!(x.entities.iter().all(|e| e.types != ["Place"] && !e.types.contains(&"Country".to_string())), "{:?}", x.entities);
    }

    #[test]
    fn organization_names_and_anaphora() {
        let (kb, ids) = gazetteer_kb();
        let rx = RuleExtractor::from_kb(&kb);
        // `山川県新聞` `山川県人` の地名は場所にしない
        let x = rx.extract(&doc("山川県新聞によると、2020年5月1日、山川県人の男性が表彰された。"));
        assert!(x.entities.iter().all(|e| e.resource.as_deref() != Some(ids["山川県"].as_str())), "{:?}", x.entities);
        // `同市` は前に出てきた市
        let x = rx.extract(&doc("海辺市の職員によると、2020年5月1日、同市で会議が開かれた。"));
        let ev = x.entities.iter().find(|e| e.types == ["Event"]).unwrap();
        assert_eq!(label_of(&x, claim(&x, &ev.reference, "took_place_at")).as_deref(), Some("海辺市"));
    }

    #[test]
    fn news_dates_near_the_article() {
        let rx = RuleExtractor::new(Gazetteer::default());
        let news = |title: &str, text: &str| -> Extraction {
            rx.extract(&serde_json::from_value(serde_json::json!({ "title": title, "text": text })).unwrap())
        };
        // 最初の文の日付が古い出来事なら、続く文の記事の日付に近い日付
        let x = news("丸石氏の死去が判明", "【2020年3月13日】 丸石氏が2019年4月5日に死去していたことが分かった。丸石社が3月12日に発表した。");
        let ev = x.entities.iter().find(|e| e.types == ["Event"]).unwrap();
        assert_eq!(time_of(claim(&x, &ev.reference, "occurred_at")).as_deref(), Some("3月12日"));
        // 日付が無ければ記事の日付以前
        let x = news("丸石社が新製品", "【2020年3月13日】 丸石社は新しい製品を発表した。");
        let ev = x.entities.iter().find(|e| e.types == ["Event"]).unwrap();
        assert_eq!(time_of(claim(&x, &ev.reference, "occurred_at")).as_deref(), Some("2020年3月13日以前"));
        // 予定を伝える記事・過去形でない文には付けず、時間も場所も無い見出しは出来事にしない
        for (title, text) in [
            ("丸石社が新製品発売へ", "【2020年3月13日】 丸石社は新しい製品を発売する予定だ。"),
            ("丸石の魅力とは", "【2020年3月13日】 丸石の魅力について考えてみたい。"),
        ] {
            let x = news(title, text);
            assert!(x.entities.iter().all(|e| e.types != ["Event"]), "{title}: {:?}", x.entities);
        }
        // 告知: 本文に日付が無ければ見出しの `月/日`（本文の `今日` より優先）
        let x = news("【4/12開催】丸石講座", "【2020年3月13日】 10：00～今日の予定、丸石の話を聞きます。");
        let ev = x.entities.iter().find(|e| e.types == ["Event"]).unwrap();
        assert_eq!(time_of(claim(&x, &ev.reference, "occurred_at")).as_deref(), Some("4/12"));
    }

    #[test]
    fn suffix_places_that_are_not_places() {
        let x = RuleExtractor::new(Gazetteer::default());
        for (t, bad) in [
            ("2020年5月1日、丸石ホールディングスが会見した。", "丸石ホール"),
            ("2020年5月1日、丸石前市長が会見した。", "丸石前市"),
            ("2020年5月1日、丸石田園都市線丸石駅で事故が起きた。", "丸石田園都"),
        ] {
            let e = x.extract(&doc(t));
            assert!(e.entities.iter().all(|e| e.label != bad), "{t}: {:?}", e.entities);
        }
    }

    #[test]
    fn organizations_facilities_and_death_places() {
        let (kb, _) = gazetteer_kb();
        let rx = RuleExtractor::from_kb(&kb);
        for (t, label) in [
            ("丸石株式会社（まるいし）は、山川県海辺市に本社を置く日本の企業。1950年に設立された。", "丸石株式会社"),
            ("山川県立丸石高等学校は、山川県海辺市にある公立高等学校。1950年に開校した。", "山川県立丸石高等学校"),
        ] {
            let x = rx.extract(&doc(t));
            let o = x.entities.iter().find(|e| e.types == ["Organization"]).unwrap_or_else(|| panic!("{t}: {:?}", x.entities));
            assert_eq!(o.label, label);
            assert_eq!(time_of(claim(&x, &o.reference, "inception")).as_deref(), Some("1950年"), "{t}");
            assert_eq!(label_of(&x, claim(&x, &o.reference, "located_in")).as_deref(), Some("海辺市"), "{t}");
        }
        let x = rx.extract(&doc("丸石大社（まるいしたいしゃ）は、山川県海辺市にある神社。"));
        let f = x.entities.iter().find(|e| e.types == ["Building"]).unwrap();
        assert_eq!(label_of(&x, claim(&x, &f.reference, "located_in")).as_deref(), Some("海辺市"));
        let x = rx.extract(&doc("山田 太郎（やまだ たろう、1900年1月2日 - 1980年3月4日）は、日本の俳優。山川県海辺市の病院で死去した。"));
        let p = x.entities.iter().find(|e| e.types == ["Person"]).unwrap();
        assert_eq!(label_of(&x, claim(&x, &p.reference, "death_place")).as_deref(), Some("海辺市"));
    }
}
