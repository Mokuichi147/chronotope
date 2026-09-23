//! Search Projection（検索速度優先・非正規化）。
//!
//! 1 Resource = 1 行のフラットな行で、検索時に JOIN を必要としない。行は Materializer が
//! Canonical から再計算し、鮮度情報（source_revision / materialized_at / stale）を持つ。
//! 行と並行して、型・関係・場所・作品・時間・座標・ラベルの各索引を維持する。

use crate::canonical::CanonicalStore;
use crate::facts::ResourceFacts;
use crate::index::geo::GeoIndex;
use crate::index::temporal::TemporalIndex;
use crate::text::TextIndex;
use chronotope_core::model::{Label, Visibility};
use chronotope_core::space::Placement;
use chronotope_core::time::Tick;
use chronotope_core::time::order::OrderLabel;
use chronotope_core::time::range::ResolvedTemporal;
use chronotope_core::time::resolve::Unresolved;
use chronotope_core::*;
use roaring::RoaringBitmap;
use serde::Serialize;
use std::collections::HashMap;

pub const PROJECTION_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct ProjectionRow {
    pub canonical_id: ResourceId,
    #[serde(skip)]
    pub doc: u32,
    pub types_mask: u64,
    pub type_ids: Vec<ResourceId>,
    pub direct_types: Vec<ResourceId>,
    pub labels: Vec<Label>,
    pub description: Option<String>,
    pub temporal: Option<ResolvedTemporal>,
    pub temporal_raw: Option<String>,
    pub temporal_unresolved: Option<Unresolved>,
    pub temporal_contested: bool,
    pub temporal_alternatives: Vec<ResolvedTemporal>,
    pub order_label: Option<OrderLabel>,
    pub placement: Option<Placement>,
    pub placement_inherited_from: Option<ResourceId>,
    pub space_ids: Vec<ResourceId>,
    pub space_ancestor_ids: Vec<ResourceId>,
    pub entity_ids: Vec<ResourceId>,
    pub work_ids: Vec<ResourceId>,
    pub branch_ids: Vec<BranchId>,
    pub canon_ids: Vec<ResourceId>,
    pub timeline_ids: Vec<ResourceId>,
    pub rank: f64,
    pub contested: bool,
    pub known_from: Tick,
    pub redistributable: bool,
    pub visibility: Visibility,
    pub embedding_ref: Option<String>,
    pub projection_version: u32,
    pub source_revision: u64,
    pub materialized_at: Tick,
    pub stale: bool,
}

impl ProjectionRow {
    pub fn from_facts(store: &CanonicalStore, f: &ResourceFacts, order_label: Option<OrderLabel>, source_revision: u64, now: Tick) -> Option<Self> {
        let res = store.resources.get(&f.id)?;
        let (temporal, temporal_unresolved) = match f.time.as_ref().map(|t| &t.result) {
            Some(Ok(t)) => (Some(t.clone()), None),
            Some(Err(u)) => (None, Some(u.clone())),
            None => (None, None),
        };
        // 統合済みメンバーのラベルも検索対象にする。
        let mut labels = res.labels.clone();
        for m in store.identity_group(f.id).into_iter().skip(1) {
            if let Some(r) = store.resources.get(&m) {
                labels.extend(r.labels.iter().cloned());
            }
        }
        Some(ProjectionRow {
            canonical_id: f.id,
            doc: 0,
            types_mask: store.types_mask(&f.types),
            type_ids: f.types.iter().copied().collect(),
            direct_types: res.types.iter().copied().collect(),
            labels,
            description: res.description(None).map(Into::into),
            temporal,
            temporal_raw: f.time.as_ref().and_then(|t| t.raw.clone()),
            temporal_unresolved,
            temporal_contested: f.time.as_ref().is_some_and(|t| t.contested),
            temporal_alternatives: f.time.as_ref().map(|t| t.alternatives.clone()).unwrap_or_default(),
            order_label,
            placement: f.placement.clone(),
            placement_inherited_from: f.placement_inherited_from,
            space_ids: f.places.clone(),
            space_ancestor_ids: f.place_ancestors.clone(),
            entity_ids: f.entities.clone(),
            work_ids: f.works.clone(),
            branch_ids: f.branches.clone(),
            canon_ids: f.canons.clone(),
            timeline_ids: f.timelines.clone(),
            rank: f.rank,
            contested: f.contested,
            known_from: f.known_from,
            redistributable: f.redistributable,
            visibility: res.visibility.clone(),
            embedding_ref: None,
            projection_version: PROJECTION_VERSION,
            source_revision,
            materialized_at: now,
            stale: false,
        })
    }

    pub fn label(&self, lang: Option<&str>) -> &str {
        let pref = |l: &&Label| l.kind == chronotope_core::model::LabelKind::Preferred;
        self.labels
            .iter()
            .filter(pref)
            .find(|l| lang.is_some() && l.lang.as_deref() == lang)
            .or_else(|| self.labels.iter().find(pref))
            .or_else(|| self.labels.first())
            .map(|l| l.text.as_str())
            .unwrap_or("")
    }
}

type Postings = HashMap<ResourceId, RoaringBitmap>;

