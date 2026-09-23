//! search 以外の読み取り操作。

use super::render::{time_json, unresolved_json};
use super::{Direction, QCtx};
use crate::projection::ProjectionRow;
use crate::store::columnar::ObservationStore;
use crate::view::{StatusSet, View};
use chronotope_core::model::*;
use chronotope_core::space::{Geometry, Placement};
use chronotope_core::time::Tick;
use chronotope_core::time::allen::possible_relations;
use chronotope_core::time::expr::TemporalExpression;
use chronotope_core::time::resolve::UnresolvedKind;
use chronotope_core::vocab::{keys, type_id};
use chronotope_core::*;
use roaring::RoaringBitmap;
use serde_json::{Value as Json, json};
use std::collections::{HashMap, HashSet};

fn parse_tick(s: &str) -> Result<Tick> {
    Tick::parse_iso(s)
}

/// Projection 行が無い（まだ Materialize されていない）場合の最小要約。
fn fallback_summary(ctx: &QCtx, id: ResourceId) -> Json {
    let s = &ctx.kb.store;
    match s.resource(id) {
        Some(r) => json!({
            "id": r.id,
            "label": r.label(ctx.lang()).unwrap_or(""),
            "types": ctx.type_keys(&r.types.iter().copied().collect::<Vec<_>>()),
            "stale": true,
            "note": "projection row not materialized yet",
        }),
        None => json!({ "id": id, "missing": true }),
    }
}

fn row_or_fallback(ctx: &QCtx, row: Option<&ProjectionRow>, id: ResourceId) -> Json {
    match row {
        Some(r) if ctx.principal.can_see(&r.visibility) => ctx.summary(r),
        Some(_) => json!({ "id": id, "withheld": "not visible to this principal" }),
        None => fallback_summary(ctx, id),
    }
}

/// possibly_same_as で結ばれた（統合されていない）Resource。
fn possibly_same(ctx: &QCtx, id: ResourceId) -> Vec<ResourceId> {
    let s = &ctx.kb.store;
    let Some(pid) = s.predicate_by_key.get(keys::POSSIBLY_SAME_AS).copied() else { return vec![] };
    let mut out = vec![];
    for (a, st) in s.claims_about(id, &ctx.view).into_iter().chain(s.claims_referencing(id, &ctx.view)) {
        if a.predicate != pid || !st.is_live() || a.polarity != Polarity::Affirmed {
            continue;
        }
        let other = if s.resolve_id(a.subject) == id { a.object.as_resource() } else { Some(a.subject) };
        if let Some(o) = other.map(|o| s.resolve_id(o)).filter(|o| *o != id) {
            if !out.contains(&o) {
                out.push(o);
            }
        }
    }
    out
}

pub(super) fn lookup(ctx: &QCtx, branch: BranchId, id: Option<&str>, ext: Option<&str>, label: Option<&str>) -> Result<Json> {
    let s = &ctx.kb.store;
    let st = ctx.kb.state(branch)?;
    let ids: Vec<(ResourceId, Option<ResourceId>)> = if let Some(id) = id {
        let raw: ResourceId = id.parse().map_err(|_| Error::invalid(format!("bad id `{id}`")))?;
        let canonical = s.resolve_id(raw);
        if !s.resources.contains_key(&canonical) {
            return Ok(json!({ "matches": [], "ambiguous": false }));
        }
        vec![(canonical, (canonical != raw).then_some(raw))]
    } else if let Some(e) = ext {
        let ext = ExternalId::parse(e).ok_or_else(|| Error::invalid(format!("bad external id `{e}`")))?;
        s.find_by_external(&ext).map(|x| vec![(x, None)]).unwrap_or_default()
    } else if let Some(l) = label {
        s.find_by_label(l).into_iter().map(|x| (x, None)).collect()
    } else {
        return Err(Error::invalid("lookup needs `id`, `external_id` or `label`"));
    };
    let matches: Vec<Json> = ids
        .iter()
        .map(|(id, from)| {
            let mut j = row_or_fallback(ctx, st.projection.row(id), *id);
            if let Some(f) = from {
                j["redirected_from"] = json!(f);
            }
            let ps = possibly_same(ctx, *id);
            if !ps.is_empty() {
                j["possibly_same_as"] = json!(ps);
            }
            j
        })
        .collect();
    let ambiguous = matches.len() > 1;
    if ambiguous {
        ctx.warn("the label matches several resources; none was chosen automatically");
    }
    Ok(json!({ "matches": matches, "ambiguous": ambiguous }))
}

