//! Allen Interval Algebra（13 関係）のビットマスク表現。
//!
//! - 確定区間同士の関係判定
//! - 4 点境界（FuzzyRange）同士で「あり得る関係」の集合（差分制約の充足判定で厳密に求める）
//! - 合成表（起動時に小さな整数区間を総当たりして生成）

use super::Tick;
use super::range::FuzzyRange;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum AllenRelation {
    Before = 0,
    Meets,
    Overlaps,
    Starts,
    During,
    Finishes,
    Equals,
    FinishedBy,
    Contains,
    StartedBy,
    OverlappedBy,
    MetBy,
    After,
}

pub const ALL_RELATIONS: [AllenRelation; 13] = [
    AllenRelation::Before,
    AllenRelation::Meets,
    AllenRelation::Overlaps,
    AllenRelation::Starts,
    AllenRelation::During,
    AllenRelation::Finishes,
    AllenRelation::Equals,
    AllenRelation::FinishedBy,
    AllenRelation::Contains,
    AllenRelation::StartedBy,
    AllenRelation::OverlappedBy,
    AllenRelation::MetBy,
    AllenRelation::After,
];

impl AllenRelation {
    pub fn inverse(self) -> AllenRelation {
        ALL_RELATIONS[12 - self as usize]
    }

    pub fn bit(self) -> u16 {
        1 << self as u16
    }

    pub fn name(self) -> &'static str {
        match self {
            AllenRelation::Before => "before",
            AllenRelation::Meets => "meets",
            AllenRelation::Overlaps => "overlaps",
            AllenRelation::Starts => "starts",
            AllenRelation::During => "during",
            AllenRelation::Finishes => "finishes",
            AllenRelation::Equals => "equals",
            AllenRelation::FinishedBy => "finished_by",
            AllenRelation::Contains => "contains",
            AllenRelation::StartedBy => "started_by",
            AllenRelation::OverlappedBy => "overlapped_by",
            AllenRelation::MetBy => "met_by",
            AllenRelation::After => "after",
        }
    }

    pub fn from_name(s: &str) -> Option<AllenRelation> {
        ALL_RELATIONS.iter().copied().find(|r| r.name() == s)
    }

    /// 確定区間 `[s1,e1)` と `[s2,e2)` の関係（s < e を前提）。
    pub fn of(a: (i64, i64), b: (i64, i64)) -> AllenRelation {
        let ((s1, e1), (s2, e2)) = (a, b);
        use std::cmp::Ordering::*;
        match (s1.cmp(&s2), e1.cmp(&e2)) {
            (Equal, Equal) => AllenRelation::Equals,
            (Equal, Less) => AllenRelation::Starts,
            (Equal, Greater) => AllenRelation::StartedBy,
            (Greater, Equal) => AllenRelation::Finishes,
            (Less, Equal) => AllenRelation::FinishedBy,
            (Greater, Less) => AllenRelation::During,
            (Less, Greater) => AllenRelation::Contains,
            (Less, Less) => match e1.cmp(&s2) {
                Less => AllenRelation::Before,
                Equal => AllenRelation::Meets,
                Greater => AllenRelation::Overlaps,
            },
            (Greater, Greater) => match s1.cmp(&e2) {
                Greater => AllenRelation::After,
                Equal => AllenRelation::MetBy,
                Less => AllenRelation::OverlappedBy,
            },
        }
    }

    /// 端点制約 (i, j, rel)。変数: 0 = s1, 1 = e1, 2 = s2, 3 = e2。
    /// rel: -1 は x_i < x_j、0 は x_i = x_j。
    fn endpoint_constraints(self) -> &'static [(usize, usize, i8)] {
        use AllenRelation::*;
        match self {
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
}

/// Allen 関係の集合（13 ビット）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AllenSet(pub u16);

impl AllenSet {
    pub const EMPTY: AllenSet = AllenSet(0);
    pub const ALL: AllenSet = AllenSet(0x1FFF);

    pub fn single(r: AllenRelation) -> Self {
        AllenSet(r.bit())
    }

    pub fn of(rels: &[AllenRelation]) -> Self {
        AllenSet(rels.iter().fold(0, |m, r| m | r.bit()))
    }

    pub fn contains(self, r: AllenRelation) -> bool {
        self.0 & r.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn len(self) -> u32 {
        self.0.count_ones()
    }

    pub fn union(self, o: AllenSet) -> AllenSet {
        AllenSet(self.0 | o.0)
    }

    pub fn intersect(self, o: AllenSet) -> AllenSet {
        AllenSet(self.0 & o.0)
    }

    pub fn iter(self) -> impl Iterator<Item = AllenRelation> {
        ALL_RELATIONS.into_iter().filter(move |r| self.contains(*r))
    }

    pub fn inverse(self) -> AllenSet {
        AllenSet(self.iter().fold(0, |m, r| m | r.inverse().bit()))
    }

    pub fn names(self) -> Vec<&'static str> {
        self.iter().map(AllenRelation::name).collect()
    }

    /// 単一関係に確定しているか。
    pub fn certain(self) -> Option<AllenRelation> {
        (self.len() == 1).then(|| self.iter().next()).flatten()
    }

