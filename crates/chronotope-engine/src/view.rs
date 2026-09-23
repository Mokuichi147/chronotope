//! 読み取りビュー。どのブランチ・正史系統・世界線・過去時点・状態・主体から見るかを表し、
//! Assertion ごとの可視性と実効状態を決める（RLS 相当の判定もここで行う）。

use crate::canonical::CanonicalStore;
use chronotope_core::model::*;
use chronotope_core::time::Tick;
use chronotope_core::vocab::keys;
use chronotope_core::*;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSet(u8);

impl StatusSet {
    pub fn live() -> Self {
        StatusSet::of(&[AssertionStatus::Accepted, AssertionStatus::Proposed, AssertionStatus::Disputed])
    }
    pub fn all() -> Self {
        StatusSet(0x1F)
    }
    pub fn of(s: &[AssertionStatus]) -> Self {
        StatusSet(s.iter().fold(0, |m, s| m | (1 << *s as u8)))
    }
    pub fn contains(self, s: AssertionStatus) -> bool {
        self.0 & (1 << s as u8) != 0
    }
    pub fn is_live_default(self) -> bool {
        self == StatusSet::live()
    }
}

#[derive(Debug, Clone)]
pub struct View {
    pub branch: BranchId,
    pub chain: Vec<(BranchId, u64)>,
    pub canon: Option<ResourceId>,
    /// 指定された世界線と、その分岐元の世界線。
    pub timelines: Option<HashSet<ResourceId>>,
    pub as_known_at: Option<Tick>,
    pub statuses: StatusSet,
    pub principal: Principal,
}

impl View {
    /// Projection の既定ビュー（公開情報のみ・生きている主張・全 canon/timeline）。
    pub fn projection(store: &CanonicalStore, branch: BranchId) -> Result<View> {
        Ok(View {
            branch,
            chain: store.branch_chain(branch)?,
            canon: None,
            timelines: None,
            as_known_at: None,
            statuses: StatusSet::live(),
            principal: Principal::anonymous(),
        })
    }

    /// Projection の既定ビューと同じ結果になるか（検証の要否判定）。
    pub fn is_projection_equivalent(&self) -> bool {
        self.canon.is_none() && self.timelines.is_none() && self.as_known_at.is_none() && self.statuses.is_live_default()
    }
}

impl CanonicalStore {
    /// 世界線とその分岐元（diverges_from の祖先）。
    pub fn timeline_lineage(&self, t: ResourceId) -> HashSet<ResourceId> {
        let pred = self.predicate_by_key.get(keys::DIVERGES_FROM).copied();
        let mut out = HashSet::new();
        let mut stack = vec![t];
        while let Some(x) = stack.pop() {
            if out.len() > 64 || !out.insert(x) {
                continue;
            }
            for aid in self.by_subject.get(&x).into_iter().flatten() {
                let a = &self.assertions[aid];
                if Some(a.predicate) == pred && a.status.is_live() && a.polarity == Polarity::Affirmed {
                    if let Value::Resource(p) = a.object {
                        stack.push(p);
                    }
                }
            }
        }
        out
    }

    /// ビューから見た実効状態。見えなければ None。
    pub fn effective_status(&self, a: &Assertion, view: &View) -> Option<AssertionStatus> {
        let pos = view.chain.iter().position(|(b, _)| *b == a.branch)?;
        let limit = view.chain[pos].1;
        if self.seq_of(&a.created_revision) > limit {
            return None;
        }
        // より近いブランチで上書き（copy-on-write）されていれば、元は見えない。
        if let Some(ovs) = self.overrides.get(&a.id) {
            for o in ovs {
                if let Some(oa) = self.assertions.get(o) {
                    if let Some(opos) = view.chain.iter().position(|(b, _)| *b == oa.branch) {
                        if opos < pos && self.seq_of(&oa.created_revision) <= view.chain[opos].1 {
                            return None;
                        }
                    }
                }
            }
        }
        if let Some(t) = view.as_known_at {
            if a.first_known_at > t {
                return None;
            }
        }
        let mut st = a.status_history.first().map(|c| c.from).unwrap_or(a.status);
        for c in &a.status_history {
            if self.seq_of(&c.revision) > limit {
                continue;
            }
            if let Some(t) = view.as_known_at {
                if c.basis_acquired_at.unwrap_or(c.recorded_at) > t {
                    continue;
                }
            }
            st = c.to;
        }
        if !view.statuses.contains(st) || !view.principal.can_see(&a.visibility) {
            return None;
        }
        if let (Some(c), Some(ac)) = (view.canon, a.canon) {
            if c != ac {
                return None;
            }
        }
        if let (Some(ts), Some(at)) = (&view.timelines, a.timeline) {
            if !ts.contains(&at) {
                return None;
            }
        }
        Some(st)
    }

    /// Resource（と統合済みメンバー）を主語とする可視な Assertion。
    pub fn claims_about<'a>(&'a self, id: ResourceId, view: &View) -> Vec<(&'a Assertion, AssertionStatus)> {
        let mut out = vec![];
        for member in self.identity_group(id) {
            for aid in self.by_subject.get(&member).into_iter().flatten() {
                let a = &self.assertions[aid];
                if let Some(st) = self.effective_status(a, view) {
                    out.push((a, st));
                }
            }
        }
        out
    }

    /// Resource（と統合済みメンバー）を目的語とする可視な Assertion。
    pub fn claims_referencing<'a>(&'a self, id: ResourceId, view: &View) -> Vec<(&'a Assertion, AssertionStatus)> {
        let mut out = vec![];
        for member in self.identity_group(id) {
            for aid in self.by_object.get(&member).into_iter().flatten() {
                let a = &self.assertions[aid];
                if let Some(st) = self.effective_status(a, view) {
                    out.push((a, st));
                }
            }
        }
        out
    }

    /// 特定の述語で Resource（と統合済みメンバー）を目的語とする可視な Assertion。
    pub fn claims_referencing_with<'a>(&'a self, id: ResourceId, predicate: ResourceId, view: &View) -> Vec<(&'a Assertion, AssertionStatus)> {
        let mut out = vec![];
        for member in self.identity_group(id) {
            for aid in self.by_object_predicate.get(&(member, predicate)).into_iter().flatten() {
                let a = &self.assertions[aid];
                if let Some(st) = self.effective_status(a, view) {
                    out.push((a, st));
                }
            }
        }
        out
    }

    pub fn role_of(&self, predicate: &ResourceId) -> PredicateRole {
        self.predicates.get(predicate).map(|p| p.role).unwrap_or(PredicateRole::General)
    }
}
