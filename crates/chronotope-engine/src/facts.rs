//! Resolver: Canonical から、あるビューにおける Resource の事実（時間・場所・関係・ランク・異説）を導く。
//! Materializer は既定ビューでこれを計算して Projection 行にし、検索時の検証（canon / timeline /
//! 過去時点指定など）は同じ関数を別ビューで呼ぶ。

use crate::canonical::CanonicalStore;
use crate::view::View;
use chronotope_core::model::*;
use chronotope_core::rank::RankInput;
use chronotope_core::space::Placement;
use chronotope_core::time::Tick;
use chronotope_core::time::expr::{Anchor, TemporalExpression, TimeAst};
use chronotope_core::time::range::{FuzzyRange, ResolvedTemporal};
use chronotope_core::time::resolve::{AnchorResult, ResolveContext, Unresolved, UnresolvedKind, resolve};
use chronotope_core::vocab::keys;
use chronotope_core::*;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};

/// 相対参照（A事件の3日前 …）をたどる深さの上限。
pub const MAX_RELATIVE_DEPTH: usize = 8;
const MAX_HIERARCHY_DEPTH: usize = 32;
const MAX_RELATED: usize = 256;

#[derive(Debug, Clone)]
pub struct TimeSlot {
    pub result: std::result::Result<ResolvedTemporal, Unresolved>,
    pub assertion: Option<AssertionId>,
    pub raw: Option<String>,
    pub contested: bool,
    /// 異説（優先されなかった値）の解決結果。検索で取りこぼさないよう索引では包絡を使う。
    pub alternatives: Vec<ResolvedTemporal>,
    pub depends_on: Vec<ResourceId>,
    pub depends_on_names: Vec<String>,
}

impl TimeSlot {
    fn unresolved(kind: UnresolvedKind, msg: &str) -> Self {
        TimeSlot {
            result: Err(Unresolved::new(kind, msg)),
            assertion: None,
            raw: None,
            contested: false,
            alternatives: vec![],
            depends_on: vec![],
            depends_on_names: vec![],
        }
    }
    pub fn resolved(&self) -> Option<&ResolvedTemporal> {
        self.result.as_ref().ok()
    }
}

#[derive(Debug, Clone)]
pub struct ClaimEval {
    pub rank: ComputedRank,
    pub confidence: ConfidenceComponents,
}

#[derive(Debug, Clone)]
pub struct PredicateSummary {
    pub predicate: ResourceId,
    pub preferred: Option<AssertionId>,
    /// 肯定の主張（ランク順）。
    pub values: Vec<AssertionId>,
    pub negated: Vec<AssertionId>,
    pub contested: bool,
}

#[derive(Debug, Clone)]
pub struct ResourceFacts {
    pub id: ResourceId,
    pub types: BTreeSet<ResourceId>,
    pub time: Option<TimeSlot>,
    pub placement: Option<Placement>,
    pub places: Vec<ResourceId>,
    pub place_ancestors: Vec<ResourceId>,
    pub entities: Vec<ResourceId>,
    pub works: Vec<ResourceId>,
    pub canons: Vec<ResourceId>,
    pub timelines: Vec<ResourceId>,
    pub branches: Vec<BranchId>,
    pub rank: f64,
    pub contested: bool,
    pub contested_predicates: Vec<ResourceId>,
    pub known_from: Tick,
    pub redistributable: bool,
    pub evals: HashMap<AssertionId, ClaimEval>,
    pub summaries: Vec<PredicateSummary>,
}

/// (依存元, 参照先 ID 群, 参照名群)
pub type TimeDeps = (ResourceId, Vec<ResourceId>, Vec<String>);

pub struct Facts<'a> {
    pub store: &'a CanonicalStore,
    pub view: &'a View,
    pub now: Tick,
    cache: RefCell<HashMap<ResourceId, TimeSlot>>,
    stack: RefCell<Vec<ResourceId>>,
    /// この計算中に新しく解決した時間とその依存（無効化の登録用）。
    new_deps: RefCell<Vec<TimeDeps>>,
}