pub(super) fn expand_claims(
    ctx: &QCtx,
    branch: BranchId,
    id: &str,
    predicates: &[String],
    include_history: bool,
    include_incoming: bool,
    valid_at: Option<&str>,
) -> Result<Json> {
    let kb = ctx.kb;
    let s = &kb.store;
    let st = kb.state(branch)?;
    let id = ctx.resolve(id)?;
    let res = s.resource(id).ok_or_else(|| Error::not_found(format!("resource {id}")))?;
    if !ctx.principal.can_see(&res.visibility) {
        return Err(Error::forbidden("resource is not visible to this principal"));
    }
    let view_all = View { statuses: StatusSet::all(), ..ctx.view.clone() };
    let view = if include_history { &view_all } else { &ctx.view };
    let facts = crate::facts::Facts::new(s, view, ctx.now, HashMap::new());
    let pred_filter: Vec<ResourceId> =
        predicates.iter().map(|k| s.predicate(k).map(|p| p.id).ok_or_else(|| Error::not_found(format!("predicate `{k}`")))).collect::<Result<_>>()?;
    let valid_at = valid_at.map(parse_tick).transpose()?;
    let mut claims = s.claims_about(id, view);
    claims.retain(|(a, _)| pred_filter.is_empty() || pred_filter.contains(&a.predicate));
    if let Some(t) = valid_at {
        claims.retain(|(a, _)| match &a.valid_time {
            None => true,
            Some(vt) => match facts.resolve_expression(vt, facts.reference_time(a)) {
                Ok(r) => r.range.possibly_overlaps(t, t.offset(1)),
                Err(_) => true,
            },
        });
    }
    let evals = facts.evaluate(&claims);
    let summaries = facts.summarize(&claims, &evals);
    let by_id: HashMap<AssertionId, (&Assertion, AssertionStatus)> = claims.iter().map(|(a, st)| (a.id, (*a, *st))).collect();
    let claim = |aid: &AssertionId| -> Json {
        let (a, stt) = by_id[aid];
        let mut j = ctx.claim_json(a, stt, evals.get(aid));
        if let Some(c) = st.order_conflicts.get(aid) {
            j["order_conflict"] = json!(c);
        }
        j
    };
    let mut groups = vec![];
    for sm in &summaries {
        if ctx.out_of_budget() {
            break;
        }
        let p = s.predicates.get(&sm.predicate);
        groups.push(json!({
            "predicate": p.map(|p| p.key.clone()),
            "functional": p.map(|p| p.functional),
            "contested": sm.contested,
            "preferred": sm.preferred.as_ref().map(claim),
            "alternatives": sm.values.iter().skip(1).map(claim).collect::<Vec<_>>(),
            "negated": sm.negated.iter().map(claim).collect::<Vec<_>>(),
        }));
    }
    let mut out = json!({
        "id": id,
        "label": res.label(ctx.lang()),
        "labels": res.labels,
        "types": ctx.type_keys(&res.types.iter().copied().collect::<Vec<_>>()),
        "external_ids": res.external_ids,
        "claims": groups,
    });
    let slot = facts.time_of(id);
    out["time"] = match &slot.result {
        Ok(t) => {
            let mut j = time_json(ctx, t, slot.raw.as_deref());
            j["contested"] = json!(slot.contested);
            j["depends_on"] = json!(slot.depends_on);
            j
        }
        Err(u) if u.kind == UnresolvedKind::Unknown && slot.raw.is_none() => Json::Null,
        Err(u) => {
            let mut j = unresolved_json(u, slot.raw.as_deref());
            j["depends_on"] = json!(slot.depends_on);
            j
        }
    };
    if include_history {
        let hist: Vec<Json> = claims.iter().filter(|(_, stt)| !stt.is_live()).map(|(a, stt)| ctx.claim_json(a, *stt, evals.get(&a.id))).collect();
        out["history"] = json!(hist);
    }
    if include_incoming {
        let inc: Vec<Json> = s
            .claims_referencing(id, &ctx.view)
            .into_iter()
            .take(200)
            .map(|(a, stt)| {
                let mut j = ctx.claim_json(a, stt, None);
                j["subject"] = json!({ "id": a.subject, "label": ctx.label_of(a.subject) });
                j
            })
            .collect();
        out["incoming"] = json!(inc);
    }
    let ps = possibly_same(ctx, id);
    let merged: Vec<ResourceId> = s.identity_group(id).into_iter().skip(1).collect();
    out["identity"] = json!({
        "possibly_same_as": ps,
        "merged_from": merged,
        "note": "possibly_same_as resources are treated as distinct until a curator approves a merge",
    });
    let metrics = kb.columnar.observations.metrics(id);
    if !metrics.is_empty() {
        out["observation_metrics"] = json!(metrics);
    }
    if let Some(links) = s.rows_by_resource.get(&id) {
        out["table_rows"] = json!(links);
    }
    Ok(out)
}

