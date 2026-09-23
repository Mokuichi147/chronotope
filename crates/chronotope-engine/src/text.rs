//! ラベル・別名の全文索引用トークナイザ。
//! ASCII は小文字化した単語、CJK などは文字 bigram（1 文字語は unigram）を出す。

use roaring::RoaringBitmap;
use std::collections::HashMap;

pub fn normalize_label(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            '\u{3000}' => ' ',
            c => c,
        })
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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

/// トークン → 文書集合の転置索引。
#[derive(Default)]
pub struct TextIndex {
    postings: HashMap<String, RoaringBitmap>,
    doc_tokens: HashMap<u32, Vec<String>>,
}

impl TextIndex {
    pub fn set(&mut self, doc: u32, texts: &[&str]) {
        self.remove(doc);
        let mut toks: Vec<String> = texts.iter().flat_map(|t| tokenize(t)).collect();
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
        let toks = tokenize(query);
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
}
