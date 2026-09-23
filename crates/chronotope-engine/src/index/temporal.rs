//! 区間索引（階層バケット方式）。
//!
//! 各区間 `[es, le)` を「幅 2^L のバケット 2 つ以内に収まる最小のレベル L」に登録し、
//! バケット番号 `es >> L` をキーに BTreeMap へ格納する。問い合わせ `[qs, qe)` は各レベルで
//! バケット `(qs >> L) - 1 ..= (qe >> L)` を範囲走査すればよく、更新も O(log n) で行える。
//! PostgreSQL 側では同じ役割を int8range + GiST が担う。

use chronotope_core::time::Tick;
use roaring::RoaringBitmap;
use std::collections::{BTreeMap, HashMap};

const LEVEL_STEP: u32 = 4;
const LEVELS: usize = (64 / LEVEL_STEP) as usize + 1;

#[derive(Default)]
pub struct TemporalIndex {
    levels: Vec<BTreeMap<i64, Vec<u32>>>,
    entries: HashMap<u32, (usize, i64, i64, i64)>,
}

fn level_of(es: i64, le: i64) -> usize {
    for l in 0..LEVELS - 1 {
        let shift = l as u32 * LEVEL_STEP;
        if (le >> shift) - (es >> shift) <= 1 {
            return l;
        }
    }
    LEVELS - 1
}

impl TemporalIndex {
    pub fn new() -> Self {
        TemporalIndex { levels: (0..LEVELS).map(|_| BTreeMap::new()).collect(), entries: HashMap::new() }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn insert(&mut self, doc: u32, es: Tick, le: Tick) {
        self.remove(doc);
        let (es, le) = (es.0, le.0.max(es.0));
        let l = if es == i64::MIN || le == i64::MAX { LEVELS - 1 } else { level_of(es, le) };
        let key = if l == LEVELS - 1 { 0 } else { es >> (l as u32 * LEVEL_STEP) };
        self.levels[l].entry(key).or_default().push(doc);
        self.entries.insert(doc, (l, key, es, le));
    }

    pub fn remove(&mut self, doc: u32) {
        if let Some((l, key, _, _)) = self.entries.remove(&doc) {
            if let Some(v) = self.levels[l].get_mut(&key) {
                v.retain(|d| *d != doc);
                if v.is_empty() {
                    self.levels[l].remove(&key);
                }
            }
        }
    }

    /// `[qs, qe)` と可能区間が重なる文書。
    pub fn overlapping(&self, qs: Tick, qe: Tick) -> RoaringBitmap {
        let mut out = RoaringBitmap::new();
        let (qs, qe) = (qs.0, qe.0);
        for (l, level) in self.levels.iter().enumerate() {
            if level.is_empty() {
                continue;
            }
            let range: Box<dyn Iterator<Item = (&i64, &Vec<u32>)>> = if l == LEVELS - 1 {
                Box::new(level.iter())
            } else {
                let shift = l as u32 * LEVEL_STEP;
                let lo = (qs >> shift).saturating_sub(1);
                let hi = qe.saturating_sub(1) >> shift;
                if lo > hi {
                    continue;
                }
                Box::new(level.range(lo..=hi))
            };
            for (_, docs) in range {
                for d in docs {
                    let (_, _, es, le) = self.entries[d];
                    if es < qe && le > qs {
                        out.insert(*d);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_bruteforce() {
        let mut idx = TemporalIndex::new();
        let mut iv = vec![];
        let mut seed = 7u64;
        let mut rnd = |m: i64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as i64) % m
        };
        for d in 0..2000u32 {
            let s = rnd(1_000_000) - 500_000;
            let w = match d % 4 {
                0 => rnd(10),
                1 => rnd(1000),
                2 => rnd(100_000),
                _ => rnd(2_000_000),
            } + 1;
            iv.push((d, s, s + w));
            idx.insert(d, Tick(s), Tick(s + w));
        }
        idx.insert(5000, Tick::NEG_INF, Tick(0));
        iv.push((5000, i64::MIN, 0));
        for _ in 0..200 {
            let qs = rnd(1_200_000) - 600_000;
            let qe = qs + rnd(50_000) + 1;
            let got = idx.overlapping(Tick(qs), Tick(qe));
            let want: RoaringBitmap = iv.iter().filter(|(_, s, e)| *s < qe && *e > qs).map(|(d, _, _)| *d).collect();
            assert_eq!(got, want);
        }
        idx.remove(3);
        assert!(!idx.overlapping(Tick(i64::MIN + 1), Tick(i64::MAX - 1)).contains(3));
    }
}