fn snapshot_json(ctx: &QCtx, acq: &Acquisition, source: Option<&Source>, include: bool) -> Json {
    let Some(r) = &acq.snapshot_ref else { return Json::Null };
    let s = &ctx.kb.store;
    let license = source.and_then(|x| x.license.clone()).unwrap_or_else(|| "unknown".into());
    let redistributable = s.licenses.get(&license).is_some_and(|l| l.redistributable);
    let mut j = json!({ "ref": r, "license": license, "redistributable": redistributable });
    if !include {
        return j;
    }
    if !redistributable && !(ctx.principal.curator && !ctx.principal.actor.is_ai()) {
        j["withheld"] = json!("the source license does not permit redistribution");
        return j;
    }
    match ctx.kb.objects.get(&r.hash) {
        Ok(Some(bytes)) => {
            let plain = match r.encrypted_with {
                Some(k) if bytes.len() > 12 => ctx.kb.vault.decrypt_bytes(&k, &hex::encode(&bytes[..12]), &bytes[12..]),
                Some(_) => None,
                None => Some(bytes),
            };
            match plain {
                Some(b) => {
                    const MAX: usize = 64 * 1024;
                    let cut = &b[..b.len().min(MAX)];
                    j["content"] = json!(String::from_utf8_lossy(cut));
                    j["content_truncated"] = json!(b.len() > MAX);
                }
                None => j["withheld"] = json!("snapshot key has been shredded"),
            }
        }
        Ok(None) => j["withheld"] = json!("snapshot object missing"),
        Err(e) => j["withheld"] = json!(e.to_string()),
    }
    j
}

fn acquisition_json(ctx: &QCtx, acq: &Acquisition, derivation: Option<&Derivation>, include_snapshot: bool) -> Json {
    let s = &ctx.kb.store;
    let source = s.sources.get(&acq.source).filter(|x| ctx.principal.can_see(&x.visibility));
    json!({
        "acquisition": {
            "id": acq.id,
            "acquired_at": acq.acquired_at.to_iso(),
            "acquired_by": acq.acquired_by,
            "method": acq.method,
            "locator": acq.locator,
            "content_hash": acq.content_hash,
            "recorded_at": acq.recorded_at.to_iso(),
        },
        "source": source.map(|x| json!({
            "id": x.id,
            "kind": x.kind,
            "title": x.title,
            "locator": x.locator,
            "source_time": x.source_time.as_ref().map(|t| t.raw_text.clone()),
            "origin": x.origin,
            "reliability": x.reliability,
            "provenance_root": x.root(),
            "is_reprint": x.provenance_root.is_some(),
            "license": x.license,
            "resource": x.resource,
        })),
        "derivation": derivation.map(|d| json!({
            "id": d.id,
            "extractor": d.extractor,
            "model": d.model,
            "model_version": d.model_version,
            "schema_version": d.schema_version,
            "source_span": d.source_span,
            "extracted_at": d.extracted_at.to_iso(),
            "extraction_conf": d.extraction_conf,
        })),
        "snapshot": snapshot_json(ctx, acq, source, include_snapshot),
    })
}

pub(super) fn get_acquisition(ctx: &QCtx, assertion: Option<AssertionId>, acquisition: Option<AcquisitionId>, include_snapshot: bool) -> Result<Json> {
    let s = &ctx.kb.store;
    match (assertion, acquisition) {
        (Some(aid), _) => {
            let a = s.assertions.get(&aid).ok_or_else(|| Error::not_found(format!("assertion {aid}")))?;
            if !ctx.principal.can_see(&a.visibility) {
                return Err(Error::forbidden("assertion is not visible to this principal"));
            }
            let ev: Vec<Json> = a
                .evidence
                .iter()
                .filter_map(|e| {
                    let acq = s.acquisitions.get(&e.acquisition)?;
                    let d = e.derivation.and_then(|d| s.derivations.get(&d));
                    Some(acquisition_json(ctx, acq, d, include_snapshot))
                })
                .collect();
            let mut out = json!({ "assertion": ctx.claim_json(a, a.status, None), "evidence": ev });
            if a.evidence.is_empty() {
                out["note"] = json!(format!("asserted directly by {} without an acquisition", a.asserted_by.id));
            }
            Ok(out)
        }
        (None, Some(qid)) => {
            let acq = s.acquisitions.get(&qid).ok_or_else(|| Error::not_found(format!("acquisition {qid}")))?;
            Ok(acquisition_json(ctx, acq, None, include_snapshot))
        }
        _ => Err(Error::invalid("get_acquisition needs `assertion` or `acquisition`")),
    }
}

