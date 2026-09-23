//! Structured Filter + Vector ANN → 候補 → 検証・再順位付け。

use super::{OrderBy, QCtx, SearchSpec, SimilarityWeights, TimeFilter, TimeMode, VectorQuery};
use crate::facts::ResourceFacts;
use crate::kb::DEFAULT_EMBEDDING_SPACE;
use crate::projection::{ProjectionRow, ProjectionSet};
use crate::text::normalize_label;
use chronotope_core::space::{Geometry, Placement, Point};
use chronotope_core::time::expr::TemporalExpression;
use chronotope_core::time::range::ResolvedTemporal;
use chronotope_core::time::resolve::{AnchorResult, ResolveContext, resolve};
use chronotope_core::time::{TICKS_PER_DAY, Tick};
use chronotope_core::*;
use roaring::RoaringBitmap;
use serde_json::{Value as Json, json};
use std::collections::{HashMap, HashSet};

/// 検索時間窓（軸付き）。
pub(super) struct Window {
    pub qs: Tick,
    pub qe: Tick,
    pub axis: String,
    pub mode: TimeMode,
    pub include_undated: bool,
}

impl Window {
    fn accepts(&self, t: &ResolvedTemporal) -> bool {
        match self.mode {
            TimeMode::Within if t.recurrence.is_none() => t.range.certainly_within(self.qs, self.qe),
            TimeMode::Certainly => t.matches_window(self.qs, self.qe, true),
            _ => t.matches_window(self.qs, self.qe, false),
        }
    }
}

fn resolve_bound(ctx: &QCtx, s: &str, calendar: &str) -> Result<ResolvedTemporal> {
    let expr = TemporalExpression::strict(s, calendar)?;
    let cal = ctx.kb.store.calendar(calendar).ok_or_else(|| Error::not_found(format!("calendar `{calendar}`")))?;
    let facts = ctx.facts();
    let lookup = |a: &chronotope_core::time::expr::Anchor| -> AnchorResult {
        use chronotope_core::time::expr::Anchor;
        let id = match a {
            Anchor::Resource(r) => *r,
            Anchor::Named(n) => match ctx.kb.store.find_by_label(n).as_slice() {
                [] => return AnchorResult::NotFound,
                [one] => *one,
                many => return AnchorResult::Ambiguous(many.to_vec()),
            },
            Anchor::Reference => return AnchorResult::NotFound,
        };
        match facts.time_of(id).result {
            Ok(t) => AnchorResult::Found(t),
            Err(u) => AnchorResult::Unresolved(u),
        }
    };
    resolve(&expr, &ResolveContext { reference: Some(ctx.now), calendar: &cal, lookup: &lookup })
        .map_err(|u| Error::invalid(format!("time filter `{s}`: {} ({:?})", u.message, u.kind)))
}

pub(super) fn window(ctx: &QCtx, tf: &TimeFilter) -> Result<Window> {
    let calendar = tf.calendar.clone().unwrap_or_else(|| "gregorian".into());
    let (qs, qe, axis) = if let Some(e) = &tf.expression {
        let r = resolve_bound(ctx, e, &calendar)?;
        (r.range.earliest_start, r.range.latest_end, r.axis)
    } else {
        let bound = |s: &Option<String>| -> Result<Option<ResolvedTemporal>> {
            match s.as_deref().map(str::trim) {
                None | Some("-inf" | "+inf" | "inf" | "") => Ok(None),
                Some(x) => resolve_bound(ctx, x, &calendar).map(Some),
            }
        };
        let (from, to) = (bound(&tf.from)?, bound(&tf.to)?);
        let axis = from
            .as_ref()
            .or(to.as_ref())
            .map(|r| r.axis.clone())
            .or_else(|| ctx.kb.store.calendar(&calendar).map(|c| c.axis))
            .unwrap_or_else(|| "earth".into());
        (from.map(|r| r.range.earliest_start).unwrap_or(Tick::NEG_INF), to.map(|r| r.range.latest_end).unwrap_or(Tick::POS_INF), axis)
    };
    if qs >= qe {
        return Err(Error::invalid("empty time window"));
    }
    Ok(Window { qs, qe, axis, mode: tf.mode, include_undated: tf.include_undated })
}

