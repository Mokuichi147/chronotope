//! Canonical Knowledge Layer（正しさ・表現力優先）。
//!
//! すべての変更は [`Command`] 経由で `apply` され、どの Resource / Branch に影響したかを
//! [`Touch`] として返す。検索にはこの層を直接使わず、Materializer が Projection へ展開する。

use crate::command::{Command, Revision, RevisionHeader};
use crate::text::normalize_label;
use chronotope_core::model::*;
use chronotope_core::rank::RankPolicy;
use chronotope_core::space::FrameRegistry;
use chronotope_core::time::Tick;
use chronotope_core::time::calendar::CalendarFrame;
use chronotope_core::*;
use std::collections::{BTreeSet, HashMap, HashSet};

/// 変更の影響範囲。
#[derive(Debug, Default, Clone)]
pub struct Touch {
    /// 語彙・ランク方針など、全 Projection の再計算が必要な変更。
    pub global: bool,
    /// Resource 自体（ブランチに依存しない）の変更か。
    pub all_branches: bool,
    pub branch: Option<BranchId>,
    pub resources: Vec<ResourceId>,
    pub role: Option<PredicateRole>,
    /// 新しく索引されたラベル（名前参照の再解決に使う）。
    pub labels: Vec<String>,
}

#[derive(Default)]
pub struct CanonicalStore {
    pub types: HashMap<ResourceId, TypeDef>,
    pub type_by_key: HashMap<String, ResourceId>,
    /// types_mask 用のビット位置（定義順に最大 64）。
    pub type_bits: HashMap<ResourceId, u8>,
    pub predicates: HashMap<ResourceId, PredicateDef>,
    pub predicate_by_key: HashMap<String, ResourceId>,
    pub calendars: HashMap<String, CalendarFrame>,
    pub frames: FrameRegistry,
    pub licenses: HashMap<String, License>,
    pub rank_policy: RankPolicy,

    pub resources: HashMap<ResourceId, Resource>,
    pub label_index: HashMap<String, BTreeSet<ResourceId>>,
    pub external_index: HashMap<ExternalId, ResourceId>,

    pub assertions: HashMap<AssertionId, Assertion>,
    pub by_subject: HashMap<ResourceId, Vec<AssertionId>>,
    pub by_object: HashMap<ResourceId, Vec<AssertionId>>,
    pub by_predicate: HashMap<ResourceId, Vec<AssertionId>>,
    /// (目的語, 述語) → Assertion。場所の包含など、特定述語の被参照だけを引くため。
    pub by_object_predicate: HashMap<(ResourceId, ResourceId), Vec<AssertionId>>,
    pub by_derivation: HashMap<DerivationId, Vec<AssertionId>>,
    /// copy-on-write: 元 Assertion → それを上書きした Assertion 群。
    pub overrides: HashMap<AssertionId, Vec<AssertionId>>,

    pub sources: HashMap<SourceId, Source>,
    pub source_by_locator: HashMap<String, SourceId>,
    pub sources_by_resource: HashMap<ResourceId, Vec<SourceId>>,
    pub acquisitions: HashMap<AcquisitionId, Acquisition>,
    pub acquisitions_by_source: HashMap<SourceId, Vec<AcquisitionId>>,
    pub derivations: HashMap<DerivationId, Derivation>,

    pub merge_proposals: HashMap<MergeProposalId, MergeProposal>,
    pub redirects: HashMap<ResourceId, IdentityRedirect>,
    pub merged_members: HashMap<ResourceId, Vec<ResourceId>>,

    pub branches: HashMap<BranchId, Branch>,
    pub branch_by_name: HashMap<String, BranchId>,
    pub revisions: Vec<RevisionHeader>,
    pub revision_seq: HashMap<RevisionId, u64>,
    pub head_seq: u64,

    pub sequences: HashMap<SequenceId, Sequence>,
    pub tables: HashMap<TableId, TableDef>,
    pub row_links: HashMap<(TableId, String), Vec<ResourceId>>,
    pub rows_by_resource: HashMap<ResourceId, Vec<RowLink>>,
    pub keys: HashSet<KeyId>,
    pub shredded_keys: HashSet<KeyId>,
}

impl CanonicalStore {
    pub fn new() -> Self {
        let mut s = CanonicalStore { frames: FrameRegistry::new(), ..Default::default() };
        let main = Branch::main();
        s.branch_by_name.insert(main.name.clone(), main.id);
        s.branches.insert(main.id, main);
        s.calendars.insert("gregorian".into(), CalendarFrame::gregorian());
        s
    }

