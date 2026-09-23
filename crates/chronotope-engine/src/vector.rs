//! Vector Search。Embedding 空間（モデル・版・次元）ごとに独立した索引を持ち、
//! Binary（符号ビット）で候補を絞り → half precision で再順位付け → float32 で最終順位付けする。
//! 量子化方式や次元は固定仕様にせず、空間ごとに差し替え可能。

use chronotope_core::time::Tick;
use chronotope_core::{Error, ResourceId, Result};
use half::f16;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingSpace {
    /// `<embedding_model_id>@<embedding_version>`
    pub key: String,
    pub embedding_model_id: String,
    pub embedding_version: String,
    pub dimension: usize,
}

impl EmbeddingSpace {
    pub fn new(model: &str, version: &str, dimension: usize) -> Self {
        EmbeddingSpace { key: format!("{model}@{version}"), embedding_model_id: model.into(), embedding_version: version.into(), dimension }
    }
}

pub struct VectorIndex {
    pub space: EmbeddingSpace,
    ids: Vec<ResourceId>,
    pos: HashMap<ResourceId, usize>,
    live: Vec<bool>,
    words: usize,
    bits: Vec<u64>,
    halfs: Vec<f16>,
    floats: Vec<f32>,
    generated_at: Vec<Tick>,
}

#[derive(PartialEq)]
struct Cand(f32, usize);
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Cand {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.partial_cmp(&o.0).unwrap_or(Ordering::Equal).then(self.1.cmp(&o.1))
    }
}

fn normalize(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n == 0.0 { v.to_vec() } else { v.iter().map(|x| x / n).collect() }
}

impl VectorIndex {
    pub fn new(space: EmbeddingSpace) -> Self {
        let words = space.dimension.div_ceil(64);
        VectorIndex { space, ids: vec![], pos: HashMap::new(), live: vec![], words, bits: vec![], halfs: vec![], floats: vec![], generated_at: vec![] }
    }

    pub fn len(&self) -> usize {
        self.pos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pos.is_empty()
    }

    pub fn contains(&self, id: &ResourceId) -> bool {
        self.pos.contains_key(id)
    }

    pub fn generated_at(&self, id: &ResourceId) -> Option<Tick> {
        self.pos.get(id).map(|&i| self.generated_at[i])
    }

    pub fn upsert(&mut self, id: ResourceId, v: &[f32], at: Tick) -> Result<()> {
        let d = self.space.dimension;
        if v.len() != d {
            return Err(Error::invalid(format!("embedding dimension {} != {} for space {}", v.len(), d, self.space.key)));
        }
        let v = normalize(v);
        let i = match self.pos.get(&id) {
            Some(&i) => i,
            None => {
                let i = self.ids.len();
                self.ids.push(id);
                self.live.push(true);
                self.bits.extend(std::iter::repeat_n(0, self.words));
                self.halfs.extend(std::iter::repeat_n(f16::ZERO, d));
                self.floats.extend(std::iter::repeat_n(0.0, d));
                self.generated_at.push(at);
                self.pos.insert(id, i);
                i
            }
        };
        self.live[i] = true;
        self.generated_at[i] = at;
        let b = &mut self.bits[i * self.words..(i + 1) * self.words];
        b.iter_mut().for_each(|w| *w = 0);
        for (k, x) in v.iter().enumerate() {
            if *x > 0.0 {
                b[k / 64] |= 1 << (k % 64);
            }
        }
        for (k, x) in v.iter().enumerate() {
            self.halfs[i * d + k] = f16::from_f32(*x);
            self.floats[i * d + k] = *x;
        }
        Ok(())
    }

    pub fn remove(&mut self, id: &ResourceId) {
        if let Some(i) = self.pos.remove(id) {
            self.live[i] = false;
        }
    }

    pub fn vector(&self, id: &ResourceId) -> Option<&[f32]> {
        let d = self.space.dimension;
        self.pos.get(id).map(|&i| &self.floats[i * d..(i + 1) * d])
    }