pub(super) fn temporal_relation(ctx: &QCtx, branch: BranchId, a: &str, b: &str) -> Result<Json> {
    let st = ctx.kb.state(branch)?;
    let (ia, ib) = (ctx.resolve(a)?, ctx.resolve(b)?);
    let facts = ctx.facts();
    let (ta, tb) = (facts.time_of(ia), facts.time_of(ib));
    let side = |id: ResourceId, t: &crate::facts::TimeSlot| {
        json!({
            "id": id,
            "label": ctx.label_of(id),
            "time": match &t.result {
                Ok(r) => time_json(ctx, r, t.raw.as_deref()),
                Err(u) => unresolved_json(u, t.raw.as_deref()),
            },
        })
    };
    let mut out = json!({ "a": side(ia, &ta), "b": side(ib, &tb) });
    let absolute = match (&ta.result, &tb.result) {
        (Ok(x), Ok(y)) if x.axis != y.axis => {
            out["comparable"] = json!(false);
            out["reason"] = json!(format!("different time axes: `{}` vs `{}`", x.axis, y.axis));
            return Ok(out);
        }
        (Ok(x), Ok(y)) if x.recurrence.is_none() && y.recurrence.is_none() => Some(possible_relations(&x.range, &y.range)),
        _ => None,
    };
    let graph = if ctx.view.is_projection_equivalent() {
        st.order.relation(&ia, &ib)
    } else {
        ctx.warn("the order graph reflects the default view; only absolute times were used for this view");
        None
    };
    let (rels, basis) = match (absolute, graph) {
        (Some(x), Some(g)) => (x.intersect(g), "absolute_time+order_graph"),
        (Some(x), None) => (x, "absolute_time"),
        (None, Some(g)) => (g, "order_graph"),
        (None, None) => {
            let incomparable = [&ta, &tb].iter().any(|t| matches!(&t.result, Err(u) if u.kind == UnresolvedKind::Incomparable));
            out["comparable"] = json!(!incomparable);
            out["determined"] = json!(false);
            out["relations"] = json!(chronotope_core::time::allen::AllenSet::ALL.names());
            out["reason"] = json!("no absolute time or ordering constraints relate these resources");
            return Ok(out);
        }
    };
    out["comparable"] = json!(true);
    out["basis"] = json!(basis);
    out["contradiction"] = json!(rels.is_empty());
    out["relations"] = json!(rels.names());
    out["certain"] = json!(rels.certain().map(|r| r.name()));
    out["determined"] = json!(rels.len() < 13);
    Ok(out)
}

pub(super) fn timeline(
    ctx: &QCtx,
    branch: BranchId,
    entity: Option<&str>,
    work: Option<&str>,
    place: Option<&str>,
    types: &[String],
    limit: usize,
) -> Result<Json> {
    let s = &ctx.kb.store;
    let st = ctx.kb.state(branch)?;
    let p = &st.projection;
    let mut cand: RoaringBitmap = p.live.clone();
    if let Some(e) = entity {
        let id = ctx.resolve(e)?;
        let mut bm = p.bitmap_of(&p.idx_entity, &id);
        if let Some(d) = p.doc(&id) {
            bm.insert(d);
        }
        cand &= bm;
    }
    if let Some(w) = work {
        cand &= p.bitmap_of(&p.idx_work, &ctx.resolve(w)?);
    }
    if let Some(pl) = place {
        cand &= p.bitmap_of(&p.idx_space_ancestor, &ctx.resolve(pl)?);
    }
    let types: Vec<ResourceId> = if types.is_empty() {
        vec![type_id("Event")]
    } else {
        types.iter().map(|t| s.type_id(t).ok_or_else(|| Error::not_found(format!("type `{t}`")))).collect::<Result<_>>()?
    };
    let mut tb = RoaringBitmap::new();
    for t in &types {
        tb |= p.bitmap_of(&p.idx_type, t);
    }
    cand &= tb;
    let mut dated: HashMap<String, Vec<(Tick, Tick, u32)>> = HashMap::new();
    let mut relative = vec![];
    let mut unplaced = vec![];
    for d in cand.iter() {
        if ctx.out_of_budget() {
            break;
        }
        let Some(row) = p.row_by_doc(d) else { continue };
        if !ctx.principal.can_see(&row.visibility) {
            continue;
        }
        match (&row.temporal, st.order.label(&row.canonical_id)) {
            (Some(t), _) if t.recurrence.is_none() => dated.entry(t.axis.clone()).or_default().push((t.range.earliest_start, t.range.latest_end, d)),
            (_, Some(l)) => relative.push((l.start_ord, l.end_ord, d)),
            _ => unplaced.push(row.canonical_id),
        }
    }
    let mut axes = serde_json::Map::new();
    for (axis, mut v) in dated {
        v.sort();
        axes.insert(axis, json!(v.iter().take(limit).filter_map(|(_, _, d)| p.row_by_doc(*d).map(|r| ctx.summary(r))).collect::<Vec<_>>()));
    }
    relative.sort();
    let rel: Vec<Json> = relative
        .iter()
        .take(limit)
        .filter_map(|(so, eo, d)| {
            let mut j = ctx.summary(p.row_by_doc(*d)?);
            j["order_label"] = json!({ "start_ord": so, "end_ord": eo });
            Some(j)
        })
        .collect();
    Ok(json!({
        "axes": axes,
        "relative_order": rel,
        "unplaced": unplaced.iter().take(limit).collect::<Vec<_>>(),
        "note": "each time axis is ordered by absolute time; relative_order is one linear extension of the partial order (only pairs linked by constraints are actually ordered)",
    }))
}