/// 1 ブランチ分の Projection と索引。
#[derive(Default)]
pub struct ProjectionSet {
    rows: Vec<Option<ProjectionRow>>,
    doc_of: HashMap<ResourceId, u32>,
    pub live: RoaringBitmap,
    pub stale: RoaringBitmap,
    pub idx_type: Postings,
    pub idx_entity: Postings,
    pub idx_space: Postings,
    pub idx_space_ancestor: Postings,
    pub idx_work: Postings,
    pub idx_canon: Postings,
    pub idx_timeline: Postings,
    pub idx_branch: HashMap<BranchId, RoaringBitmap>,
    pub idx_text: TextIndex,
    pub idx_time: TemporalIndex,
    pub idx_geo: GeoIndex,
    /// 繰り返し出来事（常に候補へ入れ、発生単位で検証する）。
    pub recurring: RoaringBitmap,
    /// 時間を持つ行。
    pub dated: RoaringBitmap,
    pub contested: RoaringBitmap,
}

fn post(idx: &mut Postings, keys: &[ResourceId], doc: u32, add: bool) {
    for k in keys {
        if add {
            idx.entry(*k).or_default().insert(doc);
        } else if let Some(b) = idx.get_mut(k) {
            b.remove(doc);
            if b.is_empty() {
                idx.remove(k);
            }
        }
    }
}

impl ProjectionSet {
    pub fn new() -> Self {
        ProjectionSet { idx_time: TemporalIndex::new(), ..Default::default() }
    }

    pub fn len(&self) -> usize {
        self.live.len() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    pub fn doc(&self, id: &ResourceId) -> Option<u32> {
        self.doc_of.get(id).copied()
    }

    pub fn row(&self, id: &ResourceId) -> Option<&ProjectionRow> {
        self.doc_of.get(id).and_then(|d| self.rows[*d as usize].as_ref())
    }

    pub fn row_by_doc(&self, doc: u32) -> Option<&ProjectionRow> {
        self.rows.get(doc as usize).and_then(|r| r.as_ref())
    }

    pub fn mark_stale(&mut self, id: &ResourceId) {
        if let Some(&d) = self.doc_of.get(id) {
            if let Some(r) = self.rows[d as usize].as_mut() {
                r.stale = true;
                self.stale.insert(d);
            }
        }
    }

    fn unindex(&mut self, row: &ProjectionRow) {
        let d = row.doc;
        post(&mut self.idx_type, &row.type_ids, d, false);
        post(&mut self.idx_entity, &row.entity_ids, d, false);
        post(&mut self.idx_space, &row.space_ids, d, false);
        post(&mut self.idx_space_ancestor, &row.space_ancestor_ids, d, false);
        post(&mut self.idx_work, &row.work_ids, d, false);
        post(&mut self.idx_canon, &row.canon_ids, d, false);
        post(&mut self.idx_timeline, &row.timeline_ids, d, false);
        for b in &row.branch_ids {
            if let Some(x) = self.idx_branch.get_mut(b) {
                x.remove(d);
            }
        }
        self.idx_text.remove(d);
        self.idx_time.remove(d);
        self.idx_geo.remove(d);
        self.recurring.remove(d);
        self.dated.remove(d);
        self.contested.remove(d);
    }

    pub fn upsert(&mut self, store: &CanonicalStore, mut row: ProjectionRow) {
        let doc = match self.doc_of.get(&row.canonical_id) {
            Some(&d) => d,
            None => {
                let d = self.rows.len() as u32;
                self.rows.push(None);
                self.doc_of.insert(row.canonical_id, d);
                d
            }
        };
        row.doc = doc;
        if let Some(old) = self.rows[doc as usize].take() {
            self.unindex(&old);
        }
        post(&mut self.idx_type, &row.type_ids, doc, true);
        post(&mut self.idx_entity, &row.entity_ids, doc, true);
        post(&mut self.idx_space, &row.space_ids, doc, true);
        post(&mut self.idx_space_ancestor, &row.space_ancestor_ids, doc, true);
        post(&mut self.idx_work, &row.work_ids, doc, true);
        post(&mut self.idx_canon, &row.canon_ids, doc, true);
        post(&mut self.idx_timeline, &row.timeline_ids, doc, true);
        for b in &row.branch_ids {
            self.idx_branch.entry(*b).or_default().insert(doc);
        }
        let texts: Vec<&str> = row.labels.iter().map(|l| l.text.as_str()).collect();
        self.idx_text.set(doc, &texts);
        if let Some(t) = &row.temporal {
            self.dated.insert(doc);
            if t.recurrence.is_some() {
                self.recurring.insert(doc);
            } else {
                // 異説の時間も取りこぼさないよう、同じ軸の包絡で索引する。
                let (mut es, mut le) = t.range.possible_span();
                for a in row.temporal_alternatives.iter().filter(|a| a.axis == t.axis && a.recurrence.is_none()) {
                    es = es.min(a.range.earliest_start);
                    le = le.max(a.range.latest_end);
                }
                self.idx_time.insert(doc, es, le);
            }
        }
        if let Some(p) = &row.placement {
            self.idx_geo.insert(&store.frames, doc, p);
        }
        if row.contested {
            self.contested.insert(doc);
        }
        self.live.insert(doc);
        self.stale.remove(doc);
        self.rows[doc as usize] = Some(row);
    }

    pub fn remove(&mut self, id: &ResourceId) {
        if let Some(&d) = self.doc_of.get(id) {
            if let Some(old) = self.rows[d as usize].take() {
                self.unindex(&old);
            }
            self.live.remove(d);
            self.stale.remove(d);
        }
    }

    pub fn ids_of(&self, bm: &RoaringBitmap) -> Vec<ResourceId> {
        bm.iter().filter_map(|d| self.row_by_doc(d).map(|r| r.canonical_id)).collect()
    }

    pub fn bitmap_of(&self, idx: &Postings, key: &ResourceId) -> RoaringBitmap {
        idx.get(key).cloned().unwrap_or_default()
    }
}