/// 時間表現の具体性（信頼度の specificity 成分）。
pub fn ast_specificity(ast: &TimeAst) -> f32 {
    match ast {
        TimeAst::Date { date } => match (date.month, date.day, &date.clock) {
            (_, Some(_), Some(_)) => 1.0,
            (_, Some(_), None) => 0.85,
            (Some(_), None, _) => {
                if date.month_part.is_some() {
                    0.7
                } else {
                    0.6
                }
            }
            _ => 0.4,
        },
        TimeAst::Approx { inner } => ast_specificity(inner) * 0.8,
        TimeAst::Interval { .. } => 0.5,
        TimeAst::Relative { .. } => 0.6,
        TimeAst::Weekday { .. } => 0.8,
        TimeAst::Recurring { .. } => 0.7,
        TimeAst::Between { .. } => 0.3,
        TimeAst::Unknown | TimeAst::Unparsed => 0.0,
    }
}

impl<'a> Facts<'a> {
    pub fn new(store: &'a CanonicalStore, view: &'a View, now: Tick, cache: HashMap<ResourceId, TimeSlot>) -> Self {
        Facts { store, view, now, cache: RefCell::new(cache), stack: RefCell::new(vec![]), new_deps: RefCell::new(vec![]) }
    }

    /// (時間キャッシュ, 新たに記録した依存)
    pub fn into_parts(self) -> (HashMap<ResourceId, TimeSlot>, Vec<TimeDeps>) {
        (self.cache.into_inner(), self.new_deps.into_inner())
    }

    // ------------------------------------------------------------ claims & ranking