pub(super) fn neighbors(ctx: &QCtx, branch: BranchId, id: &str, predicates: &[String], dir: Direction, depth: usize, limit: usize) -> Result<Json> {
    let s = &ctx.kb.store;
    let st = ctx.kb.state(branch)?;
    let id = ctx.resolve(id)?;
    let filter: Vec<ResourceId> =
        predicates.iter().map(|k| s.predicate(k).map(|p| p.id).ok_or_else(|| Error::not_found(format!("predicate `{k}`")))).collect::<Result<_>>()?;
    let mut seen: Vec<ResourceId> = vec![id];
    let mut frontier = vec![id];
    let mut edges = vec![];
    'outer: for _ in 0..depth.max(1) {
        let mut next = vec![];
        for n in frontier {
            let mut add = |a: &Assertion, stt: AssertionStatus, from: ResourceId, to: ResourceId| {
                edges.push(json!({ "from": from, "predicate": s.predicates.get(&a.predicate).map(|p| p.key.clone()), "to": to, "assertion": a.id, "status": stt, "polarity": a.polarity }));
                if !seen.contains(&to) {
                    seen.push(to);
                    next.push(to);
                }
                if !seen.contains(&from) {
                    seen.push(from);
                    next.push(from);
                }
            };
            if matches!(dir, Direction::Out | Direction::Both) {
                for (a, stt) in s.claims_about(n, &ctx.view) {
                    if let (true, Some(o)) = (filter.is_empty() || filter.contains(&a.predicate), a.object.as_resource()) {
                        add(a, stt, n, s.resolve_id(o));
                    }
                }
            }
            if matches!(dir, Direction::In | Direction::Both) {
                for (a, stt) in s.claims_referencing(n, &ctx.view) {
                    if filter.is_empty() || filter.contains(&a.predicate) {
                        add(a, stt, s.resolve_id(a.subject), n);
                    }
                }
            }
            if edges.len() >= limit || ctx.out_of_budget() {
                if edges.len() >= limit {
                    ctx.truncated.set(true);
                }
                break 'outer;
            }
        }
        frontier = next;
    }
    edges.truncate(limit);
    let nodes: Vec<Json> = seen.iter().map(|n| row_or_fallback(ctx, st.projection.row(n), *n)).collect();
    Ok(json!({ "root": id, "nodes": nodes, "edges": edges }))
}