    /// 3 段階検索。`allow` で構造化フィルタを適用する（None なら全件）。
    /// `deadline` を超えたら途中結果を返し、2 番目の値が true になる。
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        allow: Option<&dyn Fn(ResourceId) -> bool>,
        deadline: &dyn Fn() -> bool,
    ) -> Result<(Vec<(ResourceId, f32)>, bool)> {
        let d = self.space.dimension;
        if query.len() != d {
            return Err(Error::invalid(format!("query dimension {} != {}", query.len(), d)));
        }
        let q = normalize(query);
        let mut qbits = vec![0u64; self.words];
        for (i, x) in q.iter().enumerate() {
            if *x > 0.0 {
                qbits[i / 64] |= 1 << (i % 64);
            }
        }
        let stage1 = (k * 16).max(64);
        let stage2 = (k * 4).max(16);
        let mut truncated = false;
        // Stage 1: ハミング距離（小さいほど近い）で上位 stage1 件。
        let mut heap: BinaryHeap<(u32, usize)> = BinaryHeap::new();
        for i in 0..self.ids.len() {
            if i % 4096 == 0 && deadline() {
                truncated = true;
                break;
            }
            if !self.live[i] || allow.is_some_and(|f| !f(self.ids[i])) {
                continue;
            }
            let b = &self.bits[i * self.words..(i + 1) * self.words];
            let dist: u32 = b.iter().zip(&qbits).map(|(a, c)| (a ^ c).count_ones()).sum();
            if heap.len() < stage1 {
                heap.push((dist, i));
            } else if let Some(&(worst, _)) = heap.peek() {
                if dist < worst {
                    heap.pop();
                    heap.push((dist, i));
                }
            }
        }
        // Stage 2: half precision の内積で上位 stage2 件。
        let mut c2: Vec<Cand> = heap
            .into_iter()
            .map(|(_, i)| {
                let s: f32 = self.halfs[i * d..(i + 1) * d].iter().zip(&q).map(|(h, x)| h.to_f32() * x).sum();
                Cand(s, i)
            })
            .collect();
        c2.sort_by(|a, b| b.cmp(a));
        c2.truncate(stage2);
        // Stage 3: float32 の内積で最終順位。
        let mut c3: Vec<Cand> = c2.into_iter().map(|Cand(_, i)| Cand(self.floats[i * d..(i + 1) * d].iter().zip(&q).map(|(a, b)| a * b).sum(), i)).collect();
        c3.sort_by(|a, b| b.cmp(a));
        c3.truncate(k);
        Ok((c3.into_iter().map(|Cand(s, i)| (self.ids[i], s)).collect(), truncated))
    }

    /// 候補集合が小さい場合の厳密計算（pre-filter）。
    pub fn exact(&self, query: &[f32], candidates: &[ResourceId], k: usize) -> Vec<(ResourceId, f32)> {
        let q = normalize(query);
        let mut v: Vec<(ResourceId, f32)> =
            candidates.iter().filter_map(|id| self.vector(id).map(|x| (*id, x.iter().zip(&q).map(|(a, b)| a * b).sum()))).collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        v.truncate(k);
        v
    }
}

/// 全 Embedding 空間の索引。
#[derive(Default)]
pub struct VectorStore {
    pub spaces: HashMap<String, VectorIndex>,
}

impl VectorStore {
    pub fn define(&mut self, space: EmbeddingSpace) {
        self.spaces.entry(space.key.clone()).or_insert_with(|| VectorIndex::new(space));
    }
    pub fn get(&self, key: &str) -> Option<&VectorIndex> {
        self.spaces.get(key)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut VectorIndex> {
        self.spaces.get_mut(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_stage_search_finds_nearest() {
        let mut idx = VectorIndex::new(EmbeddingSpace::new("t", "1", 32));
        let mut ids = vec![];
        let mut seed = 1u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 2000) as f32 / 1000.0 - 1.0
        };
        for _ in 0..500 {
            let id = ResourceId::new();
            let v: Vec<f32> = (0..32).map(|_| rnd()).collect();
            idx.upsert(id, &v, Tick(0)).unwrap();
            ids.push((id, v));
        }
        let (target, tv) = ids[123].clone();
        let (res, truncated) = idx.search(&tv, 5, None, &|| false).unwrap();
        assert!(!truncated);
        assert_eq!(res[0].0, target);
        assert!((res[0].1 - 1.0).abs() < 1e-4);
        let (res, _) = idx.search(&tv, 5, Some(&|id| id != target), &|| false).unwrap();
        assert_ne!(res[0].0, target);
    }
}
