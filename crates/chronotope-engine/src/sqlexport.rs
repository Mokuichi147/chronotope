//! PostgreSQL / Citus スキーマ（`sql/migrations`）向けの COPY 形式エクスポート。
//! Canonical 層とブランチの Projection を、そのまま `psql -f` で流し込める SQL として書き出す。

use crate::kb::KnowledgeBase;
use chronotope_core::model::*;
use chronotope_core::time::Tick;
use chronotope_core::*;
use std::io::Write;

type F = Option<String>;

fn s(v: impl ToString) -> F {
    Some(v.to_string())
}

fn uuid(u: &uuid::Uuid) -> F {
    Some(u.to_string())
}

fn tick(t: Tick) -> F {
    t.is_finite().then(|| t.0.to_string())
}

fn json<T: serde::Serialize>(v: &T) -> F {
    Some(serde_json::to_string(v).unwrap_or_else(|_| "null".into()))
}

fn uuid_arr<'a>(it: impl IntoIterator<Item = &'a uuid::Uuid>) -> F {
    Some(format!("{{{}}}", it.into_iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",")))
}

fn text_arr<'a>(it: impl IntoIterator<Item = &'a String>) -> F {
    let items: Vec<String> = it.into_iter().map(|t| format!("\"{}\"", t.replace('\\', "\\\\").replace('"', "\\\""))).collect();
    Some(format!("{{{}}}", items.join(",")))
}

fn range(a: Tick, b: Tick) -> F {
    if a.is_finite() && b.is_finite() && a >= b {
        return s("empty");
    }
    Some(format!("[{},{})", tick(a).unwrap_or_default(), tick(b).unwrap_or_default()))
}

fn vis(v: &Visibility) -> (F, F, F) {
    match v {
        Visibility::Public => (s("public"), s("{}"), None),
        Visibility::Groups { groups } => (s("groups"), text_arr(groups.iter()), None),
        Visibility::Private { owner } => (s("private"), s("{}"), s(owner)),
    }
}

fn csv_field(f: &F) -> String {
    match f {
        None => String::new(),
        Some(v) if v.is_empty() || v.contains([',', '"', '\n', '\r']) || v == "\\." => format!("\"{}\"", v.replace('"', "\"\"")),
        Some(v) => v.clone(),
    }
}

struct Copy<'a> {
    w: &'a mut dyn Write,
}

impl Copy<'_> {
    fn table(&mut self, name: &str, cols: &[&str], rows: impl IntoIterator<Item = Vec<F>>) -> std::io::Result<usize> {
        writeln!(self.w, "COPY chronotope.{name} ({}) FROM STDIN WITH (FORMAT csv);", cols.join(", "))?;
        let mut n = 0;
        for r in rows {
            debug_assert_eq!(r.len(), cols.len(), "{name}");
            writeln!(self.w, "{}", r.iter().map(csv_field).collect::<Vec<_>>().join(","))?;
            n += 1;
        }
        writeln!(self.w, "\\.")?;
        Ok(n)
    }
}

