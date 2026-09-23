//! Resolver / Materializer。
//!
//! 変更 → 無効化キュー → 再計算 の結果整合モデル。変更時に全伝播を同期実行せず、
//! 影響を受けた Resource をキューへ積み、Projection 行を stale にしておく。
//! `process` がキューを消化して行を作り直し、時間が変わった Resource の依存先（相対時間で
//! 参照している Event）を追加で無効化する。

use crate::canonical::CanonicalStore;
use crate::facts::{ClaimEval, Facts, TimeSlot};
use crate::projection::{ProjectionRow, ProjectionSet};
use crate::view::View;
use chronotope_core::model::{AssertionStatus, Polarity, PredicateRole};
use chronotope_core::time::Tick;
use chronotope_core::time::allen::AllenRelation;
use chronotope_core::time::order::TemporalOrderGraph;
use chronotope_core::*;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

pub struct BranchState {
    pub branch: BranchId,
    pub projection: ProjectionSet,
    pub time_cache: HashMap<ResourceId, TimeSlot>,
    pub time_dependents: HashMap<ResourceId, HashSet<ResourceId>>,
    pub name_dependents: HashMap<String, HashSet<ResourceId>>,
    pub ranks: HashMap<AssertionId, ClaimEval>,
    pub order: TemporalOrderGraph,
    pub order_dirty: bool,
    pub pending_relations: Vec<AssertionId>,
    /// 時間順序グラフへ載せられなかった（既存制約と矛盾する）関係。
    pub order_conflicts: BTreeMap<AssertionId, String>,
    queue: VecDeque<ResourceId>,
    queued: HashSet<ResourceId>,
    pub materialized_seq: u64,
    pub last_materialized_at: Tick,
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct MaterializeStats {
    pub processed: usize,
    pub remaining: usize,
    pub order_rebuilt: bool,
}

impl BranchState {
    pub fn new(branch: BranchId) -> Self {
        BranchState {
            branch,
            projection: ProjectionSet::new(),
            time_cache: HashMap::new(),
            time_dependents: HashMap::new(),
            name_dependents: HashMap::new(),
            ranks: HashMap::new(),
            order: TemporalOrderGraph::new(),
            order_dirty: true,
            pending_relations: vec![],
            order_conflicts: BTreeMap::new(),
            queue: VecDeque::new(),
            queued: HashSet::new(),
            materialized_seq: 0,
            last_materialized_at: Tick(0),
        }
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Resource を無効化する。相対時間でこれを参照している Resource も推移的に stale にして
    /// キューへ積む（再計算前に古い値を「新鮮」と見せないため）。
    pub fn invalidate(&mut self, id: ResourceId) {
        let mut work = vec![id];
        let mut seen = HashSet::new();
        while let Some(x) = work.pop() {
            if !seen.insert(x) {
                continue;
            }
            self.time_cache.remove(&x);
            self.projection.mark_stale(&x);
            if self.queued.insert(x) {
                self.queue.push_back(x);
            }
            if let Some(deps) = self.time_dependents.get(&x) {
                work.extend(deps.iter().copied());
            }
        }
    }

    pub fn invalidate_all(&mut self, store: &CanonicalStore) {
        self.time_cache.clear();
        self.order_dirty = true;
        let mut ids: Vec<ResourceId> = store.resources.keys().copied().collect();
        ids.sort();
        for id in ids {
            self.invalidate(id);
        }
    }

    pub fn invalidate_name(&mut self, name: &str) {
        if let Some(ids) = self.name_dependents.get(name).cloned() {
            for id in ids {
                self.invalidate(id);
            }
        }
    }

    fn relation_of(store: &CanonicalStore, aid: &AssertionId, view: &View) -> Option<(ResourceId, AllenRelation, ResourceId, bool)> {
        let a = store.assertions.get(aid)?;
        let p = store.predicates.get(&a.predicate)?;
        if p.role != PredicateRole::TemporalRelation || a.polarity != Polarity::Affirmed {
            return None;
        }
        let st = store.effective_status(a, view)?;
        let rel = AllenRelation::from_name(p.allen.as_deref()?)?;
        let o = store.resolve_id(a.object.as_resource()?);
        Some((store.resolve_id(a.subject), rel, o, st == AssertionStatus::Accepted))
    }

    /// Canonical の時間関係 Assertion から順序グラフを作り直す（Source of Truth は Canonical 側）。
    /// 承認済みを優先して載せ、矛盾する関係は order_conflicts に記録する。
    pub fn rebuild_order(&mut self, store: &CanonicalStore, view: &View) {
        let mut rels = vec![];
        for p in store.predicates.values().filter(|p| p.role == PredicateRole::TemporalRelation) {
            for aid in store.by_predicate.get(&p.id).into_iter().flatten() {
                if let Some((s, r, o, accepted)) = Self::relation_of(store, aid, view) {
                    let seq = store.seq_of(&store.assertions[aid].created_revision);
                    rels.push((!accepted, seq, *aid, s, r, o));
                }
            }
        }
        rels.sort();
        self.order = TemporalOrderGraph::new();
        self.order_conflicts.clear();
        for (_, _, aid, s, r, o) in rels {
            if let Err(e) = self.order.add_relation(s, r, o) {
                self.order_conflicts.insert(aid, e.to_string());
            }
        }
        self.order_dirty = false;
        self.pending_relations.clear();
    }

    fn apply_pending_relations(&mut self, store: &CanonicalStore, view: &View) {
        for aid in std::mem::take(&mut self.pending_relations) {
            if let Some((s, r, o, _)) = Self::relation_of(store, &aid, view) {
                if let Err(e) = self.order.add_relation(s, r, o) {
                    self.order_conflicts.insert(aid, e.to_string());
                }
            }
        }
    }

    /// キューを最大 `max` 件処理する。`on_row` は行が更新されるたびに呼ばれる（Embedding 生成など）。
    pub fn process(&mut self, store: &CanonicalStore, now: Tick, max: usize, on_row: &mut dyn FnMut(&ProjectionRow)) -> Result<MaterializeStats> {
        let view = View::projection(store, self.branch)?;
        let mut stats = MaterializeStats::default();
        if self.order_dirty {
            self.rebuild_order(store, &view);
            stats.order_rebuilt = true;
        } else if !self.pending_relations.is_empty() {
            self.apply_pending_relations(store, &view);
        }
        while stats.processed < max {
            let Some(id) = self.queue.pop_front() else { break };
            self.queued.remove(&id);
            stats.processed += 1;
            let canonical = store.resolve_id(id);
            if canonical != id {
                // 統合済み: 旧 ID の行は消し、統合先を作り直す。
                self.projection.remove(&id);
                if !self.queued.contains(&canonical) {
                    self.invalidate(canonical);
                }
                continue;
            }
            let facts_engine = Facts::new(store, &view, now, std::mem::take(&mut self.time_cache));
            let facts = facts_engine.compute(id);
            let (cache, deps) = facts_engine.into_parts();
            self.time_cache = cache;
            for (dependent, anchors, names) in deps {
                for a in anchors {
                    self.time_dependents.entry(a).or_default().insert(dependent);
                }
                for n in names {
                    self.name_dependents.entry(n).or_default().insert(dependent);
                }
            }
            let Some(facts) = facts else {
                self.projection.remove(&id);
                continue;
            };
            for (aid, ev) in &facts.evals {
                self.ranks.insert(*aid, ev.clone());
            }
            let Some(row) = ProjectionRow::from_facts(store, &facts, self.order.label(&id), store.head_seq, now) else { continue };
            let old_time = self.projection.row(&id).map(|r| (r.temporal.clone(), r.temporal_unresolved.clone()));
            let new_time = (row.temporal.clone(), row.temporal_unresolved.clone());
            if old_time.as_ref() != Some(&new_time) {
                if let Some(deps) = self.time_dependents.get(&id).cloned() {
                    for d in deps {
                        if d != id {
                            self.invalidate(d);
                        }
                    }
                }
            }
            on_row(&row);
            self.projection.upsert(store, row);
        }
        stats.remaining = self.queue.len();
        if self.queue.is_empty() {
            self.materialized_seq = store.head_seq;
        }
        self.last_materialized_at = now;
        Ok(stats)
    }
}