    /// 関係合成 R ∘ S（A R B かつ B S C のとき A と C のあり得る関係）。
    pub fn compose(self, other: AllenSet) -> AllenSet {
        let table = composition_table();
        let mut out = 0u16;
        for a in self.iter() {
            for b in other.iter() {
                out |= table[a as usize][b as usize];
            }
        }
        AllenSet(out)
    }

    /// 「A は B より前に終わる」系（before / meets）。
    pub fn precedes() -> AllenSet {
        AllenSet::of(&[AllenRelation::Before, AllenRelation::Meets])
    }
}

impl std::fmt::Debug for AllenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{{}}}", self.names().join(","))
    }
}

fn composition_table() -> &'static [[u16; 13]; 13] {
    static TABLE: OnceLock<[[u16; 13]; 13]> = OnceLock::new();
    TABLE.get_or_init(|| {
        // 6 端点の全順序型を網羅するには 6 値あれば十分。
        let mut ivs = Vec::new();
        for s in 0..6i64 {
            for e in (s + 1)..6 {
                ivs.push((s, e));
            }
        }
        let mut t = [[0u16; 13]; 13];
        for &a in &ivs {
            for &b in &ivs {
                let r1 = AllenRelation::of(a, b) as usize;
                for &c in &ivs {
                    let r2 = AllenRelation::of(b, c) as usize;
                    t[r1][r2] |= AllenRelation::of(a, c).bit();
                }
            }
        }
        t
    })
}

/// 4 点境界の区間 A, B について、あり得る Allen 関係の集合。
/// 端点の値域と関係ごとの端点制約を差分制約系として表し、負閉路の有無で充足可能性を判定する。
pub fn possible_relations(a: &FuzzyRange, b: &FuzzyRange) -> AllenSet {
    let mut out = 0u16;
    for r in ALL_RELATIONS {
        if feasible(a, b, r) {
            out |= r.bit();
        }
    }
    AllenSet(out)
}

fn feasible(a: &FuzzyRange, b: &FuzzyRange, rel: AllenRelation) -> bool {
    const INF: i128 = i128::MAX / 4;
    // ノード 0..4 = s1,e1,s2,e2、ノード 4 = 原点(0)。
    let mut d = [[INF; 5]; 5];
    for (i, row) in d.iter_mut().enumerate() {
        row[i] = 0;
    }
    // x_j - x_i ≤ w を辺 i→j (重み w) として追加。
    let mut add = |i: usize, j: usize, w: i128| {
        if w < d[i][j] {
            d[i][j] = w;
        }
    };
    let bounds = [(a.earliest_start, a.latest_start), (a.earliest_end, a.latest_end), (b.earliest_start, b.latest_start), (b.earliest_end, b.latest_end)];
    for (v, (lo, hi)) in bounds.iter().enumerate() {
        if lo.is_finite() {
            add(v, 4, -(lo.0 as i128)); // 0 - x ≤ -lo
        }
        if hi.is_finite() {
            add(4, v, hi.0 as i128); // x - 0 ≤ hi
        }
    }
    // s < e
    add(1, 0, -1);
    add(3, 2, -1);
    for &(i, j, op) in rel.endpoint_constraints() {
        match op {
            -1 => add(j, i, -1), // x_i - x_j ≤ -1
            _ => {
                add(j, i, 0);
                add(i, j, 0);
            }
        }
    }
    for k in 0..5 {
        for i in 0..5 {
            if d[i][k] == INF {
                continue;
            }
            for j in 0..5 {
                if d[k][j] == INF {
                    continue;
                }
                let v = d[i][k] + d[k][j];
                if v < d[i][j] {
                    d[i][j] = v;
                }
            }
        }
    }
    (0..5).all(|i| d[i][i] >= 0)
}

/// 確定区間から関係を得る簡易版。
pub fn relation_of_exact(a: (Tick, Tick), b: (Tick, Tick)) -> AllenRelation {
    AllenRelation::of((a.0.0, a.1.0), (b.0.0, b.1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use AllenRelation::*;

    #[test]
    fn composition_known_entries() {
        assert_eq!(AllenSet::single(Before).compose(AllenSet::single(Before)), AllenSet::single(Before));
        assert_eq!(AllenSet::single(Meets).compose(AllenSet::single(Meets)), AllenSet::single(Before));
        assert_eq!(AllenSet::single(During).compose(AllenSet::single(During)), AllenSet::single(During));
        assert_eq!(AllenSet::single(Before).compose(AllenSet::single(After)), AllenSet::ALL);
        assert_eq!(AllenSet::single(Equals).compose(AllenSet::single(Overlaps)), AllenSet::single(Overlaps));
        for r in ALL_RELATIONS {
            assert_eq!(r.inverse().inverse(), r);
        }
    }

    #[test]
    fn fuzzy_relations() {
        let a = FuzzyRange::exact(Tick(0), Tick(10));
        let b = FuzzyRange::exact(Tick(20), Tick(30));
        assert_eq!(possible_relations(&a, &b), AllenSet::single(Before));
        let c = FuzzyRange::within(Tick(5), Tick(25));
        let rs = possible_relations(&a, &c);
        assert!(rs.contains(Before) && rs.contains(Overlaps) && rs.contains(Meets));
        assert!(!rs.contains(After));
        assert_eq!(possible_relations(&a, &FuzzyRange::unknown()), AllenSet::ALL);
    }
}