impl KnowledgeBase {
    /// Canonical 層と `branch` の Projection を COPY 文として書き出す。
    pub fn export_sql(&self, out: &mut dyn Write, branch: BranchId) -> Result<()> {
        let io = |e: std::io::Error| Error::Storage(e.to_string());
        let st = &self.store;
        let proj = &self.state(branch)?.projection;
        writeln!(out, "-- chronotope export (source_revision {})\nBEGIN;\nSET search_path = chronotope, public;", st.head_seq).map_err(io)?;
        let mut c = Copy { w: out };
        c.table(
            "revision",
            &["id", "seq", "branch_id", "actor_id", "actor_kind", "message", "committed_at", "commands"],
            st.revisions.iter().map(|r| {
                vec![
                    uuid(&r.id.0),
                    s(r.seq),
                    uuid(&r.branch.0),
                    s(&r.actor.id),
                    json(&r.actor.kind).map(|x| x.trim_matches('"').to_string()),
                    r.message.clone(),
                    tick(r.committed_at),
                    json(&r.commands),
                ]
            }),
        )
        .map_err(io)?;
        c.table(
            "branch",
            &["id", "name", "parent_id", "fork_seq", "kind", "created_at", "description"],
            st.branches.values().map(|b| {
                vec![uuid(&b.id.0), s(&b.name), b.parent.and_then(|p| uuid(&p.0)), s(b.fork_seq), s("data"), s(b.created_at.0), b.description.clone()]
            }),
        )
        .map_err(io)?;
        let status = |v: VocabStatus| s(format!("{v:?}").to_lowercase());
        c.table(
            "type_def",
            &["id", "key", "parents", "labels", "status", "mask_bit"],
            st.types.values().map(|t| {
                vec![
                    uuid(&t.id.0),
                    s(&t.key),
                    uuid_arr(t.parents.iter().map(|p| &p.0)),
                    json(&t.labels),
                    status(t.status),
                    st.type_bits.get(&t.id).map(|b| b.to_string()),
                ]
            }),
        )
        .map_err(io)?;
        c.table(
            "predicate",
            &["id", "key", "labels", "domain", "range_spec", "inverse_id", "is_transitive", "is_symmetric", "is_functional", "status", "role", "allen"],
            st.predicates.values().map(|p| {
                vec![
                    uuid(&p.id.0),
                    s(&p.key),
                    json(&p.labels),
                    uuid_arr(p.domain.iter().map(|d| &d.0)),
                    json(&p.range),
                    p.inverse.and_then(|i| uuid(&i.0)),
                    s(p.transitive),
                    s(p.symmetric),
                    s(p.functional),
                    status(p.status),
                    json(&p.role).map(|x| x.trim_matches('"').to_string()),
                    p.allen.clone(),
                ]
            }),
        )
        .map_err(io)?;
        let mut maps: Vec<Vec<F>> = vec![];
        let mut seen = std::collections::HashSet::new();
        for (owner, ms) in st.predicates.values().map(|p| (p.id, &p.mappings)).chain(st.types.values().map(|t| (t.id, &t.mappings))) {
            for m in ms {
                if seen.insert((owner, m.vocabulary.clone(), m.iri.clone())) {
                    maps.push(vec![uuid(&owner.0), s(&m.vocabulary), s(&m.iri), json(&m.match_kind).map(|x| x.trim_matches('"').to_string())]);
                }
            }
        }
        c.table("vocab_mapping", &["owner_id", "vocabulary", "iri", "match_kind"], maps).map_err(io)?;
        c.table("calendar_frame", &["key", "name", "axis", "kind"], st.calendars.values().map(|k| vec![s(&k.key), s(&k.name), s(&k.axis), json(&k.kind)]))
            .map_err(io)?;
        c.table(
            "spatial_frame",
            &["id", "name", "parent_id", "coordinate_system", "dimensionality", "unit", "transform"],
            st.frames.all().map(|f| {
                vec![
                    uuid(&f.id.0),
                    s(&f.name),
                    f.parent.and_then(|p| uuid(&p.0)),
                    json(&f.coordinate_system),
                    s(f.dimensionality),
                    f.unit.clone(),
                    f.transform.map(|t| format!("{{{}}}", t.m.iter().flatten().map(|x| x.to_string()).collect::<Vec<_>>().join(","))),
                ]
            }),
        )
        .map_err(io)?;
        c.table(
            "license",
            &["key", "name", "url", "redistributable", "attribution_required"],
            st.licenses.values().map(|l| vec![s(&l.key), s(&l.name), l.url.clone(), s(l.redistributable), s(l.attribution_required)]),
        )
        .map_err(io)?;
        c.table(
            "resource",
            &[
                "id",
                "labels",
                "descriptions",
                "visibility_level",
                "visibility_groups",
                "visibility_owner",
                "license",
                "created_at",
                "created_by",
                "created_revision",
            ],
            st.resources.values().map(|r| {
                let (l, g, o) = vis(&r.visibility);
                vec![
                    uuid(&r.id.0),
                    json(&r.labels),
                    json(&r.descriptions),
                    l,
                    g,
                    o,
                    r.license.clone(),
                    tick(r.created_at),
                    s(&r.created_by.id),
                    uuid(&r.created_revision.0),
                ]
            }),
        )
        .map_err(io)?;
        c.table("resource_type", &["resource_id", "type_id"], st.resources.values().flat_map(|r| r.types.iter().map(move |t| vec![uuid(&r.id.0), uuid(&t.0)])))
            .map_err(io)?;
        let mut labels = vec![];
        let mut seen = std::collections::HashSet::new();
        for r in st.resources.values() {
            for l in &r.labels {
                let norm = crate::text::normalize_label(&l.text);
                if seen.insert((norm.clone(), r.id, l.text.clone(), l.lang.clone())) {
                    labels.push(vec![
                        s(norm),
                        uuid(&r.id.0),
                        s(&l.text),
                        s(l.lang.clone().unwrap_or_default()),
                        json(&l.kind).map(|x| x.trim_matches('"').to_string()),
                    ]);
                }
            }
        }
        c.table("label_index", &["norm", "resource_id", "text", "lang", "kind"], labels).map_err(io)?;
        c.table("external_id", &["scheme", "value", "resource_id"], st.external_index.iter().map(|(e, r)| vec![s(&e.scheme), s(&e.value), uuid(&r.0)]))
            .map_err(io)?;
        c.table(
            "identity_redirect",
            &["from_id", "to_id", "revision", "approved_by", "at", "reason"],
            st.redirects.values().map(|r| vec![uuid(&r.from.0), uuid(&r.to.0), uuid(&r.revision.0), s(&r.approved_by.id), tick(r.at), r.reason.clone()]),
        )
        .map_err(io)?;
        c.table(
            "source",
            &[
                "id",
                "kind",
                "locator",
                "locator_key",
                "title",
                "resource_id",
                "source_time",
                "origin",
                "reliability",
                "provenance_root",
                "license",
                "visibility_level",
                "visibility_groups",
                "registered_at",
            ],
            st.sources.values().map(|x| {
                let (l, g, _) = vis(&x.visibility);
                vec![
                    uuid(&x.id.0),
                    json(&x.kind).map(|k| k.trim_matches('"').to_string()),
                    json(&x.locator),
                    s(x.locator.key()),
                    x.title.clone(),
                    x.resource.and_then(|r| uuid(&r.0)),
                    x.source_time.as_ref().and_then(json),
                    json(&x.origin).map(|k| k.trim_matches('"').to_string()),
                    x.reliability.map(|r| r.to_string()),
                    x.provenance_root.and_then(|r| uuid(&r.0)),
                    x.license.clone(),
                    l,
                    g,
                    tick(x.registered_at),
                ]
            }),
        )
        .map_err(io)?;
        c.table(
            "acquisition",
            &[
                "source_id",
                "id",
                "acquired_at",
                "acquired_by",
                "acquired_by_kind",
                "method",
                "locator",
                "content_hash",
                "snapshot_hash",
                "snapshot_size",
                "media_type",
                "encrypted_with",
                "recorded_at",
            ],
            st.acquisitions.values().map(|a| {
                vec![
                    uuid(&a.source.0),
                    uuid(&a.id.0),
                    tick(a.acquired_at),
                    s(&a.acquired_by.id),
                    json(&a.acquired_by.kind).map(|k| k.trim_matches('"').to_string()),
                    json(&a.method).map(|k| k.trim_matches('"').to_string()),
                    json(&a.locator),
                    a.content_hash.as_ref().map(|h| h.0.clone()),
                    a.snapshot_ref.as_ref().map(|r| r.hash.0.clone()),
                    a.snapshot_ref.as_ref().map(|r| r.size.to_string()),
                    a.snapshot_ref.as_ref().and_then(|r| r.media_type.clone()),
                    a.snapshot_ref.as_ref().and_then(|r| r.encrypted_with).and_then(|k| uuid(&k.0)),
                    tick(a.recorded_at),
                ]
            }),
        )
        .map_err(io)?;
        c.table(
            "derivation",
            &["source_id", "id", "acquisition_id", "extractor", "model", "model_version", "schema_version", "source_span", "extracted_at", "extraction_conf"],
            st.derivations.values().filter_map(|d| {
                let src = st.acquisitions.get(&d.acquisition)?.source;
                Some(vec![
                    uuid(&src.0),
                    uuid(&d.id.0),
                    uuid(&d.acquisition.0),
                    s(&d.extractor),
                    d.model.clone(),
                    d.model_version.clone(),
                    d.schema_version.clone(),
                    d.source_span.as_ref().and_then(json),
                    tick(d.extracted_at),
                    d.extraction_conf.map(|x| x.to_string()),
                ])
            }),
        )
        .map_err(io)?;
        let status_s = |x: AssertionStatus| json(&x).map(|k| k.trim_matches('"').to_string());
        c.table(
            "assertion",
            &[
                "subject_id",
                "id",
                "predicate_id",
                "object_resource_id",
                "object_value",
                "polarity",
                "valid_time",
                "spatial_scope",
                "branch_id",
                "timeline_id",
                "canon_id",
                "status",
                "source_reliability",
                "extraction_conf",
                "specificity",
                "human_verified",
                "supersedes",
                "superseded_by",
                "overrides",
                "asserted_by",
                "asserted_by_kind",
                "created_revision",
                "created_seq",
                "created_at",
                "first_known_at",
                "visibility_level",
                "visibility_groups",
                "visibility_owner",
                "license",
                "note",
            ],
            st.assertions.values().map(|a| {
                let (l, g, o) = vis(&a.visibility);
                let u = |x: Option<ResourceId>| x.and_then(|r| uuid(&r.0));
                let ua = |x: Option<AssertionId>| x.and_then(|r| uuid(&r.0));
                vec![
                    uuid(&a.subject.0),
                    uuid(&a.id.0),
                    uuid(&a.predicate.0),
                    u(a.object.as_resource()),
                    json(&a.object),
                    s(if a.polarity == Polarity::Affirmed { 1 } else { -1 }),
                    a.valid_time.as_ref().and_then(json),
                    u(a.spatial_scope),
                    uuid(&a.branch.0),
                    u(a.timeline),
                    u(a.canon),
                    status_s(a.status),
                    a.confidence.source_reliability.map(|x| x.to_string()),
                    a.confidence.extraction_conf.map(|x| x.to_string()),
                    a.confidence.specificity.map(|x| x.to_string()),
                    s(a.confidence.human_verified),
                    ua(a.supersedes),
                    ua(a.superseded_by),
                    ua(a.overrides),
                    s(&a.asserted_by.id),
                    json(&a.asserted_by.kind).map(|k| k.trim_matches('"').to_string()),
                    uuid(&a.created_revision.0),
                    s(st.seq_of(&a.created_revision)),
                    tick(a.created_at),
                    tick(a.first_known_at),
                    l,
                    g,
                    o,
                    a.license.clone(),
                    a.note.clone(),
                ]
            }),
        )
        .map_err(io)?;
        c.table(
            "assertion_status_change",
            &["subject_id", "assertion_id", "revision", "from_status", "to_status", "changed_by", "recorded_at", "basis_acquired_at", "reason"],
            st.assertions.values().flat_map(|a| {
                a.status_history.iter().map(move |h| {
                    vec![
                        uuid(&a.subject.0),
                        uuid(&a.id.0),
                        uuid(&h.revision.0),
                        status_s(h.from),
                        status_s(h.to),
                        s(&h.by.id),
                        tick(h.recorded_at),
                        h.basis_acquired_at.and_then(tick),
                        h.reason.clone(),
                    ]
                })
            }),
        )
        .map_err(io)?;
        c.table(
            "evidence",
            &["subject_id", "assertion_id", "acquisition_id", "derivation_id"],
            st.assertions.values().flat_map(|a| {
                a.evidence.iter().map(move |e| vec![uuid(&a.subject.0), uuid(&a.id.0), uuid(&e.acquisition.0), e.derivation.and_then(|d| uuid(&d.0))])
            }),
        )
        .map_err(io)?;
        let mut rows = vec![];
        for d in proj.live.iter() {
            let Some(r) = proj.row_by_doc(d) else { continue };
            let t = r.temporal.as_ref();
            let (vl, vg, _) = vis(&r.visibility);
            let (mut es, mut le) = t.map(|t| t.range.possible_span()).unwrap_or((Tick::NEG_INF, Tick::POS_INF));
            for a in r.temporal_alternatives.iter().filter(|a| t.is_some_and(|t| t.axis == a.axis)) {
                es = es.min(a.range.earliest_start);
                le = le.max(a.range.latest_end);
            }
            let geo = r.placement.as_ref().and_then(|p| st.frames.to_root(p));
            rows.push(vec![
                uuid(&r.canonical_id.0),
                uuid(&branch.0),
                s(r.types_mask as i64),
                uuid_arr(r.type_ids.iter().map(|x| &x.0)),
                s(r.label(self.config.default_lang.as_deref())),
                text_arr(r.labels.iter().map(|l| &l.text)),
                s(r.description.clone().unwrap_or_default()),
                t.and_then(|t| tick(t.range.earliest_start)),
                t.and_then(|t| tick(t.range.latest_start)),
                t.and_then(|t| tick(t.range.earliest_end)),
                t.and_then(|t| tick(t.range.latest_end)),
                t.and(range(es, le)),
                t.and_then(|t| t.range.certain_span()).and_then(|(a, b)| range(a, b)),
                t.map(|t| t.axis.clone()),
                t.and_then(|t| json(&t.granularity)).map(|g| g.trim_matches('"').to_string()),
                t.and_then(|t| t.recurrence.as_ref()).and_then(json),
                s(r.temporal_contested),
                r.order_label.map(|o| o.start_ord.to_string()),
                r.order_label.map(|o| o.end_ord.to_string()),
                r.placement_inherited_from.and_then(|x| uuid(&x.0)),
                geo.as_ref().and_then(|g| uuid(&g.frame.0)),
                geo.map(|g| {
                    let (a, b) = g.geometry.bbox();
                    format!("(({},{}),({},{}))", a.x, a.y, b.x, b.y)
                }),
                uuid_arr(r.space_ids.iter().map(|x| &x.0)),
                uuid_arr(r.space_ancestor_ids.iter().map(|x| &x.0)),
                uuid_arr(r.entity_ids.iter().map(|x| &x.0)),
                uuid_arr(r.work_ids.iter().map(|x| &x.0)),
                uuid_arr(r.branch_ids.iter().map(|x| &x.0)),
                uuid_arr(r.canon_ids.iter().map(|x| &x.0)),
                uuid_arr(r.timeline_ids.iter().map(|x| &x.0)),
                s(r.rank),
                s(r.contested),
                s(r.known_from.0),
                s(r.redistributable),
                vl,
                vg,
                r.embedding_ref.clone(),
                s(r.projection_version),
                s(r.source_revision),
                s(r.materialized_at.0),
                s(r.stale),
            ]);
        }
        c.table(
            "search_projection",
            &[
                "canonical_id",
                "branch_id",
                "types_mask",
                "type_ids",
                "label",
                "labels",
                "summary",
                "earliest_start",
                "latest_start",
                "earliest_end",
                "latest_end",
                "temporal_possible",
                "temporal_certain",
                "time_axis",
                "granularity",
                "recurrence",
                "temporal_contested",
                "order_start",
                "order_end",
                "geo_inherited_from",
                "geo_frame",
                "geo_bbox",
                "space_ids",
                "space_ancestor_ids",
                "entity_ids",
                "work_ids",
                "branch_ids",
                "canon_ids",
                "timeline_ids",
                "rank",
                "contested",
                "known_from",
                "redistributable",
                "visibility_level",
                "visibility_groups",
                "embedding_ref",
                "projection_version",
                "source_revision",
                "materialized_at",
                "stale",
            ],
            rows,
        )
        .map_err(io)?;
        writeln!(c.w, "COMMIT;").map_err(io)?;
        Ok(())
    }
}