    // ------------------------------------------------------------ lookup helpers

    /// identity_redirect をたどった最終 ID。
    pub fn resolve_id(&self, id: ResourceId) -> ResourceId {
        let mut cur = id;
        for _ in 0..16 {
            match self.redirects.get(&cur) {
                Some(r) => cur = r.to,
                None => break,
            }
        }
        cur
    }

    pub fn resource(&self, id: ResourceId) -> Option<&Resource> {
        self.resources.get(&self.resolve_id(id))
    }

    pub fn predicate(&self, key_or_id: &str) -> Option<&PredicateDef> {
        if let Some(id) = self.predicate_by_key.get(key_or_id) {
            return self.predicates.get(id);
        }
        key_or_id.parse::<ResourceId>().ok().and_then(|id| self.predicates.get(&id))
    }

    pub fn type_id(&self, key_or_id: &str) -> Option<ResourceId> {
        self.type_by_key.get(key_or_id).copied().or_else(|| key_or_id.parse::<ResourceId>().ok().filter(|id| self.types.contains_key(id)))
    }

    pub fn branch_id(&self, name_or_id: &str) -> Option<BranchId> {
        self.branch_by_name.get(name_or_id).copied().or_else(|| name_or_id.parse::<BranchId>().ok().filter(|b| self.branches.contains_key(b)))
    }

    pub fn calendar(&self, key: &str) -> Option<CalendarFrame> {
        self.calendars.get(key).cloned().or_else(|| CalendarFrame::builtin(key))
    }

    /// 型とその上位型すべて。
    pub fn type_closure(&self, types: impl IntoIterator<Item = ResourceId>) -> BTreeSet<ResourceId> {
        let mut out = BTreeSet::new();
        let mut stack: Vec<ResourceId> = types.into_iter().collect();
        while let Some(t) = stack.pop() {
            if out.insert(t) {
                if let Some(def) = self.types.get(&t) {
                    stack.extend(def.parents.iter().copied());
                }
            }
        }
        out
    }

    pub fn types_mask(&self, types: &BTreeSet<ResourceId>) -> u64 {
        types.iter().filter_map(|t| self.type_bits.get(t)).fold(0u64, |m, b| m | (1u64 << b))
    }

    /// ラベル完全一致（正規化後）で Resource を探す。リダイレクト解決済み・重複除去済み。
    pub fn find_by_label(&self, label: &str) -> Vec<ResourceId> {
        let mut v: Vec<ResourceId> =
            self.label_index.get(&normalize_label(label)).map(|s| s.iter().map(|id| self.resolve_id(*id)).collect()).unwrap_or_default();
        v.sort();
        v.dedup();
        v
    }

    pub fn find_by_external(&self, ext: &ExternalId) -> Option<ResourceId> {
        self.external_index.get(ext).map(|id| self.resolve_id(*id))
    }

    /// ブランチの可視チェーン: (ブランチ, そのブランチで見える最大 Revision 通番)。
    pub fn branch_chain(&self, b: BranchId) -> Result<Vec<(BranchId, u64)>> {
        let mut out = Vec::new();
        let mut cur = Some(b);
        let mut limit = u64::MAX;
        while let Some(id) = cur {
            let br = self.branches.get(&id).ok_or_else(|| Error::not_found(format!("branch {id}")))?;
            out.push((id, limit));
            if out.len() > 64 {
                return Err(Error::DepthExceeded("branch chain deeper than 64".into()));
            }
            limit = limit.min(br.fork_seq);
            cur = br.parent;
        }
        Ok(out)
    }

    pub fn seq_of(&self, rev: &RevisionId) -> u64 {
        self.revision_seq.get(rev).copied().unwrap_or(0)
    }

    /// Resource と、それへ統合された Resource の ID 群。
    pub fn identity_group(&self, id: ResourceId) -> Vec<ResourceId> {
        let mut out = vec![id];
        let mut i = 0;
        while i < out.len() && out.len() < 256 {
            if let Some(m) = self.merged_members.get(&out[i]) {
                out.extend(m.iter().copied());
            }
            i += 1;
        }
        out
    }

    pub fn register_revision(&mut self, rev: &Revision) {
        self.revision_seq.insert(rev.id, rev.seq);
        self.head_seq = self.head_seq.max(rev.seq);
        self.revisions.push(rev.header());
    }

    fn index_labels(&mut self, id: ResourceId, labels: &[Label]) -> Vec<String> {
        let mut out = vec![];
        for l in labels {
            let n = normalize_label(&l.text);
            if !n.is_empty() {
                self.label_index.entry(n.clone()).or_default().insert(id);
                out.push(n);
            }
        }
        out
    }