fn and(cand: &mut Option<RoaringBitmap>, bm: RoaringBitmap) {
    *cand = Some(match cand.take() {
        None => bm,
        Some(c) => c & bm,
    });
}

/// 指定 canon / timeline に属するか、どの canon / timeline にも属さない（共通設定の）行。
fn scoped(p: &ProjectionSet, idx: &HashMap<ResourceId, RoaringBitmap>, keys: &HashSet<ResourceId>) -> RoaringBitmap {
    let mut any = RoaringBitmap::new();
    let mut hit = RoaringBitmap::new();
    for (k, b) in idx {
        any |= b;
        if keys.contains(k) {
            hit |= b;
        }
    }
    hit | (&p.live - any)
}

fn query_vector(ctx: &QCtx, vq: &VectorQuery) -> Result<(String, Vec<f32>)> {
    let space = vq.space.clone().unwrap_or_else(|| DEFAULT_EMBEDDING_SPACE.to_string());
    if let Some(e) = &vq.embedding {
        return Ok((space, e.clone()));
    }
    let text = vq.text.as_ref().ok_or_else(|| Error::invalid("vector query needs `embedding` or `text`"))?;
    let emb = &ctx.kb.embedder;
    let own = format!("{}@{}", emb.model_id(), emb.version());
    if own != space {
        return Err(Error::invalid(format!("text vector queries are only supported for the built-in space `{own}`; pass `embedding` for `{space}`")));
    }
    Ok((space, emb.embed(text)))
}

/// 構造化フィルタの再検証（canon / timeline / 過去時点などで Projection と結果が変わる場合）。
struct FactFilters {
    types: Vec<ResourceId>,
    entities: Vec<ResourceId>,
    works: Vec<ResourceId>,
    within_place: Option<ResourceId>,
}

impl FactFilters {
    fn accepts(&self, f: &ResourceFacts) -> bool {
        (self.types.is_empty() || self.types.iter().any(|t| f.types.contains(t)))
            && self.entities.iter().all(|e| f.entities.contains(e))
            && self.works.iter().all(|w| f.works.contains(w))
            && self.within_place.is_none_or(|p| f.place_ancestors.contains(&p))
    }
}