    fn source_of(&self, e: &Evidence) -> Option<&'a Source> {
        let acq = self.store.acquisitions.get(&e.acquisition)?;
        self.store.sources.get(&acq.source)
    }

    /// 主張の独立出典 root 集合（転載は同じ root）。根拠なしの主張は主張者を擬似 root とする。
    fn roots(&self, a: &Assertion) -> Vec<String> {
        if a.evidence.is_empty() {
            return vec![format!("actor:{}", a.asserted_by.id)];
        }
        a.evidence.iter().filter_map(|e| self.source_of(e)).map(|s| s.root().to_string()).collect()
    }

    pub fn evaluate(&self, claims: &[(&Assertion, AssertionStatus)]) -> HashMap<AssertionId, ClaimEval> {
        let policy = &self.store.rank_policy;
        let mut groups: HashMap<(ResourceId, String, Polarity), HashSet<String>> = HashMap::new();
        for (a, st) in claims {
            if st.is_live() {
                groups.entry((a.predicate, a.object.identity_key(), a.polarity)).or_default().extend(self.roots(a));
            }
        }
        let mut out = HashMap::new();
        for (a, st) in claims {
            let mut c = a.confidence.clone();
            let n = groups.get(&(a.predicate, a.object.identity_key(), a.polarity)).map(|s| s.len() as u32).unwrap_or(0);
            c.independent_sources = n;
            c.corroboration = policy.corroboration(n);
            let sources: Vec<&Source> = a.evidence.iter().filter_map(|e| self.source_of(e)).collect();
            if c.source_reliability.is_none() {
                c.source_reliability = sources.iter().filter_map(|s| s.reliability).fold(None, |m: Option<f32>, r| Some(m.map_or(r, |x| x.max(r))));
            }
            let derivations: Vec<&Derivation> = a.evidence.iter().filter_map(|e| e.derivation.and_then(|d| self.store.derivations.get(&d))).collect();
            if c.extraction_conf.is_none() {
                c.extraction_conf = derivations.iter().filter_map(|d| d.extraction_conf).fold(None, |m: Option<f32>, r| Some(m.map_or(r, |x| x.max(r))));
            }
            if c.specificity.is_none() {
                if let Value::Time(t) = &a.object {
                    c.specificity = Some(ast_specificity(&t.ast));
                }
            }
            let best_origin = sources.iter().map(|s| s.origin).min_by_key(|o| match o {
                SourceOrigin::Primary => 0,
                SourceOrigin::Secondary => 1,
                SourceOrigin::Tertiary => 2,
                SourceOrigin::Unknown => 3,
            });
            let ai_only = if a.evidence.is_empty() {
                a.asserted_by.is_ai()
            } else {
                a.evidence.iter().all(|e| e.derivation.and_then(|d| self.store.derivations.get(&d)).is_some_and(|d| d.model.is_some()))
            };
            let rank = policy.compute(&RankInput { confidence: &c, best_origin: best_origin.unwrap_or_default(), ai_only, status: *st }, self.now);
            out.insert(a.id, ClaimEval { rank, confidence: c });
        }
        out
    }

    pub fn summarize(&self, claims: &[(&Assertion, AssertionStatus)], evals: &HashMap<AssertionId, ClaimEval>) -> Vec<PredicateSummary> {
        let mut by_pred: HashMap<ResourceId, Vec<(&Assertion, AssertionStatus)>> = HashMap::new();
        for (a, st) in claims {
            if st.is_live() {
                by_pred.entry(a.predicate).or_default().push((a, *st));
            }
        }
        let mut out = vec![];
        for (pred, mut v) in by_pred {
            v.sort_by(|(a, sa), (b, sb)| {
                let ra = evals.get(&a.id).map(|e| e.rank.value).unwrap_or(0.0);
                let rb = evals.get(&b.id).map(|e| e.rank.value).unwrap_or(0.0);
                rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal).then(sa.cmp(sb)).then(a.created_at.cmp(&b.created_at)).then(a.id.cmp(&b.id))
            });
            let values: Vec<&Assertion> = v.iter().filter(|(a, _)| a.polarity == Polarity::Affirmed).map(|(a, _)| *a).collect();
            let negated: Vec<&Assertion> = v.iter().filter(|(a, _)| a.polarity == Polarity::Negated).map(|(a, _)| *a).collect();
            let functional = self.store.predicates.get(&pred).is_some_and(|p| p.functional);
            let distinct: HashSet<String> = values.iter().map(|a| a.object.identity_key()).collect();
            let negated_keys: HashSet<String> = negated.iter().map(|a| a.object.identity_key()).collect();
            let contested = (functional && distinct.len() > 1)
                || distinct.iter().any(|k| negated_keys.contains(k))
                || v.iter().any(|(_, st)| *st == AssertionStatus::Disputed);
            out.push(PredicateSummary {
                predicate: pred,
                preferred: values.first().map(|a| a.id),
                values: values.iter().map(|a| a.id).collect(),
                negated: negated.iter().map(|a| a.id).collect(),
                contested,
            });
        }
        out.sort_by_key(|s| s.predicate);
        out
    }

    // ------------------------------------------------------------ time

    /// 相対表現の基準時刻: 根拠の情報源の source_time、無ければ acquired_at。
    pub fn reference_time(&self, a: &Assertion) -> Option<Tick> {
        for e in &a.evidence {
            let Some(acq) = self.store.acquisitions.get(&e.acquisition) else { continue };
            if let Some(src) = self.store.sources.get(&acq.source) {
                if let Some(expr) = &src.source_time {
                    if let Some(t) = self.absolute_start(expr) {
                        return Some(t);
                    }
                }
            }
            return Some(acq.acquired_at);
        }
        None
    }

    fn absolute_start(&self, expr: &TemporalExpression) -> Option<Tick> {
        let cal = self.store.calendar(&expr.calendar_frame)?;
        let r = resolve(expr, &ResolveContext { reference: None, calendar: &cal, lookup: &|_| AnchorResult::NotFound }).ok()?;
        Some(r.range.earliest_start).filter(|t| t.is_finite())
    }

    fn resolve_expr(
        &self,
        expr: &TemporalExpression,
        reference: Option<Tick>,
        depth: usize,
        deps: &RefCell<Vec<ResourceId>>,
        names: &RefCell<Vec<String>>,
    ) -> std::result::Result<ResolvedTemporal, Unresolved> {
        let Some(cal) = self.store.calendar(&expr.calendar_frame) else {
            return Err(Unresolved::new(UnresolvedKind::Unsupported, format!("unknown calendar `{}`", expr.calendar_frame)));
        };
        let lookup = |anchor: &Anchor| -> AnchorResult {
            let id = match anchor {
                Anchor::Resource(r) => self.store.resolve_id(*r),
                Anchor::Named(n) => {
                    names.borrow_mut().push(crate::text::normalize_label(n));
                    let ids = self.store.find_by_label(n);
                    match ids.len() {
                        0 => return AnchorResult::NotFound,
                        1 => ids[0],
                        _ => return AnchorResult::Ambiguous(ids),
                    }
                }
                Anchor::Reference => return AnchorResult::NotFound,
            };
            deps.borrow_mut().push(id);
            let slot = self.time_at_depth(id, depth + 1);
            match slot.result {
                Ok(t) => AnchorResult::Found(t),
                Err(u) => AnchorResult::Unresolved(Unresolved {
                    kind: match u.kind {
                        UnresolvedKind::Cycle | UnresolvedKind::DepthExceeded => u.kind,
                        _ => UnresolvedKind::AnchorUnresolved,
                    },
                    ..u
                }),
            }
        };
        resolve(expr, &ResolveContext { reference, calendar: &cal, lookup: &lookup })
    }

    /// 任意の時間表現を（他の出来事への参照も含めて）解決する。
    pub fn resolve_expression(&self, expr: &TemporalExpression, reference: Option<Tick>) -> std::result::Result<ResolvedTemporal, Unresolved> {
        self.resolve_expr(expr, reference, 0, &RefCell::new(vec![]), &RefCell::new(vec![]))
    }

    pub fn time_of(&self, id: ResourceId) -> TimeSlot {
        self.time_at_depth(self.store.resolve_id(id), 0)
    }

    fn time_at_depth(&self, id: ResourceId, depth: usize) -> TimeSlot {
        if let Some(s) = self.cache.borrow().get(&id) {
            return s.clone();
        }
        if self.stack.borrow().contains(&id) {
            return TimeSlot::unresolved(UnresolvedKind::Cycle, "circular relative time reference");
        }
        if depth > MAX_RELATIVE_DEPTH {
            return TimeSlot::unresolved(UnresolvedKind::DepthExceeded, "relative time reference chain too deep");
        }
        self.stack.borrow_mut().push(id);
        let slot = self.compute_time(id, depth);
        self.stack.borrow_mut().pop();
        // 循環・深さ超過は探索経路に依存するためキャッシュしない。
        let path_dependent = matches!(&slot.result, Err(u) if matches!(u.kind, UnresolvedKind::Cycle | UnresolvedKind::DepthExceeded));
        self.new_deps.borrow_mut().push((id, slot.depends_on.clone(), slot.depends_on_names.clone()));
        if !path_dependent {
            self.cache.borrow_mut().insert(id, slot.clone());
        }
        slot
    }

    fn compute_time(&self, id: ResourceId, depth: usize) -> TimeSlot {
        let store = self.store;
        let claims: Vec<(&Assertion, AssertionStatus)> = store
            .claims_about(id, self.view)
            .into_iter()
            .filter(|(a, _)| store.role_of(&a.predicate) == PredicateRole::EventTime && matches!(a.object, Value::Time(_)))
            .collect();
        let deps = RefCell::new(vec![]);
        let names = RefCell::new(vec![]);
        if claims.is_empty() {
            // 文書・投稿は source_time を時間として使う。
            for sid in store.sources_by_resource.get(&id).into_iter().flatten() {
                if let Some(expr) = store.sources.get(sid).and_then(|s| s.source_time.as_ref()) {
                    let result = self.resolve_expr(expr, None, depth, &deps, &names);
                    return TimeSlot {
                        result,
                        assertion: None,
                        raw: Some(expr.raw_text.clone()),
                        contested: false,
                        alternatives: vec![],
                        depends_on: deps.into_inner(),
                        depends_on_names: names.into_inner(),
                    };
                }
            }
            return TimeSlot::unresolved(UnresolvedKind::Unknown, "no event time");
        }
        let evals = self.evaluate(&claims);
        let sums = self.summarize(&claims, &evals);
        let key_of = |pid: &ResourceId| store.predicates.get(pid).map(|p| p.key.as_str()).unwrap_or("");
        let pick = |key: &str| sums.iter().find(|s| key_of(&s.predicate) == key);
        let contested = sums.iter().any(|s| s.contested);
        let get = |aid: AssertionId| -> (&Assertion, &TemporalExpression) {
            let a = &store.assertions[&aid];
            (a, a.object.as_time().expect("filtered to time values"))
        };
        if let Some(sum) = pick(keys::OCCURRED_AT).filter(|s| s.preferred.is_some()) {
            let (a, expr) = get(sum.preferred.expect("filtered"));
            let result = self.resolve_expr(expr, self.reference_time(a), depth, &deps, &names);
            let alternatives = sum
                .values
                .iter()
                .skip(1)
                .filter_map(|aid| {
                    let (alt, e) = get(*aid);
                    self.resolve_expr(e, self.reference_time(alt), depth, &deps, &names).ok()
                })
                .collect();
            return TimeSlot {
                result,
                assertion: Some(a.id),
                raw: Some(expr.raw_text.clone()),
                contested,
                alternatives,
                depends_on: deps.into_inner(),
                depends_on_names: names.into_inner(),
            };
        }
        let start = pick(keys::START_TIME).and_then(|s| s.preferred).map(get);
        let end = pick(keys::END_TIME).and_then(|s| s.preferred).map(get);
        let resolve_part = |p: Option<(&Assertion, &TemporalExpression)>| match p {
            Some((a, e)) => self.resolve_expr(e, self.reference_time(a), depth, &deps, &names).map(Some),
            None => Ok(None),
        };
        let (s, e) = match (resolve_part(start), resolve_part(end)) {
            (Ok(s), Ok(e)) => (s, e),
            (Err(u), _) | (_, Err(u)) => {
                return TimeSlot {
                    result: Err(u),
                    assertion: start.or(end).map(|x| x.0.id),
                    raw: None,
                    contested,
                    alternatives: vec![],
                    depends_on: deps.into_inner(),
                    depends_on_names: names.into_inner(),
                };
            }
        };
        let raw = format!("{} 〜 {}", start.map(|x| x.1.raw_text.as_str()).unwrap_or("?"), end.map(|x| x.1.raw_text.as_str()).unwrap_or("?"));
        let base = s.clone().or(e.clone()).expect("start or end exists");
        if let (Some(s), Some(e)) = (&s, &e) {
            if s.axis != e.axis {
                return TimeSlot {
                    result: Err(Unresolved::new(UnresolvedKind::Incomparable, "start and end use different time axes")),
                    assertion: None,
                    raw: Some(raw),
                    contested,
                    alternatives: vec![],
                    depends_on: deps.into_inner(),
                    depends_on_names: names.into_inner(),
                };
            }
        }
        let range = FuzzyRange {
            earliest_start: s.as_ref().map(|x| x.range.earliest_start).unwrap_or(Tick::NEG_INF),
            latest_start: s.as_ref().map(|x| x.range.latest_end).unwrap_or(Tick::POS_INF),
            earliest_end: e.as_ref().map(|x| x.range.earliest_start).unwrap_or(Tick::NEG_INF),
            latest_end: e.as_ref().map(|x| x.range.latest_end).unwrap_or(Tick::POS_INF),
        }
        .normalized();
        let granularity = chronotope_core::time::range::Granularity::from_width(range.width());
        TimeSlot {
            result: Ok(ResolvedTemporal { range, granularity, recurrence: None, ..base }),
            assertion: start.or(end).map(|x| x.0.id),
            raw: Some(raw),
            contested,
            alternatives: vec![],
            depends_on: deps.into_inner(),
            depends_on_names: names.into_inner(),
        }
    }

    // ------------------------------------------------------------ hierarchy

    fn live_affirmed<'b>(&self, v: Vec<(&'b Assertion, AssertionStatus)>) -> impl Iterator<Item = &'b Assertion> {
        v.into_iter().filter(|(a, st)| st.is_live() && a.polarity == Polarity::Affirmed).map(|(a, _)| a)
    }

    /// 場所の直接の親（located_in / inside の目的語、contains の主語）。
    pub fn place_parents(&self, place: ResourceId) -> Vec<ResourceId> {
        let s = self.store;
        let key = |a: &Assertion| s.predicates.get(&a.predicate).map(|p| p.key.clone()).unwrap_or_default();
        let mut out = vec![];
        for a in self.live_affirmed(s.claims_about(place, self.view)) {
            let k = key(a);
            if k == keys::LOCATED_IN || k == keys::INSIDE {
                if let Value::Resource(o) = a.object {
                    out.push(s.resolve_id(o));
                }
            }
        }
        if let Some(contains) = s.predicate_by_key.get(keys::CONTAINS) {
            for a in self.live_affirmed(s.claims_referencing_with(place, *contains, self.view)) {
                out.push(s.resolve_id(a.subject));
            }
        }
        out.sort();
        out.dedup();
        out
    }

    fn closure(&self, starts: &[ResourceId], parents: impl Fn(ResourceId) -> Vec<ResourceId>) -> Vec<ResourceId> {
        let mut seen: Vec<ResourceId> = vec![];
        let mut frontier: Vec<ResourceId> = starts.to_vec();
        for _ in 0..MAX_HIERARCHY_DEPTH {
            let mut next = vec![];
            for x in frontier {
                if seen.contains(&x) {
                    continue;
                }
                seen.push(x);
                next.extend(parents(x));
            }
            if next.is_empty() || seen.len() > MAX_RELATED {
                break;
            }
            frontier = next;
        }
        seen
    }

    pub fn work_parents(&self, work: ResourceId) -> Vec<ResourceId> {
        let s = self.store;
        self.live_affirmed(s.claims_about(work, self.view))
            .filter(|a| s.predicates.get(&a.predicate).is_some_and(|p| p.key == keys::PART_OF_WORK))
            .filter_map(|a| a.object.as_resource().map(|o| s.resolve_id(o)))
            .collect()
    }

    // ------------------------------------------------------------ facts

    pub fn compute(&self, id: ResourceId) -> Option<ResourceFacts> {
        let s = self.store;
        let id = s.resolve_id(id);
        let res = s.resources.get(&id)?;
        let claims = s.claims_about(id, self.view);
        let incoming = s.claims_referencing(id, self.view);
        let evals = self.evaluate(&claims);
        let summaries = self.summarize(&claims, &evals);
        let preferred_of_role = |role: PredicateRole| -> Vec<&Assertion> {
            summaries.iter().filter(|x| s.role_of(&x.predicate) == role).flat_map(|x| x.values.iter().map(|a| &s.assertions[a])).collect()
        };

        let mut types: Vec<ResourceId> = res.types.iter().copied().collect();
        types.extend(preferred_of_role(PredicateRole::Typing).iter().filter_map(|a| a.object.as_resource()));
        let types = s.type_closure(types);
        let is_place = types.contains(&chronotope_core::vocab::type_id("Place"));
        let is_work = types.contains(&chronotope_core::vocab::type_id("Work"));

        let mut places: Vec<ResourceId> =
            preferred_of_role(PredicateRole::Location).iter().filter_map(|a| a.object.as_resource()).map(|o| s.resolve_id(o)).collect();
        if is_place {
            places.insert(0, id);
        } else {
            // 物・人物の所在（located_in）も場所として扱う。
            places.extend(preferred_of_role(PredicateRole::SpatialContainment).iter().filter_map(|a| a.object.as_resource()).map(|o| s.resolve_id(o)));
        }
        places.sort();
        places.dedup();
        let place_ancestors = self.closure(&places, |p| self.place_parents(p));

        let placement = preferred_of_role(PredicateRole::Coordinates)
            .first()
            .and_then(|a| match &a.object {
                Value::Geo(p) => Some(p.clone()),
                _ => None,
            })
            .or_else(|| {
                places.iter().filter(|p| **p != id).find_map(|p| {
                    s.claims_about(*p, self.view).into_iter().find_map(|(a, st)| match (&a.object, st.is_live(), s.role_of(&a.predicate)) {
                        (Value::Geo(g), true, PredicateRole::Coordinates) => Some(g.clone()),
                        _ => None,
                    })
                })
            });

        let mut works: Vec<ResourceId> =
            preferred_of_role(PredicateRole::WorkMembership).iter().filter_map(|a| a.object.as_resource()).map(|o| s.resolve_id(o)).collect();
        if is_work {
            works.insert(0, id);
        }
        let works = self.closure(&works, |w| self.work_parents(w));

        let related_role = |r: PredicateRole| matches!(r, PredicateRole::General | PredicateRole::TemporalRelation | PredicateRole::Adaptation);
        let mut entities: Vec<ResourceId> = vec![];
        for (a, st) in claims.iter().chain(incoming.iter()) {
            if !st.is_live() || a.polarity != Polarity::Affirmed || !related_role(s.role_of(&a.predicate)) {
                continue;
            }
            let other = if a.subject == id || s.resolve_id(a.subject) == id { a.object.as_resource() } else { Some(a.subject) };
            if let Some(o) = other {
                let o = s.resolve_id(o);
                if o != id && !entities.contains(&o) && entities.len() < MAX_RELATED {
                    entities.push(o);
                }
            }
        }

        let mut canons = vec![];
        let mut timelines = vec![];
        let mut branches = vec![];
        let mut known_from = Tick::POS_INF;
        let mut redistributable = res.license.as_ref().is_none_or(|l| s.licenses.get(l).is_none_or(|x| x.redistributable));
        for (a, st) in &claims {
            if !st.is_live() {
                continue;
            }
            if let Some(c) = a.canon {
                if !canons.contains(&c) {
                    canons.push(c);
                }
            }
            if let Some(t) = a.timeline {
                if !timelines.contains(&t) {
                    timelines.push(t);
                }
            }
            if !branches.contains(&a.branch) {
                branches.push(a.branch);
            }
            known_from = known_from.min(a.first_known_at);
            if let Some(l) = &a.license {
                if s.licenses.get(l).is_some_and(|x| !x.redistributable) {
                    redistributable = false;
                }
            }
        }
        if known_from == Tick::POS_INF {
            known_from = res.created_at;
        }
        let rank = claims
            .iter()
            .filter(|(a, st)| st.is_live() && a.polarity == Polarity::Affirmed)
            .filter_map(|(a, _)| evals.get(&a.id))
            .map(|e| e.rank.value)
            .fold(0.0, f64::max);
        let contested_predicates: Vec<ResourceId> = summaries.iter().filter(|x| x.contested).map(|x| x.predicate).collect();
        let time = self.time_of(id);
        let time = match &time.result {
            Err(u) if u.kind == UnresolvedKind::Unknown && time.assertion.is_none() && time.raw.is_none() => None,
            _ => Some(time),
        };
        Some(ResourceFacts {
            id,
            types,
            time,
            placement,
            places,
            place_ancestors,
            entities,
            works,
            canons,
            timelines,
            branches,
            rank,
            contested: !contested_predicates.is_empty(),
            contested_predicates,
            known_from,
            redistributable,
            evals,
            summaries,
        })
    }
}
