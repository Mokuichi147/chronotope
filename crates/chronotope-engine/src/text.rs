//! ラベル・別名・会話本文の全文索引用トークナイザ。
//! ASCII は小文字化した単語、CJK などは文字 bigram（1 文字語は unigram）を出す。

use roaring::RoaringBitmap;
use std::collections::HashMap;

pub fn normalize_label(s: &str) -> String {
    s.chars().map(fold_width).flat_map(char::to_lowercase).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fold_width(c: char) -> char {
    match c {
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        '\u{3000}' => ' ',
        c => c,
    }
}

/// [`normalize_label`] と同じ正規化をした文字列と、正規化後の各文字の元の位置。
/// 正規化した文字列で照合し、結果を原文のバイト範囲で返すために使う。
pub struct MappedText {
    pub text: String,
    /// (正規化後のバイト位置, 原文の開始バイト, 原文の終了バイト)。
    map: Vec<(usize, usize, usize)>,
}

impl MappedText {
    pub fn new(raw: &str) -> Self {
        let mut text = String::new();
        let mut map = Vec::new();
        let mut space: Option<(usize, usize)> = None;
        for (i, c) in raw.char_indices() {
            let end = i + c.len_utf8();
            let c = fold_width(c);
            if c.is_whitespace() {
                if !text.is_empty() {
                    space = Some(space.map_or((i, end), |(s, _)| (s, end)));
                }
                continue;
            }
            if let Some((s, e)) = space.take() {
                map.push((text.len(), s, e));
                text.push(' ');
            }
            for lc in c.to_lowercase() {
                map.push((text.len(), i, end));
                text.push(lc);
            }
        }
        MappedText { text, map }
    }

    /// 正規化後のバイト範囲 → 原文のバイト範囲。
    pub fn raw_span(&self, start: usize, end: usize) -> Option<(usize, usize)> {
        let first = self.map.partition_point(|m| m.0 < start);
        let last = self.map.partition_point(|m| m.0 < end).checked_sub(1)?;
        Some((self.map.get(first)?.1, self.map.get(last)?.2))
    }

    /// 正規化した語をすべて含むか。最初の語の最初の出現位置（原文のバイト範囲）を返す。
    pub fn find_all(&self, terms: &[String]) -> Option<(usize, usize)> {
        let mut first = None;
        for t in terms {
            let at = self.text.find(t.as_str())?;
            if first.is_none() {
                first = self.raw_span(at, at + t.len());
            }
        }
        first
    }
}

/// 検索語を正規化して空白で分ける。
pub fn query_terms(q: &str) -> Vec<String> {
    normalize_label(q).split(' ').filter(|t| !t.is_empty()).map(str::to_string).collect()
}

pub fn tokenize(s: &str) -> Vec<String> {
    let s = normalize_label(s);
    let mut out = Vec::new();
    let mut word = String::new();
    let mut cjk: Vec<char> = Vec::new();
    let flush_cjk = |cjk: &mut Vec<char>, out: &mut Vec<String>| {
        match cjk.len() {
            0 => {}
            1 => out.push(cjk[0].to_string()),
            _ => {
                for w in cjk.windows(2) {
                    out.push(w.iter().collect());
                }
            }
        }
        cjk.clear();
    };
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            flush_cjk(&mut cjk, &mut out);
            word.push(c);
        } else if c.is_alphanumeric() {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            cjk.push(c);
        } else {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            flush_cjk(&mut cjk, &mut out);
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    flush_cjk(&mut cjk, &mut out);
    out
}

/// 本文の部分一致検索用のトークン。正規化した文字列の英数字の並びごとに文字 bigram
/// （1 文字だけの並びは unigram）を出す。ASCII も単語ではなく bigram にするので、
/// 語の途中から始まる検索語も取りこぼさない。
pub fn gram_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for run in normalize_label(s).split(|c: char| !c.is_alphanumeric()).filter(|r| !r.is_empty()) {
        let chars: Vec<char> = run.chars().collect();
        if chars.len() == 1 {
            out.push(run.to_string());
        }
        for w in chars.windows(2) {
            out.push(w.iter().collect());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 検索語が部分一致する本文なら必ず持つトークン（[`gram_tokens`] の部分集合）。
/// 1 文字の並びは本文側で bigram に含まれるため、条件にしない。
pub fn gram_query_tokens(term: &str) -> Vec<String> {
    let mut out = Vec::new();
    for run in normalize_label(term).split(|c: char| !c.is_alphanumeric()) {
        let chars: Vec<char> = run.chars().collect();
        for w in chars.windows(2) {
            out.push(w.iter().collect());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// トークン → 文書集合の転置索引。
#[derive(Default)]
pub struct TextIndex {
    postings: HashMap<String, RoaringBitmap>,
    doc_tokens: HashMap<u32, Vec<String>>,
}

impl TextIndex {
    pub fn set(&mut self, doc: u32, texts: &[&str]) {
        let toks: Vec<String> = texts.iter().flat_map(|t| tokenize(t)).collect();
        self.set_tokens(doc, toks);
    }

    pub fn set_tokens(&mut self, doc: u32, mut toks: Vec<String>) {
        self.remove(doc);
        toks.sort();
        toks.dedup();
        for t in &toks {
            self.postings.entry(t.clone()).or_default().insert(doc);
        }
        self.doc_tokens.insert(doc, toks);
    }

    pub fn remove(&mut self, doc: u32) {
        if let Some(toks) = self.doc_tokens.remove(&doc) {
            for t in toks {
                if let Some(p) = self.postings.get_mut(&t) {
                    p.remove(doc);
                }
            }
        }
    }

    /// 全トークンを含む文書（AND）。トークンが無ければ None。
    pub fn search(&self, query: &str) -> Option<RoaringBitmap> {
        self.search_tokens(tokenize(query))
    }

    pub fn search_tokens(&self, toks: Vec<String>) -> Option<RoaringBitmap> {
        if toks.is_empty() {
            return None;
        }
        let mut acc: Option<RoaringBitmap> = None;
        for t in toks {
            let p = self.postings.get(&t).cloned().unwrap_or_default();
            acc = Some(match acc {
                None => p,
                Some(a) => a & p,
            });
        }
        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_mixed_text() {
        assert_eq!(tokenize("Tokyo Tower"), vec!["tokyo", "tower"]);
        assert_eq!(tokenize("東京タワー"), vec!["東京", "京タ", "タワ", "ワー"]);
        assert_eq!(tokenize("ＡＢＣ駅"), vec!["abc", "駅"]);
    }

    #[test]
    fn mapped_text_matches_normalize_label_and_maps_back() {
        let raw = "  前回の  ＴＯＫＹＯ　Tower は\n\t良かった ";
        let m = MappedText::new(raw);
        assert_eq!(m.text, normalize_label(raw));
        let (s, e) = m.find_all(&query_terms("tokyo tower")).unwrap();
        assert_eq!(&raw[s..e], "ＴＯＫＹＯ");
        let at = m.text.find("tokyo tower").unwrap();
        let (s, e) = m.raw_span(at, at + "tokyo tower".len()).unwrap();
        assert_eq!(&raw[s..e], "ＴＯＫＹＯ　Tower");
        assert!(m.find_all(&query_terms("tokyo 京都")).is_none());
    }

    #[test]
    fn gram_tokens_cover_substrings() {
        let doc = gram_tokens("東京タワーへ行く。Deploy は mainline で");
        for q in ["京タワ", "ploy", "Mainl", "へ行く", "東京タワーへ"] {
            let need = gram_query_tokens(q);
            assert!(!need.is_empty());
            assert!(need.iter().all(|t| doc.contains(t)), "{q}");
        }
        assert!(gram_query_tokens("京").is_empty());
        assert!(gram_tokens("a b").contains(&"a".to_string()));
    }
}
