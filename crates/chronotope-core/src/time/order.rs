//! C. Temporal Order Label — 絶対時刻にアンカーできない Event の部分順序アクセラレータ。
//!
//! 各 Event を開始点・終了点の 2 点で表し、Allen 関係を点の順序制約（`<` と `=`）へ分解して
//! 有向グラフに載せる。`<` 辺は Pearce–Kelly の増分トポロジカル順序で維持し、
//! 順序番号 `ord` を使って到達可能性探索を枝刈りする（`ord(x) ≥ ord(y)` なら x→y の経路は無い）。
//! `=` は union-find で点を併合する（併合時のみ全体を再整列）。
//!
//! これは Canonical な事実ではなく派生物であり、Source of Truth は Canonical 側の Temporal Constraint
//! （時間関係 Assertion）である。制約の撤回時は Engine が再構築する。

use super::allen::{ALL_RELATIONS, AllenRelation, AllenSet};
use crate::{Error, ResourceId, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// 到達可能性探索で訪問するノード数の上限。超えた場合は「不明」を返す（誤答はしない）。
pub const DEFAULT_SEARCH_BUDGET: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderLabel {
    /// 開始点のトポロジカル順序番号（線形拡張の一つ。値そのものに意味は無く比較専用）。
    pub start_ord: u64,
    pub end_ord: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pc {
    Lt,
    Le,
}

#[derive(Debug, Clone, Default)]
pub struct TemporalOrderGraph {
    events: HashMap<ResourceId, (u32, u32)>,
    parent: Vec<u32>,
    succ: Vec<Vec<u32>>,
    pred: Vec<Vec<u32>>,
    ord: Vec<u64>,
    next_ord: u64,
    budget: usize,
}

impl TemporalOrderGraph {
    pub fn new() -> Self {
        TemporalOrderGraph { budget: DEFAULT_SEARCH_BUDGET, ..Default::default() }
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    pub fn contains(&self, id: &ResourceId) -> bool {
        self.events.contains_key(id)
    }

    fn new_point(&mut self) -> u32 {
        let i = self.parent.len() as u32;
        self.parent.push(i);
        self.succ.push(Vec::new());
        self.pred.push(Vec::new());
        self.ord.push(self.next_ord);
        self.next_ord += 1;
        i
    }

    fn ensure_event(&mut self, id: ResourceId) -> (u32, u32) {
        if let Some(p) = self.events.get(&id) {
            return *p;
        }
        let s = self.new_point();
        let e = self.new_point();
        self.succ[s as usize].push(e);
        self.pred[e as usize].push(s);
        self.events.insert(id, (s, e));
        (s, e)
    }

    fn find(&self, mut x: u32) -> u32 {
        while self.parent[x as usize] != x {
            x = self.parent[x as usize];
        }
        x
    }

    /// x から y へ `<` の経路があるか。None は探索予算超過（不明）。
    fn reaches(&self, x: u32, y: u32) -> Option<bool> {
        let (x, y) = (self.find(x), self.find(y));
        if x == y || self.ord[x as usize] >= self.ord[y as usize] {
            return Some(false);
        }
        let limit = self.ord[y as usize];
        let mut stack = vec![x];
        let mut seen = HashSet::new();
        seen.insert(x);
        while let Some(n) = stack.pop() {
            for &m in &self.succ[n as usize] {
                if m == y {
                    return Some(true);
                }
                if self.ord[m as usize] < limit && seen.insert(m) {
                    if seen.len() > self.budget {
                        return None;
                    }
                    stack.push(m);
                }
            }
        }
        Some(false)
    }

    /// 2 点間の既知の順序。
    fn point_relation(&self, x: u32, y: u32) -> Option<Option<Pc>> {
        if self.find(x) == self.find(y) {
            return Some(Some(Pc::Le)); // 等号（両向き ≤）
        }
        if self.reaches(x, y)? {
            return Some(Some(Pc::Lt));
        }
        Some(None)
    }

    /// 4 端点間の既知関係行列。m[i][j] = Some(Lt) は x_i < x_j、Some(Le) は x_i ≤ x_j。
    fn known_matrix(&self, pts: [u32; 4]) -> [[Option<Pc>; 4]; 4] {
        let mut m = [[None; 4]; 4];
        for i in 0..4 {
            for j in 0..4 {
                if i != j {
                    m[i][j] = self.point_relation(pts[i], pts[j]).flatten();
                }
            }
        }
        m
    }

    /// 既知関係 + 追加制約が矛盾しないか（4 点上の閉包で厳密に判定）。
    fn consistent(mut m: [[Option<Pc>; 4]; 4], extra: &[(usize, usize, i8)]) -> bool {
        let tighten = |cur: Option<Pc>, new: Pc| match (cur, new) {
            (Some(Pc::Lt), _) | (_, Pc::Lt) => Some(Pc::Lt),
            _ => Some(Pc::Le),
        };
        for &(i, j, op) in extra {
            if op < 0 {
                m[i][j] = tighten(m[i][j], Pc::Lt);
            } else {
                m[i][j] = tighten(m[i][j], Pc::Le);
                m[j][i] = tighten(m[j][i], Pc::Le);
            }
        }
        for k in 0..4 {
            for i in 0..4 {
                for j in 0..4 {
                    if let (Some(a), Some(b)) = (m[i][k], m[k][j]) {
                        let c = if a == Pc::Lt || b == Pc::Lt { Pc::Lt } else { Pc::Le };
                        if i == j {
                            if c == Pc::Lt {
                                return false;
                            }
                        } else {
                            m[i][j] = tighten(m[i][j], c);
                        }
                    }
                }
            }
        }
        (0..4).all(|i| m[i][i] != Some(Pc::Lt))
    }

    /// `a rel b` を追加する。既存の制約と矛盾する場合は変更せず `Conflict` を返す。
    pub fn add_relation(&mut self, a: ResourceId, rel: AllenRelation, b: ResourceId) -> Result<()> {
        if a == b {
            return if rel == AllenRelation::Equals { Ok(()) } else { Err(Error::Conflict(format!("{a} cannot be `{}` itself", rel.name()))) };
        }
        let (s1, e1) = self.ensure_event(a);
        let (s2, e2) = self.ensure_event(b);
        let pts = [s1, e1, s2, e2];
        let cons = endpoint_constraints(rel);
        if !Self::consistent(self.known_matrix(pts), cons) {
            return Err(Error::Conflict(format!("`{a} {} {b}` contradicts existing temporal constraints", rel.name())));
        }
        let mut merged = false;
        for &(i, j, op) in cons {
            if op < 0 {
                self.add_lt(pts[i], pts[j])?;
            } else {
                merged |= self.merge(pts[i], pts[j]);
            }
        }
        if merged {
            self.reorder_all();
        }
        Ok(())
    }

    /// Pearce–Kelly: x < y の辺を追加し、必要な範囲だけ順序を付け直す。
    fn add_lt(&mut self, x: u32, y: u32) -> Result<()> {
        let (x, y) = (self.find(x), self.find(y));
        if x == y {
            return Err(Error::Conflict("strict order between equal points".into()));
        }
        if self.succ[x as usize].contains(&y) {
            return Ok(());
        }
        self.succ[x as usize].push(y);
        self.pred[y as usize].push(x);
        let (lb, ub) = (self.ord[y as usize], self.ord[x as usize]);
        if lb > ub {
            return Ok(());
        }
        // 前方: y から到達でき ord ≤ ub のノード。
        let mut fwd = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![y];
        seen.insert(y);
        while let Some(n) = stack.pop() {
            fwd.push(n);
            for &m in &self.succ[n as usize] {
                if m == x {
                    // consistent() で検査済みのため通常は到達しない。
                    self.succ[x as usize].retain(|&v| v != y);
                    self.pred[y as usize].retain(|&v| v != x);
                    return Err(Error::Cycle("temporal order cycle".into()));
                }
                if self.ord[m as usize] <= ub && seen.insert(m) {
                    stack.push(m);
                }
            }
        }
        // 後方: x へ到達でき ord ≥ lb のノード。
        let mut bwd = Vec::new();
        let mut seen_b = HashSet::new();
        let mut stack = vec![x];
        seen_b.insert(x);
        while let Some(n) = stack.pop() {
            bwd.push(n);
            for &m in &self.pred[n as usize] {
                if self.ord[m as usize] >= lb && seen_b.insert(m) {
                    stack.push(m);
                }
            }
        }
        bwd.sort_by_key(|n| self.ord[*n as usize]);
        fwd.sort_by_key(|n| self.ord[*n as usize]);
        let mut slots: Vec<u64> = bwd.iter().chain(fwd.iter()).map(|n| self.ord[*n as usize]).collect();
        slots.sort_unstable();
        for (n, o) in bwd.iter().chain(fwd.iter()).zip(slots) {
            self.ord[*n as usize] = o;
        }
        Ok(())
    }

    /// 点を併合する。併合した場合 true。
    fn merge(&mut self, x: u32, y: u32) -> bool {
        let (rx, ry) = (self.find(x), self.find(y));
        if rx == ry {
            return false;
        }
        self.parent[ry as usize] = rx;
        let succ = std::mem::take(&mut self.succ[ry as usize]);
        let pred = std::mem::take(&mut self.pred[ry as usize]);
        for s in succ {
            let s = self.find(s);
            for p in self.pred[s as usize].iter_mut() {
                if *p == ry {
                    *p = rx;
                }
            }
            if s != rx && !self.succ[rx as usize].contains(&s) {
                self.succ[rx as usize].push(s);
            }
        }
        for p in pred {
            let p = self.find(p);
            for s in self.succ[p as usize].iter_mut() {
                if *s == ry {
                    *s = rx;
                }
            }
            if p != rx && !self.pred[rx as usize].contains(&p) {
                self.pred[rx as usize].push(p);
            }
        }
        true
    }

    /// Kahn 法で代表点全体の順序を付け直す。
    fn reorder_all(&mut self) {
        let n = self.parent.len();
        for i in 0..n {
            let fixed: Vec<u32> = {
                let mut v: Vec<u32> = self.succ[i].iter().map(|&s| self.find(s)).collect();
                v.sort_unstable();
                v.dedup();
                v
            };
            self.succ[i] = fixed;
        }
        let mut indeg = vec![0usize; n];
        for i in 0..n {
            if self.find(i as u32) != i as u32 {
                continue;
            }
            for &s in &self.succ[i] {
                indeg[s as usize] += 1;
            }
        }
        let mut queue: std::collections::VecDeque<u32> = (0..n as u32).filter(|&i| self.find(i) == i && indeg[i as usize] == 0).collect();
        let mut next = 0u64;
        while let Some(v) = queue.pop_front() {
            self.ord[v as usize] = next;
            next += 1;
            for &s in &self.succ[v as usize] {
                indeg[s as usize] -= 1;
                if indeg[s as usize] == 0 {
                    queue.push_back(s);
                }
            }
        }
        self.next_ord = next.max(self.next_ord);
        for i in 0..n {
            let v: Vec<u32> = {
                let mut v: Vec<u32> = self.pred[i].iter().map(|&p| self.find(p)).collect();
                v.sort_unstable();
                v.dedup();
                v
            };
            self.pred[i] = v;
        }
    }

    /// 2 つの Event の間であり得る Allen 関係（グラフから導ける範囲）。
    /// どちらかが未登録なら None（情報なし）。
    pub fn relation(&self, a: &ResourceId, b: &ResourceId) -> Option<AllenSet> {
        let (s1, e1) = *self.events.get(a)?;
        let (s2, e2) = *self.events.get(b)?;
        let m = self.known_matrix([s1, e1, s2, e2]);
        let mut out = 0u16;
        for r in ALL_RELATIONS {
            if Self::consistent(m, endpoint_constraints(r)) {
                out |= r.bit();
            }
        }
        Some(AllenSet(out))
    }

    /// a が b より確実に前に終わるか（before / meets）。
    pub fn definitely_precedes(&self, a: &ResourceId, b: &ResourceId) -> Option<bool> {
        let rs = self.relation(a, b)?;
        Some(!rs.is_empty() && AllenSet::precedes().intersect(rs) == rs)
    }

    pub fn label(&self, id: &ResourceId) -> Option<OrderLabel> {
        let (s, e) = *self.events.get(id)?;
        Some(OrderLabel { start_ord: self.ord[self.find(s) as usize], end_ord: self.ord[self.find(e) as usize] })
    }

    /// 登録済み Event を順序ラベル（開始点）で並べた線形拡張。
    pub fn linear_extension(&self) -> Vec<(ResourceId, OrderLabel)> {
        let mut v: Vec<_> = self.events.keys().filter_map(|id| self.label(id).map(|l| (*id, l))).collect();
        v.sort_by_key(|(id, l)| (l.start_ord, l.end_ord, *id));
        v
    }
}

fn endpoint_constraints(rel: AllenRelation) -> &'static [(usize, usize, i8)] {
    // allen.rs と同じ端点制約（変数: 0=s1, 1=e1, 2=s2, 3=e2）。
    use AllenRelation::*;
    match rel {
        Before => &[(1, 2, -1)],
        Meets => &[(1, 2, 0)],
        Overlaps => &[(0, 2, -1), (2, 1, -1), (1, 3, -1)],
        Starts => &[(0, 2, 0), (1, 3, -1)],
        During => &[(2, 0, -1), (1, 3, -1)],
        Finishes => &[(1, 3, 0), (2, 0, -1)],
        Equals => &[(0, 2, 0), (1, 3, 0)],
        FinishedBy => &[(1, 3, 0), (0, 2, -1)],
        Contains => &[(0, 2, -1), (3, 1, -1)],
        StartedBy => &[(0, 2, 0), (3, 1, -1)],
        OverlappedBy => &[(2, 0, -1), (0, 3, -1), (3, 1, -1)],
        MetBy => &[(3, 0, 0)],
        After => &[(3, 0, -1)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AllenRelation::*;

    #[test]
    fn chain_and_contradiction() {
        let (a, b, c) = (ResourceId::new(), ResourceId::new(), ResourceId::new());
        let mut g = TemporalOrderGraph::new();
        // 逆順に追加して Pearce–Kelly の並べ替えを発生させる。
        g.add_relation(b, Before, c).unwrap();
        g.add_relation(a, Before, b).unwrap();
        assert_eq!(g.relation(&a, &c), Some(AllenSet::single(Before)));
        assert_eq!(g.definitely_precedes(&a, &c), Some(true));
        assert!(g.add_relation(c, Before, a).is_err());
        let la = g.label(&a).unwrap();
        let lc = g.label(&c).unwrap();
        assert!(la.end_ord < lc.start_ord);
        let ext: Vec<_> = g.linear_extension().into_iter().map(|x| x.0).collect();
        assert_eq!(ext, vec![a, b, c]);
    }

    #[test]
    fn equality_and_during() {
        let (a, b, c) = (ResourceId::new(), ResourceId::new(), ResourceId::new());
        let mut g = TemporalOrderGraph::new();
        g.add_relation(a, During, b).unwrap();
        g.add_relation(b, Equals, c).unwrap();
        assert_eq!(g.relation(&a, &c), Some(AllenSet::single(During)));
        assert!(g.add_relation(c, Before, a).is_err());
        let d = ResourceId::new();
        g.add_relation(a, Meets, d).unwrap();
        let rs = g.relation(&d, &c).unwrap();
        assert!(!rs.contains(Before));
        assert!(rs.contains(Overlaps) || rs.contains(During) || rs.contains(Finishes));
    }
}