pub(super) fn conflicts(ctx: &QCtx, branch: BranchId, id: Option<&str>, limit: usize) -> Result<Json> {
    let s = &ctx.kb.store;
    let st = ctx.kb.state(branch)?;
    let facts = ctx.facts();
    let describe = |rid: ResourceId| -> Option<Json> {
        let f = facts.compute(rid)?;
        let claims: HashMap<AssertionId, (&Assertion, AssertionStatus)> = s.claims_about(rid, &ctx.view).into_iter().map(|(a, stt)| (a.id, (a, stt))).collect();
        let contested: Vec<Json> = f
            .summaries
            .iter()
            .filter(|x| x.contested)
            .map(|x| {
                let vals: Vec<Json> = x
                    .values
                    .iter()
                    .chain(x.negated.iter())
                    .filter_map(|aid| claims.get(aid))
                    .map(|(a, stt)| ctx.claim_json(a, *stt, f.evals.get(&a.id)))
                    .collect();
                json!({ "predicate": s.predicates.get(&x.predicate).map(|p| p.key.clone()), "preferred": x.preferred, "claims": vals })
            })
            .collect();
        let order: Vec<Json> = st
            .order_conflicts
            .iter()
            .filter(|(aid, _)| {
                s.assertions.get(aid).is_some_and(|a| s.resolve_id(a.subject) == rid || a.object.as_resource().map(|o| s.resolve_id(o)) == Some(rid))
            })
            .map(|(aid, msg)| json!({ "assertion": aid, "error": msg }))
            .collect();
        if contested.is_empty() && order.is_empty() {
            return None;
        }
        Some(json!({ "id": rid, "label": ctx.label_of(rid), "contested_predicates": contested, "temporal_order_conflicts": order }))
    };
    if let Some(id) = id {
        let rid = ctx.resolve(id)?;
        return Ok(json!({ "resources": describe(rid).into_iter().collect::<Vec<_>>() }));
    }
    let p = &st.projection;
    let mut rows: Vec<&ProjectionRow> = p.contested.iter().filter_map(|d| p.row_by_doc(d)).collect();
    rows.sort_by(|a, b| b.rank.partial_cmp(&a.rank).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = vec![];
    let mut covered = HashSet::new();
    for r in rows {
        if out.len() >= limit || ctx.out_of_budget() {
            break;
        }
        if let Some(j) = describe(r.canonical_id) {
            covered.insert(r.canonical_id);
            out.push(j);
        }
    }
    let order_only: Vec<Json> = st
        .order_conflicts
        .iter()
        .filter(|(aid, _)| s.assertions.get(aid).is_some_and(|a| !covered.contains(&s.resolve_id(a.subject))))
        .take(limit)
        .map(|(aid, msg)| json!({ "assertion": aid, "error": msg }))
        .collect();
    Ok(json!({ "resources": out, "temporal_order_conflicts": order_only }))
}

pub(super) fn observations(ctx: &QCtx, target: &str, metric: Option<&str>, from: Option<&str>, to: Option<&str>, limit: usize) -> Result<Json> {
    let id = ctx.resolve(target)?;
    let from = from.map(parse_tick).transpose()?.unwrap_or(Tick::NEG_INF);
    let to = to.map(parse_tick).transpose()?.unwrap_or(Tick::POS_INF);
    let s = &ctx.kb.store;
    let mut obs = ctx.kb.columnar.observations.range(id, metric, from, to, usize::MAX);
    if let Some(t) = ctx.view.as_known_at {
        obs.retain(|o| o.acquisition.and_then(|a| s.acquisitions.get(&a)).map(|a| a.acquired_at).unwrap_or(o.observed_at) <= t);
    }
    let chain: Vec<BranchId> = ctx.view.chain.iter().map(|(b, _)| *b).collect();
    obs.retain(|o| chain.contains(&o.branch));
    let total = obs.len();
    obs.truncate(limit);
    let rows: Vec<Json> = obs
        .iter()
        .map(|o| json!({ "metric": o.metric, "observed_at": o.observed_at.to_iso(), "value": o.value, "unit": o.unit, "acquisition": o.acquisition }))
        .collect();
    Ok(json!({ "target": id, "count": total, "observations": rows }))
}

pub(super) fn position_at(ctx: &QCtx, target: &str, at: &str) -> Result<Json> {
    let id = ctx.resolve(target)?;
    let t = parse_tick(at)?;
    let trajs = ctx.kb.columnar.trajectories.get(&id).map(Vec::as_slice).unwrap_or(&[]);
    for tr in trajs {
        if let TrajectoryStorage::External { uri } = &tr.storage {
            if tr.samples.is_empty() {
                ctx.warn(format!("trajectory {} is stored externally at {uri}", tr.id));
                continue;
            }
        }
        if let Some((a, b)) = tr.time_span() {
            if a <= t && t <= b {
                return Ok(json!({ "target": id, "at": t.to_iso(), "trajectory": tr.id, "interpolation": tr.interpolation, "position": tr.position_at(t) }));
            }
        }
    }
    Ok(json!({ "target": id, "at": t.to_iso(), "position": null, "reason": "no trajectory covers this time" }))
}

pub(super) fn sequence(ctx: &QCtx, branch: BranchId, scope: &str, kind: Option<&str>) -> Result<Json> {
    let s = &ctx.kb.store;
    let st = ctx.kb.state(branch)?;
    let scope = ctx.resolve(scope)?;
    let kind = kind.map(SequenceKind::parse);
    let chain: Vec<BranchId> = ctx.view.chain.iter().map(|(b, _)| *b).collect();
    let mut seqs: Vec<&Sequence> = s
        .sequences
        .values()
        .filter(|q| q.scope == scope && kind.as_ref().is_none_or(|k| &q.kind == k) && chain.contains(&q.branch))
        .filter(|q| ctx.view.canon.is_none() || q.canon.is_none() || q.canon == ctx.view.canon)
        .collect();
    seqs.sort_by_key(|q| q.id);
    let out: Vec<Json> = seqs
        .iter()
        .map(|q| {
            json!({
                "id": q.id,
                "kind": q.kind,
                "label": q.label,
                "canon": q.canon,
                "items": q.items.iter().enumerate().map(|(i, it)| {
                    let mut j = row_or_fallback(ctx, st.projection.row(&s.resolve_id(*it)), s.resolve_id(*it));
                    j["position"] = json!(i + 1);
                    j
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({ "scope": scope, "sequences": out }))
}

pub(super) fn resolve_temporal(ctx: &QCtx, text: &str, calendar: Option<&str>, reference: Option<&str>) -> Result<Json> {
    let cal = calendar.unwrap_or("gregorian");
    if ctx.kb.store.calendar(cal).is_none() {
        return Err(Error::not_found(format!("calendar `{cal}`")));
    }
    let expr = TemporalExpression::parse(text, cal);
    let reference = reference.map(parse_tick).transpose()?;
    let facts = ctx.facts();
    let mut out = json!({ "raw_text": text, "calendar": cal, "ast": expr.ast, "anchors": expr.anchors(), "needs_reference": expr.ast.needs_reference() });
    match facts.resolve_expression(&expr, reference) {
        Ok(r) => out["resolved"] = time_json(ctx, &r, Some(text)),
        Err(u) => out["unresolved"] = json!(u),
    }
    Ok(out)
}

pub(super) fn resolve_spatial(ctx: &QCtx, branch: BranchId, name: Option<&str>, point: Option<&Placement>, limit: usize) -> Result<Json> {
    let s = &ctx.kb.store;
    let st = ctx.kb.state(branch)?;
    let p = &st.projection;
    let place_t = type_id("Place");
    let places = p.bitmap_of(&p.idx_type, &place_t);
    let mut cands: Vec<(f64, ResourceId)> = vec![];
    if let Some(n) = name {
        for id in s.find_by_label(n) {
            cands.push((2.0, id));
        }
        if let Some(bm) = p.idx_text.search(n) {
            for d in (bm & &places).iter().take(limit * 4) {
                if let Some(r) = p.row_by_doc(d) {
                    if !cands.iter().any(|(_, x)| *x == r.canonical_id) {
                        cands.push((1.0 + r.rank / 10.0, r.canonical_id));
                    }
                }
            }
        }
    }
    if let Some(pt) = point {
        let c = pt.geometry.centroid();
        let q = Placement { frame: pt.frame, geometry: Geometry::Point { at: c } };
        let bm =
            p.idx_geo.intersecting(&s.frames, &q).ok_or_else(|| Error::Incomparable(format!("frame {} is unknown or has no coordinate mapping", pt.frame)))?;
        for d in (bm & &places).iter() {
            if let Some(r) = p.row_by_doc(d) {
                let area = r.placement.as_ref().map(|pl| {
                    let (a, b) = pl.geometry.bbox();
                    (b.x - a.x).abs() * (b.y - a.y).abs()
                });
                // 小さい（具体的な）場所ほど上位。
                cands.push((1.0 / (1.0 + area.unwrap_or(f64::MAX)), r.canonical_id));
            }
        }
    }
    if name.is_none() && point.is_none() {
        return Err(Error::invalid("resolve_spatial needs `name` or `point`"));
    }
    cands.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let out: Vec<Json> = cands
        .iter()
        .take(limit)
        .map(|(_, id)| {
            let mut j = row_or_fallback(ctx, p.row(id), *id);
            if let Some(r) = p.row(id) {
                j["ancestors"] = json!(r.space_ancestor_ids.iter().filter(|x| *x != id).map(|x| ctx.label_of(*x)).collect::<Vec<_>>());
            }
            j
        })
        .collect();
    let ambiguous = out.len() > 1;
    Ok(json!({ "candidates": out, "ambiguous": ambiguous, "note": "no candidate is chosen automatically" }))
}

pub(super) fn merge_candidates(ctx: &QCtx, limit: usize) -> Result<Json> {
    let s = &ctx.kb.store;
    let mut proposals: Vec<&MergeProposal> = s.merge_proposals.values().filter(|m| m.status == MergeStatus::Proposed).collect();
    proposals.sort_by_key(|m| m.proposed_at);
    let props: Vec<Json> = proposals
        .iter()
        .take(limit)
        .map(|m| json!({ "proposal": m.id, "from": { "id": m.from, "label": ctx.label_of(m.from) }, "into": { "id": m.into, "label": ctx.label_of(m.into) }, "reason": m.reason, "proposed_by": m.proposed_by, "evidence": m.evidence }))
        .collect();
    let mut psa = vec![];
    if let Some(pid) = s.predicate_by_key.get(keys::POSSIBLY_SAME_AS) {
        for aid in s.by_predicate.get(pid).into_iter().flatten() {
            if psa.len() >= limit {
                break;
            }
            let a = &s.assertions[aid];
            let Some(o) = a.object.as_resource() else { continue };
            if s.effective_status(a, &ctx.view).is_none() || s.resolve_id(a.subject) == s.resolve_id(o) {
                continue;
            }
            psa.push(json!({ "assertion": a.id, "a": { "id": a.subject, "label": ctx.label_of(a.subject) }, "b": { "id": o, "label": ctx.label_of(o) }, "status": a.status }));
        }
    }
    Ok(json!({ "merge_proposals": props, "possibly_same_as": psa }))
}

pub(super) fn derived_by(ctx: &QCtx, extractor: Option<&str>, model: Option<&str>, version: Option<&str>, limit: usize) -> Result<Json> {
    let s = &ctx.kb.store;
    let mut out = vec![];
    let mut ds: Vec<&Derivation> = s
        .derivations
        .values()
        .filter(|d| {
            extractor.is_none_or(|x| d.extractor == x)
                && model.is_none_or(|x| d.model.as_deref() == Some(x))
                && version.is_none_or(|x| d.model_version.as_deref() == Some(x))
        })
        .collect();
    ds.sort_by_key(|d| d.extracted_at);
    'outer: for d in ds {
        for aid in s.by_derivation.get(&d.id).into_iter().flatten() {
            if out.len() >= limit || ctx.out_of_budget() {
                break 'outer;
            }
            let a = &s.assertions[aid];
            if !ctx.principal.can_see(&a.visibility) {
                continue;
            }
            let mut j = ctx.claim_json(a, a.status, None);
            j["subject"] = json!({ "id": a.subject, "label": ctx.label_of(a.subject) });
            j["derivation"] =
                json!({ "id": d.id, "extractor": d.extractor, "model": d.model, "model_version": d.model_version, "extracted_at": d.extracted_at.to_iso() });
            out.push(j);
        }
    }
    Ok(json!(out))
}

pub(super) fn vocabulary(ctx: &QCtx, kind: Option<&str>) -> Json {
    let s = &ctx.kb.store;
    let key_of = |id: &ResourceId| s.predicates.get(id).map(|p| p.key.clone()).or_else(|| s.types.get(id).map(|t| t.key.clone()));
    let mut out = json!({});
    if kind.is_none_or(|k| k == "predicates") {
        let mut ps: Vec<&PredicateDef> = s.predicates.values().collect();
        ps.sort_by(|a, b| a.key.cmp(&b.key));
        out["predicates"] = json!(
            ps.iter()
                .map(|p| json!({
                    "key": p.key, "id": p.id, "status": p.status, "role": p.role, "range": p.range,
                    "domain": p.domain.iter().filter_map(key_of).collect::<Vec<_>>(),
                    "inverse": p.inverse.as_ref().and_then(key_of), "functional": p.functional, "transitive": p.transitive, "symmetric": p.symmetric,
                    "labels": p.labels, "mappings": p.mappings,
                }))
                .collect::<Vec<_>>()
        );
    }
    if kind.is_none_or(|k| k == "types") {
        let mut ts: Vec<&TypeDef> = s.types.values().collect();
        ts.sort_by(|a, b| a.key.cmp(&b.key));
        out["types"] = json!(ts
            .iter()
            .map(|t| json!({ "key": t.key, "id": t.id, "status": t.status, "parents": t.parents.iter().filter_map(key_of).collect::<Vec<_>>(), "labels": t.labels, "mappings": t.mappings }))
            .collect::<Vec<_>>());
    }
    if kind.is_none_or(|k| k == "calendars") {
        out["calendars"] = json!(s.calendars.values().collect::<Vec<_>>());
    }
    if kind.is_none_or(|k| k == "frames") {
        out["frames"] = json!(s.frames.all().collect::<Vec<_>>());
    }
    out
}

pub(super) fn table_rows(ctx: &QCtx, table: &TableId, limit: usize) -> Result<Json> {
    let s = &ctx.kb.store;
    let def = s.tables.get(table).ok_or_else(|| Error::not_found(format!("table {table}")))?;
    let rows = ctx.kb.columnar.rows(table);
    let key_of = |r: &serde_json::Map<String, Json>| -> String {
        def.primary_key
            .iter()
            .map(|k| r.get(k).map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("|")
    };
    let out: Vec<Json> = rows
        .iter()
        .take(limit)
        .map(|r| {
            let k = key_of(r);
            let links = s.row_links.get(&(*table, k.clone())).cloned().unwrap_or_default();
            json!({ "row_key": k, "row": r, "resources": links })
        })
        .collect();
    Ok(json!({ "table": def, "total_rows": rows.len(), "rows": out }))
}