    fn first_known(&self, a: &Assertion) -> Tick {
        a.evidence.iter().filter_map(|e| self.acquisitions.get(&e.acquisition)).map(|q| q.acquired_at).min().unwrap_or(a.created_at)
    }

    fn assertion_touch(&self, a: &Assertion) -> Touch {
        let role = self.predicates.get(&a.predicate).map(|p| p.role);
        let mut resources = vec![a.subject];
        if let Value::Resource(o) = a.object {
            resources.push(o);
        }
        Touch { branch: Some(a.branch), resources, role, ..Default::default() }
    }

    // ------------------------------------------------------------ apply

    /// Command を適用する。検証は書き込み API 側で済ませている前提で、
    /// ここでは参照先が無い場合も状態を壊さないように無視する（ログ再生の頑健性のため）。
    pub fn apply(&mut self, rev: &Revision, cmd: &Command) -> Touch {
        match cmd {
            Command::DefineType { def } => {
                if !self.type_bits.contains_key(&def.id) && self.type_bits.len() < 64 {
                    let bit = self.type_bits.len() as u8;
                    self.type_bits.insert(def.id, bit);
                }
                self.type_by_key.insert(def.key.clone(), def.id);
                self.types.insert(def.id, def.clone());
                Touch { global: true, ..Default::default() }
            }
            Command::DefinePredicate { def } => {
                self.predicate_by_key.insert(def.key.clone(), def.id);
                self.predicates.insert(def.id, def.clone());
                Touch { global: true, ..Default::default() }
            }
            Command::SetPredicateStatus { id, status } => {
                if let Some(p) = self.predicates.get_mut(id) {
                    p.status = *status;
                }
                Touch { global: true, ..Default::default() }
            }
            Command::DefineCalendar { frame } => {
                self.calendars.insert(frame.key.clone(), frame.clone());
                Touch { global: true, ..Default::default() }
            }
            Command::DefineFrame { frame } => {
                self.frames.insert(frame.clone());
                Touch { global: true, ..Default::default() }
            }
            Command::DefineLicense { license } => {
                self.licenses.insert(license.key.clone(), license.clone());
                Touch { global: true, ..Default::default() }
            }
            Command::SetRankPolicy { policy } => {
                self.rank_policy = policy.clone();
                Touch { global: true, ..Default::default() }
            }
            Command::CreateResource { resource } => {
                let id = resource.id;
                let labels = self.index_labels(id, &resource.labels);
                for e in &resource.external_ids {
                    self.external_index.insert(e.clone(), id);
                }
                self.resources.insert(id, resource.clone());
                Touch { all_branches: true, resources: vec![id], labels, ..Default::default() }
            }
            Command::UpdateResource { id, add_labels, add_descriptions, add_external_ids, add_types, remove_types } => {
                let labels = self.index_labels(*id, add_labels);
                for e in add_external_ids {
                    self.external_index.insert(e.clone(), *id);
                }
                if let Some(r) = self.resources.get_mut(id) {
                    for l in add_labels {
                        if !r.labels.contains(l) {
                            r.labels.push(l.clone());
                        }
                    }
                    for d in add_descriptions {
                        if !r.descriptions.contains(d) {
                            r.descriptions.push(d.clone());
                        }
                    }
                    for e in add_external_ids {
                        if !r.external_ids.contains(e) {
                            r.external_ids.push(e.clone());
                        }
                    }
                    r.types.extend(add_types.iter().copied());
                    for t in remove_types {
                        r.types.remove(t);
                    }
                }
                Touch { all_branches: true, resources: vec![*id], labels, ..Default::default() }
            }
            Command::AddAssertion { assertion } => {
                let mut a = assertion.clone();
                a.first_known_at = self.first_known(&a).min(a.first_known_at);
                self.by_subject.entry(a.subject).or_default().push(a.id);
                if let Value::Resource(o) = a.object {
                    self.by_object.entry(o).or_default().push(a.id);
                    self.by_object_predicate.entry((o, a.predicate)).or_default().push(a.id);
                }
                self.by_predicate.entry(a.predicate).or_default().push(a.id);
                for e in &a.evidence {
                    if let Some(d) = e.derivation {
                        self.by_derivation.entry(d).or_default().push(a.id);
                    }
                }
                if let Some(orig) = a.overrides {
                    self.overrides.entry(orig).or_default().push(a.id);
                }
                let t = self.assertion_touch(&a);
                self.assertions.insert(a.id, a);
                t
            }
            Command::ChangeStatus { id, change } => {
                let Some(a) = self.assertions.get_mut(id) else { return Touch::default() };
                a.status = change.to;
                a.status_history.push(change.clone());
                let a = a.clone();
                self.assertion_touch(&a)
            }
            Command::LinkSupersede { old, new } => {
                if let Some(a) = self.assertions.get_mut(old) {
                    a.superseded_by = Some(*new);
                }
                if let Some(a) = self.assertions.get_mut(new) {
                    a.supersedes = Some(*old);
                }
                match self.assertions.get(old) {
                    Some(a) => self.assertion_touch(&a.clone()),
                    None => Touch::default(),
                }
            }
            Command::SetHumanVerified { id, verified } => {
                let Some(a) = self.assertions.get_mut(id) else { return Touch::default() };
                a.confidence.human_verified = *verified;
                let a = a.clone();
                self.assertion_touch(&a)
            }
            Command::AddEvidence { id, evidence } => {
                let acquired = self.acquisitions.get(&evidence.acquisition).map(|q| q.acquired_at);
                let Some(a) = self.assertions.get_mut(id) else { return Touch::default() };
                if !a.evidence.contains(evidence) {
                    a.evidence.push(evidence.clone());
                }
                if let Some(t) = acquired {
                    a.first_known_at = a.first_known_at.min(t);
                }
                if let Some(d) = evidence.derivation {
                    self.by_derivation.entry(d).or_default().push(*id);
                }
                let a = a.clone();
                self.assertion_touch(&a)
            }
            Command::RegisterSource { source } => {
                self.source_by_locator.insert(source.locator.key(), source.id);
                if let Some(r) = source.resource {
                    self.sources_by_resource.entry(r).or_default().push(source.id);
                }
                self.sources.insert(source.id, source.clone());
                Touch { all_branches: true, resources: source.resource.into_iter().collect(), ..Default::default() }
            }
            Command::RegisterAcquisition { acquisition } => {
                self.acquisitions_by_source.entry(acquisition.source).or_default().push(acquisition.id);
                self.acquisitions.insert(acquisition.id, acquisition.clone());
                let res = self.sources.get(&acquisition.source).and_then(|s| s.resource);
                Touch { all_branches: true, resources: res.into_iter().collect(), ..Default::default() }
            }
            Command::RegisterDerivation { derivation } => {
                self.derivations.insert(derivation.id, derivation.clone());
                Touch::default()
            }
            Command::ProposeMerge { proposal } => {
                self.merge_proposals.insert(proposal.id, proposal.clone());
                Touch::default()
            }
            Command::DecideMerge { id, approve, by, at, redirect } => {
                let Some(p) = self.merge_proposals.get_mut(id) else { return Touch::default() };
                p.status = if *approve { MergeStatus::Approved } else { MergeStatus::Rejected };
                p.decided_by = Some(by.clone());
                p.decided_at = Some(*at);
                let (from, into) = (p.from, p.into);
                if let (true, Some(r)) = (*approve, redirect) {
                    self.redirects.insert(r.from, r.clone());
                    self.merged_members.entry(r.to).or_default().push(r.from);
                }
                Touch { all_branches: true, resources: vec![from, into], role: Some(PredicateRole::Identity), ..Default::default() }
            }
            Command::CreateBranch { branch } => {
                self.branch_by_name.insert(branch.name.clone(), branch.id);
                self.branches.insert(branch.id, branch.clone());
                Touch::default()
            }
            Command::DefineSequence { sequence } => {
                self.sequences.insert(sequence.id, sequence.clone());
                Touch::default()
            }
            Command::DefineTable { table } => {
                self.tables.insert(table.id, table.clone());
                Touch::default()
            }
            Command::LinkRow { link } => {
                self.row_links.entry((link.table, link.row_key.clone())).or_default().push(link.resource);
                self.rows_by_resource.entry(link.resource).or_default().push(link.clone());
                Touch::default()
            }
            Command::CreateKey { key_id } => {
                self.keys.insert(*key_id);
                Touch::default()
            }
            Command::ShredKey { key_id } => {
                self.shredded_keys.insert(*key_id);
                // 暗号化値を含む Projection（要約）を作り直す。
                Touch { global: true, ..Default::default() }
            }
            Command::AddObservation { .. }
            | Command::AddTrajectory { .. }
            | Command::AddTableRows { .. }
            | Command::DefineEmbeddingSpace { .. }
            | Command::SetEmbedding { .. } => {
                let _ = rev;
                Touch::default()
            }
        }
    }
}