pub(super) fn search(ctx: &QCtx, branch: BranchId, spec: &SearchSpec) -> Result<Json> {
    let kb = ctx.kb;
    let store = &kb.store;
    let st = kb.state(branch)?;
    let p = &st.projection;
    let limit = spec.limit.unwrap_or(20).clamp(1, 500);
    let mut cand: Option<RoaringBitmap> = None;
    let mut ff = FactFilters { types: vec![], entities: vec![], works: vec![], within_place: None };

    if !spec.types.is_empty() {
        let mut u = RoaringBitmap::new();
        for t in &spec.types {
            let tid = store.type_id(t).ok_or_else(|| Error::not_found(format!("type `{t}`")))?;
            u |= p.bitmap_of(&p.idx_type, &tid);
            ff.types.push(tid);
        }
        and(&mut cand, u);
    }
    for e in &spec.entities {
        let id = ctx.resolve(e)?;
        and(&mut cand, p.bitmap_of(&p.idx_entity, &id));
        ff.entities.push(id);
    }
    for w in &spec.works {
        let id = ctx.resolve(w)?;
        and(&mut cand, p.bitmap_of(&p.idx_work, &id));
        ff.works.push(id);
    }
    let mut near_check: Option<(Placement, f64)> = None;
    let mut geo_filter = false;
    let include_inherited = spec.space.as_ref().is_some_and(|s| s.include_inherited);
    if let Some(sp) = &spec.space {
        if let Some(pl) = &sp.within_place {
            let id = ctx.resolve(pl)?;
            and(&mut cand, p.bitmap_of(&p.idx_space_ancestor, &id));
            ff.within_place = Some(id);
        }
        if let Some(bb) = &sp.bbox {
            let bm = p
                .idx_geo
                .intersecting(&store.frames, bb)
                .ok_or_else(|| Error::Incomparable(format!("frame {} is unknown or has no coordinate mapping", bb.frame)))?;
            and(&mut cand, bm);
            geo_filter = true;
        }
        if let Some(n) = &sp.near {
            let root = store.frames.to_root(&n.at).ok_or_else(|| Error::Incomparable(format!("frame {} is unknown", n.at.frame)))?;
            let c = root.geometry.centroid();
            let (dx, dy) = if store.frames.is_geodetic(&root.frame) {
                let dlat = n.radius / 111_320.0;
                (dlat / c.y.to_radians().cos().abs().max(0.01), dlat)
            } else {
                (n.radius, n.radius)
            };
            let bb = Placement { frame: root.frame, geometry: Geometry::BBox { min: Point::xy(c.x - dx, c.y - dy), max: Point::xy(c.x + dx, c.y + dy) } };
            and(&mut cand, p.idx_geo.intersecting(&store.frames, &bb).unwrap_or_default());
            near_check = Some((n.at.clone(), n.radius));
            geo_filter = true;
        }
    }
    if let Some(text) = &spec.text {
        if let Some(bm) = p.idx_text.search(text) {
            and(&mut cand, bm);
        }
    }
    if let Some(c) = ctx.view.canon {
        and(&mut cand, scoped(p, &p.idx_canon, &HashSet::from([c])));
    }
    if let Some(ts) = &ctx.view.timelines {
        and(&mut cand, scoped(p, &p.idx_timeline, ts));
    }
    let win = spec.time.as_ref().map(|tf| window(ctx, tf)).transpose()?;
    if let Some(w) = &win {
        let mut bm = p.idx_time.overlapping(w.qs, w.qe);
        bm |= &p.recurring;
        if w.include_undated {
            bm |= &p.live - &p.dated;
        }
        and(&mut cand, bm);
    }
    if spec.contested_only {
        and(&mut cand, p.contested.clone());
    }
    let cand = match cand {
        Some(c) => c & &p.live,
        None => p.live.clone(),
    };

    // ---- ベクトル候補
    let mut vec_scores: HashMap<u32, f32> = HashMap::new();
    let candidates: Vec<u32> = if let Some(vq) = &spec.vector {
        let (space, qv) = query_vector(ctx, vq)?;
        let idx = kb.vectors.get(&space).ok_or_else(|| Error::not_found(format!("embedding space `{space}`")))?;
        let k = (limit * 5).max(100);
        let hits = if cand.len() <= 20_000 {
            idx.exact(&qv, &p.ids_of(&cand), k)
        } else {
            let allow = |id: ResourceId| p.doc(&id).is_some_and(|d| cand.contains(d));
            let (hits, truncated) = idx.search(&qv, k, Some(&allow), &|| ctx.deadline.expired())?;
            if truncated {
                ctx.truncated.set(true);
            }
            hits
        };
        for (id, s) in hits {
            if let Some(d) = p.doc(&id) {
                vec_scores.insert(d, s);
            }
        }
        vec_scores.keys().copied().collect()
    } else {
        cand.iter().collect()
    };

    // ---- 並べ替え
    let text_norm = spec.text.as_deref().map(normalize_label);
    let score_of = |row: &ProjectionRow| -> f64 {
        match spec.order_by {
            OrderBy::Rank => row.rank,
            OrderBy::Time => row.temporal.as_ref().map(|t| -(t.range.earliest_start.0 as f64)).unwrap_or(f64::MIN),
            OrderBy::Relevance => {
                let mut s = row.rank * 0.1;
                if let Some(v) = vec_scores.get(&row.doc) {
                    s += *v as f64;
                }
                if let Some(q) = &text_norm {
                    if row.labels.iter().any(|l| normalize_label(&l.text) == *q) {
                        s += 1.0;
                    } else if row.labels.iter().any(|l| normalize_label(&l.text).starts_with(q.as_str())) {
                        s += 0.3;
                    }
                }
                s
            }
        }
    };
    let mut scored: Vec<(f64, u32)> = Vec::with_capacity(candidates.len());
    for (i, d) in candidates.iter().enumerate() {
        if i % 8192 == 0 && ctx.out_of_budget() {
            break;
        }
        if let Some(row) = p.row_by_doc(*d) {
            scored.push((score_of(row), *d));
        }
    }
    let cmp = |a: &(f64, u32), b: &(f64, u32)| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1));
    // 上位だけ部分選択して整列し、足りなければ残りを整列して続ける。
    let head = (limit * 4 + 64).min(scored.len());
    if head < scored.len() && head > 0 {
        scored.select_nth_unstable_by(head - 1, cmp);
        scored[..head].sort_by(cmp);
    } else {
        scored.sort_by(cmp);
    }

    // ---- 検証
    let verify = !ctx.view.is_projection_equivalent();
    let facts = ctx.facts();
    let mut results = vec![];
    let mut other_axis = 0usize;
    let mut inherited_skipped = 0usize;
    let mut sorted_tail = head >= scored.len();
    let mut i = 0;
    while i < scored.len() && results.len() < limit {
        if i == head && !sorted_tail {
            scored[head..].sort_by(cmp);
            sorted_tail = true;
        }
        let (score, doc) = scored[i];
        i += 1;
        if ctx.out_of_budget() {
            break;
        }
        let Some(row) = p.row_by_doc(doc) else { continue };
        if !ctx.principal.can_see(&row.visibility) {
            continue;
        }
        if spec.min_rank.is_some_and(|m| row.rank < m) {
            continue;
        }
        let mut matched_alternative = false;
        if let Some(w) = &win {
            match &row.temporal {
                Some(t) if t.axis == w.axis && w.accepts(t) => {}
                // 優先値は外れても、異説の時間が窓に入る可能性がある（possibly のときだけ）。
                Some(_) if w.mode == TimeMode::Possibly && row.temporal_alternatives.iter().any(|a| a.axis == w.axis && w.accepts(a)) => {
                    matched_alternative = true
                }
                Some(t) if t.axis != w.axis => {
                    other_axis += 1;
                    continue;
                }
                Some(_) if verify => {}
                Some(_) => continue,
                None if !w.include_undated => continue,
                None => {}
            }
        }
        if geo_filter && row.placement_inherited_from.is_some() && !include_inherited {
            inherited_skipped += 1;
            continue;
        }
        if let Some((at, r)) = &near_check {
            match row.placement.as_ref().and_then(|pl| store.frames.distance(pl, at)) {
                Some(d) if d <= *r => {}
                _ => continue,
            }
        }
        let mut summary = if verify {
            let Some(f) = facts.compute(row.canonical_id) else { continue };
            if f.branches.is_empty() || !ff.accepts(&f) {
                continue;
            }
            if let Some(w) = &win {
                match f.time.as_ref().and_then(|t| t.resolved()) {
                    Some(t) if t.axis == w.axis && w.accepts(t) => {}
                    None if w.include_undated => {}
                    _ => continue,
                }
            }
            let mut s = ctx.summary(row);
            if let Some(Ok(t)) = f.time.as_ref().map(|t| &t.result) {
                s["time"] = super::render::time_json(ctx, t, f.time.as_ref().and_then(|x| x.raw.as_deref()));
            }
            s["rank"] = json!((f.rank * 1000.0).round() / 1000.0);
            s["contested"] = json!(f.contested);
            s
        } else {
            ctx.summary(row)
        };
        if matched_alternative && !verify {
            summary["matched_alternative_time"] = json!(true);
        }
        if spec.order_by == OrderBy::Relevance {
            summary["score"] = json!((score * 1000.0).round() / 1000.0);
        }
        results.push(summary);
    }
    if inherited_skipped > 0 {
        ctx.warn(format!(
            "{inherited_skipped} candidates only have coordinates inherited from their location and were excluded; set space.include_inherited to include them"
        ));
    }
    if other_axis > 0 {
        ctx.warn(format!("{other_axis} candidates use a different time axis and were excluded (comparable: false)"));
    }
    Ok(Json::Array(results))
}

fn jaccard<T: Eq + std::hash::Hash>(a: &[T], b: &[T]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let sa: HashSet<&T> = a.iter().collect();
    let sb: HashSet<&T> = b.iter().collect();
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}

pub(super) fn similar(ctx: &QCtx, branch: BranchId, id: &str, limit: usize, w: SimilarityWeights, space: Option<&str>) -> Result<Json> {
    let kb = ctx.kb;
    let st = kb.state(branch)?;
    let p = &st.projection;
    let id = ctx.resolve(id)?;
    let target = p.row(&id).ok_or_else(|| Error::not_found(format!("{id} has no projection row yet")))?;
    let space = space.unwrap_or(DEFAULT_EMBEDDING_SPACE);
    let idx = kb.vectors.get(space).ok_or_else(|| Error::not_found(format!("embedding space `{space}`")))?;
    let qv: Option<Vec<f32>> = idx.vector(&id).map(|v| v.to_vec());
    let mut cands: HashSet<u32> = HashSet::new();
    if let Some(qv) = &qv {
        let (hits, truncated) = idx.search(qv, (limit * 10).max(100), Some(&|x| x != id), &|| ctx.deadline.expired())?;
        if truncated {
            ctx.truncated.set(true);
        }
        cands.extend(hits.iter().filter_map(|(r, _)| p.doc(r)));
    } else {
        ctx.warn(format!("{id} has no embedding in `{space}`; semantic similarity is 0"));
    }
    // 構造的に近いもの（共有する実体・作品・場所）も候補に入れる。
    for e in target.entity_ids.iter().chain(target.work_ids.iter()).take(32) {
        for key in [&p.idx_entity, &p.idx_work] {
            if let Some(bm) = key.get(e) {
                cands.extend(bm.iter().take(500));
            }
        }
        if let Some(d) = p.doc(e) {
            cands.insert(d);
        }
    }
    cands.remove(&target.doc);
    let mut scored = vec![];
    for d in cands {
        if ctx.out_of_budget() {
            break;
        }
        let Some(row) = p.row_by_doc(d) else { continue };
        if !ctx.principal.can_see(&row.visibility) {
            continue;
        }
        let semantic = match (&qv, idx.vector(&row.canonical_id)) {
            (Some(q), Some(v)) => q.iter().zip(v).map(|(a, b)| a * b).sum::<f32>() as f64,
            _ => 0.0,
        };
        let temporal = match (&target.temporal, &row.temporal) {
            (Some(a), Some(b)) if a.axis == b.axis => match (a.range.midpoint(), b.range.midpoint()) {
                (Some(x), Some(y)) => {
                    let scale = a.range.width().unwrap_or(TICKS_PER_DAY).max(b.range.width().unwrap_or(TICKS_PER_DAY)).max(TICKS_PER_DAY) as f64;
                    1.0 / (1.0 + ((x.0 as f64 - y.0 as f64).abs() / scale))
                }
                _ => 0.0,
            },
            _ => 0.0,
        };
        let spatial = match (&target.placement, &row.placement) {
            (Some(a), Some(b)) => kb.store.frames.distance(a, b).map(|dist| {
                let scale = if kb.store.frames.is_geodetic(&a.frame) { 50_000.0 } else { 100.0 };
                (-dist / scale).exp()
            }),
            _ => None,
        }
        .unwrap_or_else(|| jaccard(&target.space_ancestor_ids, &row.space_ancestor_ids));
        let entity = jaccard(&target.entity_ids, &row.entity_ids);
        let work = jaccard(&target.work_ids, &row.work_ids);
        let graph = if target.entity_ids.contains(&row.canonical_id) || row.entity_ids.contains(&target.canonical_id) {
            1.0
        } else if target.entity_ids.iter().any(|e| row.entity_ids.contains(e)) {
            0.5
        } else {
            0.0
        };
        let rank = (row.rank / 5.0).min(1.0);
        let score = w.semantic * semantic + w.temporal * temporal + w.spatial * spatial + w.entity * entity + w.work * work + w.graph * graph + w.rank * rank;
        scored.push((
            score,
            d,
            json!({ "semantic": semantic, "temporal": temporal, "spatial": spatial, "entity": entity, "work": work, "graph": graph, "rank": rank }),
        ));
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
    let out: Vec<Json> = scored
        .into_iter()
        .take(limit)
        .filter_map(|(s, d, f)| {
            let row = p.row_by_doc(d)?;
            let mut j = ctx.summary(row);
            j["score"] = json!((s * 1000.0).round() / 1000.0);
            j["features"] = f;
            Some(j)
        })
        .collect();
    Ok(json!({ "target": ctx.summary(target), "similar": out }))
}
