//! AI エージェント向けの意味的書き込み API。SQL による直接更新は提供しない。
//!
//! - AI（`ActorKind::Agent`）による追加は常に `proposed` から始まる。
//! - 新規 Predicate / Type も AI は `proposed` までしか作れない。
//! - Identity の統合は提案（merge candidate）→ キュレーターの確認 → identity_redirect の順。
//! - 継承したブランチの Assertion を変更するときは copy-on-write で上書きコピーを作る。

use crate::command::Command;
use crate::kb::KnowledgeBase;
use crate::store::object::content_hash;
use crate::vector::EmbeddingSpace;
use base64::Engine as _;
use chronotope_core::model::*;
use chronotope_core::rank::RankPolicy;
use chronotope_core::space::{Placement, Point, SpatialReferenceFrame};
use chronotope_core::time::Tick;
use chronotope_core::time::calendar::CalendarFrame;
use chronotope_core::time::expr::{TemporalExpression, TimeAst};
use chronotope_core::vocab::keys;
use chronotope_core::*;
use serde::Deserialize;
use serde_json::{Value as Json, json};
use std::collections::BTreeSet;

// ------------------------------------------------------------------ inputs

/// Resource の指定方法。ラベルによる指定は曖昧なので書き込みでは受け付けない。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ResourceRef {
    /// `res_...` または外部 ID（`wikidata:Q1490`）。
    Id(String),
    New {
        new: NewResource,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewResource {
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub external_ids: Vec<String>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
    #[serde(default)]
    pub license: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ValueInput {
    Resource {
        resource: ResourceRef,
    },
    Time {
        time: String,
        #[serde(default)]
        calendar: Option<String>,
    },
    Quantity {
        quantity: f64,
        #[serde(default)]
        unit: Option<String>,
    },
    Text {
        text: String,
        #[serde(default)]
        lang: Option<String>,
    },
    Bool {
        bool: bool,
    },
    Geo {
        geo: Placement,
    },
    Json {
        json: Json,
    },
    Unknown {
        unknown: bool,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct DerivationInput {
    pub extractor: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_version: Option<String>,
    #[serde(default)]
    pub schema_version: Option<String>,
    #[serde(default)]
    pub source_span: Option<Locator>,
    #[serde(default)]
    pub extraction_conf: Option<f32>,
    #[serde(default)]
    pub extracted_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvidenceInput {
    pub acquisition: AcquisitionId,
    #[serde(default)]
    pub derivation: Option<DerivationInput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProposeAssertion {
    pub subject: ResourceRef,
    pub predicate: String,
    pub object: ValueInput,
    #[serde(default)]
    pub negated: bool,
    #[serde(default)]
    pub valid_time: Option<String>,
    /// 時間リテラルの既定カレンダー（例: `gregorian+09:00`）。
    #[serde(default)]
    pub calendar: Option<String>,
    #[serde(default)]
    pub spatial_scope: Option<ResourceRef>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub timeline: Option<ResourceRef>,
    #[serde(default)]
    pub canon: Option<ResourceRef>,
    #[serde(default)]
    pub evidence: Vec<EvidenceInput>,
    #[serde(default)]
    pub source_reliability: Option<f32>,
    #[serde(default)]
    pub extraction_conf: Option<f32>,
    #[serde(default)]
    pub status: Option<AssertionStatus>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
    #[serde(default)]
    pub license: Option<String>,
    /// crypto-shredding 用の鍵で値を暗号化する（個人情報など）。
    #[serde(default)]
    pub protect_with: Option<KeyId>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcquisitionInput {
    /// 外部エージェント・クローラーが取得した時刻（必須）。
    pub acquired_at: String,
    #[serde(default)]
    pub acquired_by: Option<String>,
    #[serde(default)]
    pub method: Option<AcquisitionMethod>,
    #[serde(default)]
    pub locator: Option<Locator>,
    /// スナップショット本文（テキスト）。
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub content_base64: Option<String>,
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub encrypt_with: Option<KeyId>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LinkSource {
    #[serde(default)]
    pub source: Option<SourceId>,
    #[serde(default)]
    pub kind: Option<SourceKind>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub locator: Option<Locator>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub source_time: Option<String>,
    #[serde(default)]
    pub calendar: Option<String>,
    #[serde(default)]
    pub origin: Option<SourceOrigin>,
    #[serde(default)]
    pub reliability: Option<f32>,
    #[serde(default)]
    pub provenance_root: Option<SourceId>,
    #[serde(default)]
    pub license: Option<String>,
    /// 情報源を表す Resource（Document / Post）。`{"new": {...}}` で同時作成できる。
    #[serde(default)]
    pub resource: Option<ResourceRef>,
    pub acquisition: AcquisitionInput,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PredicateInput {
    pub key: String,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub domain: Vec<String>,
    #[serde(default)]
    pub range: Option<PredicateRange>,
    #[serde(default)]
    pub inverse: Option<String>,
    #[serde(default)]
    pub transitive: bool,
    #[serde(default)]
    pub symmetric: bool,
    #[serde(default)]
    pub functional: bool,
    #[serde(default)]
    pub role: Option<PredicateRole>,
    #[serde(default)]
    pub mappings: Vec<VocabMapping>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TypeInput {
    pub key: String,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub mappings: Vec<VocabMapping>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SampleInput {
    pub t: String,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub z: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WriteRequest {
    CreateResource {
        resource: NewResource,
    },
    UpdateResource {
        id: String,
        #[serde(default)]
        labels: Vec<Label>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        lang: Option<String>,
        #[serde(default)]
        external_ids: Vec<String>,
        #[serde(default)]
        add_types: Vec<String>,
    },
    ProposeAssertion(Box<ProposeAssertion>),
    RetractAssertion {
        id: AssertionId,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        basis_acquisition: Option<AcquisitionId>,
    },
    SupersedeAssertion {
        id: AssertionId,
        replacement: Box<ProposeAssertion>,
        #[serde(default)]
        reason: Option<String>,
    },
    AcceptAssertion {
        id: AssertionId,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        branch: Option<String>,
    },
    DisputeAssertion {
        id: AssertionId,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        branch: Option<String>,
    },
    VerifyAssertion {
        id: AssertionId,
    },
    AddEvidence {
        assertion: AssertionId,
        evidence: EvidenceInput,
    },
    MergeIdentity {
        from: String,
        into: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        evidence: Vec<AssertionId>,
    },
    DecideMerge {
        id: MergeProposalId,
        approve: bool,
    },
    LinkSource(Box<LinkSource>),
    AddObservation {
        target: String,
        metric: String,
        observed_at: String,
        value: f64,
        #[serde(default)]
        unit: Option<String>,
        #[serde(default)]
        acquisition: Option<AcquisitionId>,
        #[serde(default)]
        branch: Option<String>,
    },
    AddTrajectory {
        target: String,
        frame: FrameId,
        samples: Vec<SampleInput>,
        #[serde(default)]
        interpolation: Interpolation,
        #[serde(default)]
        acquisition: Option<AcquisitionId>,
        #[serde(default)]
        external_uri: Option<String>,
    },
    ProposePredicate(Box<PredicateInput>),
    AcceptPredicate {
        key: String,
    },
    ProposeType(Box<TypeInput>),
    DefineCalendar {
        frame: CalendarFrame,
    },
    DefineFrame {
        frame: SpatialReferenceFrame,
    },
    DefineLicense {
        license: License,
    },
    SetRankPolicy {
        policy: RankPolicy,
    },
    CreateBranch {
        name: String,
        #[serde(default)]
        parent: Option<String>,
        #[serde(default)]
        description: Option<String>,
    },
    MaterializeBranch {
        branch: String,
    },
    DefineSequence {
        scope: String,
        kind: String,
        items: Vec<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        canon: Option<String>,
        #[serde(default)]
        label: Option<String>,
    },
    DefineTable {
        dataset: ResourceRef,
        name: String,
        columns: Vec<ColumnDef>,
        #[serde(default)]
        primary_key: Vec<String>,
        #[serde(default)]
        foreign_keys: Vec<ForeignKey>,
        storage: TableStorage,
    },
    AddTableRows {
        table: TableId,
        rows: Vec<serde_json::Map<String, Json>>,
    },
    LinkRow {
        table: TableId,
        row_key: String,
        resource: String,
    },
    SetEmbedding {
        resource: String,
        model: String,
        version: String,
        vector: Vec<f32>,
    },
    CreateKey {},
    ShredKey {
        key_id: KeyId,
    },
}

// ------------------------------------------------------------------ helpers

fn parse_time(s: &str, what: &str) -> Result<Tick> {
    Tick::parse_iso(s).map_err(|e| Error::invalid(format!("{what}: {e}")))
}

/// 1 回の書き込みで作る Command 群と、そこで新規作成した Resource。
#[derive(Default)]
struct Batch {
    cmds: Vec<Command>,
    created: Vec<(ResourceId, BTreeSet<ResourceId>)>,
    warnings: Vec<String>,
}

impl KnowledgeBase {
    fn require_curator(&self, p: &Principal, what: &str) -> Result<()> {
        if p.curator && !p.actor.is_ai() { Ok(()) } else { Err(Error::forbidden(format!("{what} requires a human curator"))) }
    }

    fn branch_or_main(&self, b: &Option<String>) -> Result<BranchId> {
        match b {
            None => Ok(BranchId::main()),
            Some(name) => self.store.branch_id(name).ok_or_else(|| Error::not_found(format!("branch `{name}`"))),
        }
    }

    /// 読み取りでも使う ID 解決（`res_...` / 外部 ID）。
    pub fn resolve_ref_str(&self, s: &str) -> Result<ResourceId> {
        if let Ok(id) = s.parse::<ResourceId>() {
            let id = self.store.resolve_id(id);
            return self.store.resources.contains_key(&id).then_some(id).ok_or_else(|| Error::not_found(format!("resource {s}")));
        }
        if let Some(ext) = ExternalId::parse(s) {
            return self.store.find_by_external(&ext).ok_or_else(|| Error::not_found(format!("external id {s}")));
        }
        Err(Error::invalid(format!("`{s}` is not a resource id or external id (labels are not accepted for writes)")))
    }

    fn new_resource(&self, p: &Principal, n: &NewResource, rev_placeholder: RevisionId, batch: &mut Batch) -> Result<ResourceId> {
        let mut types = BTreeSet::new();
        for t in &n.types {
            types.insert(self.store.type_id(t).ok_or_else(|| Error::not_found(format!("type `{t}`")))?);
        }
        let mut labels = n.labels.clone();
        if let Some(l) = &n.label {
            labels.insert(0, Label::preferred(l, n.lang.as_deref()));
        }
        labels.extend(n.aliases.iter().map(|a| Label::alias(a, n.lang.as_deref())));
        if labels.is_empty() {
            return Err(Error::invalid("a new resource needs at least one label"));
        }
        let mut external_ids = vec![];
        for e in &n.external_ids {
            let ext = ExternalId::parse(e).ok_or_else(|| Error::invalid(format!("bad external id `{e}` (expected scheme:value)")))?;
            if let Some(existing) = self.store.find_by_external(&ext) {
                return Err(Error::Conflict(format!("external id {e} already belongs to {existing}")));
            }
            external_ids.push(ext);
        }
        if let Some(l) = &n.license {
            if !self.store.licenses.contains_key(l) {
                return Err(Error::not_found(format!("license `{l}`")));
            }
        }
        let id = ResourceId::new();
        let resource = Resource {
            id,
            types: types.clone(),
            labels,
            descriptions: n.description.iter().map(|d| LocalizedText { text: d.clone(), lang: n.lang.clone() }).collect(),
            external_ids,
            visibility: n.visibility.clone().unwrap_or_default(),
            license: n.license.clone(),
            created_at: self.now(),
            created_by: p.actor.clone(),
            created_revision: rev_placeholder,
        };
        batch.cmds.push(Command::CreateResource { resource });
        batch.created.push((id, types));
        Ok(id)
    }

    fn resolve_ref(&self, p: &Principal, r: &ResourceRef, rev: RevisionId, batch: &mut Batch) -> Result<ResourceId> {
        match r {
            ResourceRef::Id(s) => self.resolve_ref_str(s),
            ResourceRef::New { new } => self.new_resource(p, new, rev, batch),
        }
    }

    fn types_of(&self, id: ResourceId, batch: &Batch) -> BTreeSet<ResourceId> {
        let direct: BTreeSet<ResourceId> = match batch.created.iter().find(|(x, _)| *x == id) {
            Some((_, t)) => t.clone(),
            None => self.store.resource(id).map(|r| r.types.clone()).unwrap_or_default(),
        };
        self.store.type_closure(direct)
    }

    fn calendar_key(&self, c: &Option<String>) -> Result<String> {
        let key = c.clone().unwrap_or_else(|| "gregorian".into());
        self.store.calendar(&key).map(|_| key.clone()).ok_or_else(|| Error::not_found(format!("calendar `{key}`")))
    }

    fn time_expr(&self, raw: &str, calendar: &Option<String>, batch: &mut Batch) -> Result<TemporalExpression> {
        let expr = TemporalExpression::parse(raw, &self.calendar_key(calendar)?);
        if matches!(expr.ast, TimeAst::Unparsed) {
            batch.warnings.push(format!("temporal expression `{raw}` could not be parsed; raw text is stored and it stays unresolved"));
        }
        Ok(expr)
    }

    fn derivation_cmd(&self, acquisition: AcquisitionId, d: &DerivationInput, batch: &mut Batch) -> Result<DerivationId> {
        let id = DerivationId::new();
        let extracted_at = match &d.extracted_at {
            Some(s) => parse_time(s, "extracted_at")?,
            None => self.now(),
        };
        batch.cmds.push(Command::RegisterDerivation {
            derivation: Derivation {
                id,
                acquisition,
                extractor: d.extractor.clone(),
                model: d.model.clone(),
                model_version: d.model_version.clone(),
                schema_version: d.schema_version.clone(),
                source_span: d.source_span.clone(),
                extracted_at,
                extraction_conf: d.extraction_conf,
            },
        });
        Ok(id)
    }

    fn check_range(&self, pred: &PredicateDef, value: &Value, subject: ResourceId, batch: &Batch) -> Result<()> {
        use chronotope_core::model::LiteralKind as L;
        let ok = match (&pred.range, value) {
            (PredicateRange::Any, _) | (_, Value::Unknown) => true,
            (PredicateRange::Resource { types }, Value::Resource(o)) => {
                types.is_empty() || {
                    let ot = self.types_of(*o, batch);
                    types.iter().any(|t| ot.contains(t))
                }
            }
            (PredicateRange::Literal { literal }, v) => matches!(
                (literal, v),
                (L::Text, Value::Text { .. })
                    | (L::Quantity, Value::Quantity { .. })
                    | (L::Bool, Value::Bool(_))
                    | (L::Time, Value::Time(_))
                    | (L::Geo, Value::Geo(_))
                    | (L::Json, Value::Json(_))
            ),
            _ => false,
        };
        if !ok {
            return Err(Error::invalid(format!("value does not match the range of predicate `{}` ({:?})", pred.key, pred.range)));
        }
        if !pred.domain.is_empty() {
            let st = self.types_of(subject, batch);
            if !pred.domain.iter().any(|t| st.contains(t)) {
                return Err(Error::invalid(format!("subject type is outside the domain of predicate `{}`", pred.key)));
            }
        }
        Ok(())
    }

    fn build_assertion(&self, p: &Principal, input: &ProposeAssertion, rev: RevisionId, batch: &mut Batch) -> Result<Assertion> {
        let pred = self
            .store
            .predicate(&input.predicate)
            .ok_or_else(|| Error::not_found(format!("predicate `{}` (propose it first with propose_predicate)", input.predicate)))?
            .clone();
        if pred.status == VocabStatus::Deprecated {
            return Err(Error::invalid(format!("predicate `{}` is deprecated", pred.key)));
        }
        if pred.status == VocabStatus::Proposed {
            batch.warnings.push(format!("predicate `{}` is only proposed; the assertion is stored but the vocabulary is not yet accepted", pred.key));
        }
        let branch = self.branch_or_main(&input.branch)?;
        let subject = self.resolve_ref(p, &input.subject, rev, batch)?;
        let object = match &input.object {
            ValueInput::Resource { resource } => Value::Resource(self.resolve_ref(p, resource, rev, batch)?),
            ValueInput::Time { time, calendar } => Value::Time(self.time_expr(time, &calendar.clone().or(input.calendar.clone()), batch)?),
            ValueInput::Quantity { quantity, unit } => Value::Quantity { amount: *quantity, unit: unit.clone() },
            ValueInput::Text { text, lang } => Value::Text { text: text.clone(), lang: lang.clone() },
            ValueInput::Bool { bool } => Value::Bool(*bool),
            ValueInput::Geo { geo } => {
                if self.store.frames.get(&geo.frame).is_none() {
                    return Err(Error::not_found(format!("spatial reference frame {}", geo.frame)));
                }
                Value::Geo(geo.clone())
            }
            ValueInput::Json { json } => Value::Json(json.clone()),
            ValueInput::Unknown { .. } => Value::Unknown,
        };
        self.check_range(&pred, &object, subject, batch)?;
        if matches!(pred.key.as_str(), keys::SAME_AS | keys::POSSIBLY_SAME_AS | keys::DISTINCT_FROM) && object.as_resource() == Some(subject) {
            return Err(Error::invalid("identity relation to itself"));
        }
        let object = match input.protect_with {
            Some(k) => {
                if !self.vault.has_key(&k) {
                    return Err(Error::not_found(format!("key {k} (shredded or never created)")));
                }
                Value::Protected(self.vault.protect(k, &object)?)
            }
            None => object,
        };
        let mut evidence = vec![];
        let mut first_known = self.now();
        for e in &input.evidence {
            let acq = self.store.acquisitions.get(&e.acquisition).ok_or_else(|| Error::not_found(format!("acquisition {}", e.acquisition)))?;
            first_known = first_known.min(acq.acquired_at);
            let derivation = match &e.derivation {
                Some(d) => Some(self.derivation_cmd(e.acquisition, d, batch)?),
                None => None,
            };
            evidence.push(Evidence { acquisition: e.acquisition, derivation });
        }
        let requested = input.status.unwrap_or(AssertionStatus::Proposed);
        let status = if p.actor.is_ai() || !p.curator {
            if requested != AssertionStatus::Proposed {
                batch.warnings.push(format!("status `{}` requested by a non-curator; stored as `proposed`", format!("{requested:?}").to_lowercase()));
            }
            AssertionStatus::Proposed
        } else if matches!(requested, AssertionStatus::Accepted | AssertionStatus::Proposed | AssertionStatus::Disputed) {
            requested
        } else {
            return Err(Error::invalid("new assertions must be accepted, proposed or disputed"));
        };
        if let Some(l) = &input.license {
            if !self.store.licenses.contains_key(l) {
                return Err(Error::not_found(format!("license `{l}`")));
            }
        }
        let resolve_opt =
            |r: &Option<ResourceRef>, batch: &mut Batch| -> Result<Option<ResourceId>> { r.as_ref().map(|r| self.resolve_ref(p, r, rev, batch)).transpose() };
        let valid_time = match &input.valid_time {
            Some(v) => Some(self.time_expr(v, &input.calendar, batch)?),
            None => None,
        };
        let spatial_scope = resolve_opt(&input.spatial_scope, batch)?;
        let timeline = resolve_opt(&input.timeline, batch)?;
        let canon = resolve_opt(&input.canon, batch)?;
        Ok(Assertion {
            id: AssertionId::new(),
            subject,
            predicate: pred.id,
            object,
            polarity: if input.negated { Polarity::Negated } else { Polarity::Affirmed },
            valid_time,
            spatial_scope,
            branch,
            timeline,
            canon,
            status,
            confidence: ConfidenceComponents { source_reliability: input.source_reliability, extraction_conf: input.extraction_conf, ..Default::default() },
            rank: None,
            evidence,
            supersedes: None,
            superseded_by: None,
            overrides: None,
            asserted_by: p.actor.clone(),
            created_revision: rev,
            created_at: self.now(),
            first_known_at: first_known,
            status_history: vec![],
            visibility: input.visibility.clone().unwrap_or_default(),
            license: input.license.clone(),
            note: input.note.clone(),
        })
    }

    fn commit_batch(&mut self, p: &Principal, branch: BranchId, msg: &str, rev: RevisionId, batch: Batch, mut extra: Json) -> Result<Json> {
        // Revision ID は Command 内の created_revision と一致させる。
        let Batch { cmds, warnings, .. } = batch;
        let header = {
            let actor = p.actor.clone();
            let r = crate::command::Revision { id: rev, seq: 0, branch, actor, message: Some(msg.to_string()), committed_at: self.now(), commands: cmds };
            self.commit_prepared(r)?
        };
        if let Json::Object(m) = &mut extra {
            m.insert("revision".into(), json!(header.seq));
            if !warnings.is_empty() {
                m.insert("warnings".into(), json!(warnings));
            }
        }
        Ok(extra)
    }

    /// Status の変更。継承 Assertion なら copy-on-write でこのブランチに上書きコピーを作る。
    #[allow(clippy::too_many_arguments)]
    fn status_cmds(
        &self,
        p: &Principal,
        id: AssertionId,
        branch: BranchId,
        to: AssertionStatus,
        reason: Option<String>,
        basis: Option<Tick>,
        rev: RevisionId,
        batch: &mut Batch,
    ) -> Result<AssertionId> {
        let a = self.store.assertions.get(&id).ok_or_else(|| Error::not_found(format!("assertion {id}")))?;
        let view = crate::view::View { statuses: crate::view::StatusSet::all(), principal: p.clone(), ..crate::view::View::projection(&self.store, branch)? };
        let from = self.store.effective_status(a, &view).ok_or_else(|| Error::not_found(format!("assertion {id} is not visible in this branch")))?;
        let change = StatusChange { from, to, revision: rev, by: p.actor.clone(), recorded_at: self.now(), basis_acquired_at: basis, reason };
        if a.branch == branch {
            batch.cmds.push(Command::ChangeStatus { id, change });
            Ok(id)
        } else {
            let mut copy = a.clone();
            copy.id = AssertionId::new();
            copy.branch = branch;
            copy.overrides = Some(a.id);
            copy.created_revision = rev;
            copy.created_at = self.now();
            copy.status = to;
            copy.status_history = vec![change];
            let new_id = copy.id;
            batch.cmds.push(Command::AddAssertion { assertion: copy });
            batch.warnings.push(format!("assertion {id} is inherited from branch {}; a copy-on-write override {new_id} was created", a.branch));
            Ok(new_id)
        }
    }

    // ------------------------------------------------------------------ entry point

    pub fn write(&mut self, p: &Principal, req: WriteRequest) -> Result<Json> {
        let rev = RevisionId::new();
        let mut batch = Batch::default();
        match req {
            WriteRequest::CreateResource { resource } => {
                let id = self.new_resource(p, &resource, rev, &mut batch)?;
                self.commit_batch(p, BranchId::main(), "create_resource", rev, batch, json!({ "id": id }))
            }
            WriteRequest::UpdateResource { id, labels, description, lang, external_ids, add_types } => {
                let id = self.resolve_ref_str(&id)?;
                let mut exts = vec![];
                for e in &external_ids {
                    let ext = ExternalId::parse(e).ok_or_else(|| Error::invalid(format!("bad external id `{e}`")))?;
                    if let Some(other) = self.store.find_by_external(&ext).filter(|o| *o != id) {
                        return Err(Error::Conflict(format!("external id {e} already belongs to {other}")));
                    }
                    exts.push(ext);
                }
                let mut types = vec![];
                for t in &add_types {
                    types.push(self.store.type_id(t).ok_or_else(|| Error::not_found(format!("type `{t}`")))?);
                }
                batch.cmds.push(Command::UpdateResource {
                    id,
                    add_labels: labels,
                    add_descriptions: description.map(|d| LocalizedText { text: d, lang }).into_iter().collect(),
                    add_external_ids: exts,
                    add_types: types,
                    remove_types: vec![],
                });
                self.commit_batch(p, BranchId::main(), "update_resource", rev, batch, json!({ "id": id }))
            }
            WriteRequest::ProposeAssertion(input) => {
                let a = self.build_assertion(p, &input, rev, &mut batch)?;
                let (id, subject, branch, status) = (a.id, a.subject, a.branch, a.status);
                let created: Vec<ResourceId> = batch.created.iter().map(|(id, _)| *id).collect();
                batch.cmds.push(Command::AddAssertion { assertion: a });
                self.commit_batch(
                    p,
                    branch,
                    "propose_assertion",
                    rev,
                    batch,
                    json!({ "id": id, "subject": subject, "status": status, "created_resources": created }),
                )
            }
            WriteRequest::RetractAssertion { id, reason, branch, basis_acquisition } => {
                let branch = self.branch_or_main(&branch)?;
                let a = self.store.assertions.get(&id).ok_or_else(|| Error::not_found(format!("assertion {id}")))?;
                let own = a.asserted_by.id == p.actor.id;
                let basis = basis_acquisition
                    .map(|q| self.store.acquisitions.get(&q).map(|x| x.acquired_at).ok_or_else(|| Error::not_found(format!("acquisition {q}"))))
                    .transpose()?;
                let to = if p.curator && !p.actor.is_ai() || own {
                    AssertionStatus::Retracted
                } else {
                    batch.warnings.push("only the author or a curator can retract; the assertion was marked `disputed` instead".into());
                    AssertionStatus::Disputed
                };
                let target = self.status_cmds(p, id, branch, to, reason, basis, rev, &mut batch)?;
                self.commit_batch(p, branch, "retract_assertion", rev, batch, json!({ "id": target, "status": to }))
            }
            WriteRequest::DisputeAssertion { id, reason, branch } => {
                let branch = self.branch_or_main(&branch)?;
                let target = self.status_cmds(p, id, branch, AssertionStatus::Disputed, reason, None, rev, &mut batch)?;
                self.commit_batch(p, branch, "dispute_assertion", rev, batch, json!({ "id": target, "status": "disputed" }))
            }
            WriteRequest::AcceptAssertion { id, reason, branch } => {
                self.require_curator(p, "accept_assertion")?;
                let branch = self.branch_or_main(&branch)?;
                let target = self.status_cmds(p, id, branch, AssertionStatus::Accepted, reason, None, rev, &mut batch)?;
                if let Some(old) = self.store.assertions.get(&id).and_then(|a| a.supersedes) {
                    self.status_cmds(p, old, branch, AssertionStatus::Superseded, Some(format!("superseded by {id}")), None, rev, &mut batch)?;
                }
                self.commit_batch(p, branch, "accept_assertion", rev, batch, json!({ "id": target, "status": "accepted" }))
            }
            WriteRequest::SupersedeAssertion { id, replacement, reason } => {
                let old = self.store.assertions.get(&id).ok_or_else(|| Error::not_found(format!("assertion {id}")))?.clone();
                let mut new = self.build_assertion(p, &replacement, rev, &mut batch)?;
                if new.subject != old.subject || new.predicate != old.predicate {
                    return Err(Error::invalid("a replacement must keep the subject and predicate"));
                }
                new.supersedes = Some(id);
                let (new_id, branch, status) = (new.id, new.branch, new.status);
                batch.cmds.push(Command::AddAssertion { assertion: new });
                batch.cmds.push(Command::LinkSupersede { old: id, new: new_id });
                if status == AssertionStatus::Accepted {
                    self.status_cmds(p, id, branch, AssertionStatus::Superseded, reason, None, rev, &mut batch)?;
                } else {
                    batch.warnings.push("the replacement is proposed; the old assertion becomes superseded when the replacement is accepted".into());
                }
                self.commit_batch(p, branch, "supersede_assertion", rev, batch, json!({ "id": new_id, "supersedes": id, "status": status }))
            }
            WriteRequest::VerifyAssertion { id } => {
                self.require_curator(p, "verify_assertion")?;
                if !self.store.assertions.contains_key(&id) {
                    return Err(Error::not_found(format!("assertion {id}")));
                }
                batch.cmds.push(Command::SetHumanVerified { id, verified: true });
                self.commit_batch(p, BranchId::main(), "verify_assertion", rev, batch, json!({ "id": id, "human_verified": true }))
            }
            WriteRequest::AddEvidence { assertion, evidence } => {
                let a = self.store.assertions.get(&assertion).ok_or_else(|| Error::not_found(format!("assertion {assertion}")))?;
                let branch = a.branch;
                if !self.store.acquisitions.contains_key(&evidence.acquisition) {
                    return Err(Error::not_found(format!("acquisition {}", evidence.acquisition)));
                }
                let derivation = evidence.derivation.as_ref().map(|d| self.derivation_cmd(evidence.acquisition, d, &mut batch)).transpose()?;
                batch.cmds.push(Command::AddEvidence { id: assertion, evidence: Evidence { acquisition: evidence.acquisition, derivation } });
                self.commit_batch(p, branch, "add_evidence", rev, batch, json!({ "id": assertion, "derivation": derivation }))
            }
            WriteRequest::MergeIdentity { from, into, reason, evidence } => {
                let from = self.resolve_ref_str(&from)?;
                let into = self.resolve_ref_str(&into)?;
                if from == into {
                    return Err(Error::invalid("cannot merge a resource into itself"));
                }
                let distinct = self.store.predicate_by_key.get(keys::DISTINCT_FROM).copied();
                let blocked = self.store.by_subject.get(&from).into_iter().chain(self.store.by_subject.get(&into)).flatten().any(|aid| {
                    let a = &self.store.assertions[aid];
                    Some(a.predicate) == distinct
                        && a.status.is_live()
                        && a.polarity == Polarity::Affirmed
                        && (a.object.as_resource() == Some(into) || a.object.as_resource() == Some(from))
                });
                if blocked {
                    return Err(Error::Conflict("a live distinct_from assertion exists between these resources".into()));
                }
                let id = MergeProposalId::new();
                batch.cmds.push(Command::ProposeMerge {
                    proposal: MergeProposal {
                        id,
                        from,
                        into,
                        proposed_by: p.actor.clone(),
                        proposed_at: self.now(),
                        reason,
                        evidence,
                        status: MergeStatus::Proposed,
                        decided_by: None,
                        decided_at: None,
                    },
                });
                batch.warnings.push("merge proposals are not applied automatically; a curator must call decide_merge".into());
                self.commit_batch(p, BranchId::main(), "merge_identity", rev, batch, json!({ "proposal": id, "status": "proposed" }))
            }
            WriteRequest::DecideMerge { id, approve } => {
                self.require_curator(p, "decide_merge")?;
                let mp = self.store.merge_proposals.get(&id).ok_or_else(|| Error::not_found(format!("merge proposal {id}")))?;
                if mp.status != MergeStatus::Proposed {
                    return Err(Error::Conflict(format!("merge proposal is already {:?}", mp.status)));
                }
                let (from, into) = (self.store.resolve_id(mp.from), self.store.resolve_id(mp.into));
                if approve && from == into {
                    return Err(Error::Conflict("resources are already merged".into()));
                }
                let redirect = approve.then(|| IdentityRedirect {
                    from,
                    to: into,
                    revision: rev,
                    approved_by: p.actor.clone(),
                    at: self.now(),
                    reason: mp.reason.clone(),
                });
                batch.cmds.push(Command::DecideMerge { id, approve, by: p.actor.clone(), at: self.now(), redirect });
                self.commit_batch(p, BranchId::main(), "decide_merge", rev, batch, json!({ "proposal": id, "approved": approve, "from": from, "into": into }))
            }
            WriteRequest::LinkSource(input) => self.link_source(p, *input, rev, batch),
            WriteRequest::AddObservation { target, metric, observed_at, value, unit, acquisition, branch } => {
                let target = self.resolve_ref_str(&target)?;
                let branch = self.branch_or_main(&branch)?;
                if let Some(a) = acquisition {
                    if !self.store.acquisitions.contains_key(&a) {
                        return Err(Error::not_found(format!("acquisition {a}")));
                    }
                }
                let id = ObservationId::new();
                batch.cmds.push(Command::AddObservation {
                    observation: Observation { id, target, metric, observed_at: parse_time(&observed_at, "observed_at")?, value, unit, acquisition, branch },
                });
                self.commit_batch(p, branch, "add_observation", rev, batch, json!({ "id": id }))
            }
            WriteRequest::AddTrajectory { target, frame, samples, interpolation, acquisition, external_uri } => {
                let target = self.resolve_ref_str(&target)?;
                if self.store.frames.get(&frame).is_none() {
                    return Err(Error::not_found(format!("frame {frame}")));
                }
                let samples = samples
                    .iter()
                    .map(|s| Ok(TrajectorySample { t: parse_time(&s.t, "sample time")?, pos: Point { x: s.x, y: s.y, z: s.z } }))
                    .collect::<Result<Vec<_>>>()?;
                let id = TrajectoryId::new();
                let storage = match external_uri {
                    Some(uri) => TrajectoryStorage::External { uri },
                    None => TrajectoryStorage::Inline,
                };
                batch.cmds.push(Command::AddTrajectory {
                    trajectory: Trajectory { id, target, reference_frame: frame, samples, interpolation, acquisition, storage },
                });
                self.commit_batch(p, BranchId::main(), "add_trajectory", rev, batch, json!({ "id": id }))
            }
            WriteRequest::ProposePredicate(input) => {
                if self.store.predicate_by_key.contains_key(&input.key) {
                    return Err(Error::Conflict(format!("predicate `{}` already exists", input.key)));
                }
                if !input.key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                    return Err(Error::invalid("predicate keys are snake_case ascii"));
                }
                let lookup_type = |k: &String| self.store.type_id(k).ok_or_else(|| Error::not_found(format!("type `{k}`")));
                let domain = input.domain.iter().map(lookup_type).collect::<Result<Vec<_>>>()?;
                let inverse = input
                    .inverse
                    .as_ref()
                    .map(|k| self.store.predicate(k).map(|x| x.id).ok_or_else(|| Error::not_found(format!("predicate `{k}`"))))
                    .transpose()?;
                let status = if p.curator && !p.actor.is_ai() { VocabStatus::Accepted } else { VocabStatus::Proposed };
                let role = input.role.unwrap_or(PredicateRole::General);
                if role == PredicateRole::TemporalRelation {
                    return Err(Error::invalid("temporal relation predicates are fixed to the Allen algebra vocabulary"));
                }
                let def = PredicateDef {
                    id: chronotope_core::vocab::predicate_id(&input.key),
                    key: input.key.clone(),
                    labels: input.labels.clone(),
                    domain,
                    range: input.range.clone().unwrap_or(PredicateRange::Any),
                    inverse,
                    transitive: input.transitive,
                    symmetric: input.symmetric,
                    functional: input.functional,
                    status,
                    role,
                    allen: None,
                    mappings: input.mappings.clone(),
                };
                let id = def.id;
                batch.cmds.push(Command::DefinePredicate { def });
                self.commit_batch(p, BranchId::main(), "propose_predicate", rev, batch, json!({ "id": id, "key": input.key, "status": status }))
            }
            WriteRequest::AcceptPredicate { key } => {
                self.require_curator(p, "accept_predicate")?;
                let id = self.store.predicate(&key).map(|x| x.id).ok_or_else(|| Error::not_found(format!("predicate `{key}`")))?;
                batch.cmds.push(Command::SetPredicateStatus { id, status: VocabStatus::Accepted });
                self.commit_batch(p, BranchId::main(), "accept_predicate", rev, batch, json!({ "id": id, "status": "accepted" }))
            }
            WriteRequest::ProposeType(input) => {
                if self.store.type_by_key.contains_key(&input.key) {
                    return Err(Error::Conflict(format!("type `{}` already exists", input.key)));
                }
                let parents =
                    input.parents.iter().map(|k| self.store.type_id(k).ok_or_else(|| Error::not_found(format!("type `{k}`")))).collect::<Result<Vec<_>>>()?;
                let status = if p.curator && !p.actor.is_ai() { VocabStatus::Accepted } else { VocabStatus::Proposed };
                let def = TypeDef {
                    id: chronotope_core::vocab::type_id(&input.key),
                    key: input.key.clone(),
                    labels: input.labels.clone(),
                    parents,
                    status,
                    mappings: input.mappings.clone(),
                };
                let id = def.id;
                batch.cmds.push(Command::DefineType { def });
                self.commit_batch(p, BranchId::main(), "propose_type", rev, batch, json!({ "id": id, "status": status }))
            }
            WriteRequest::DefineCalendar { frame } => {
                self.require_curator(p, "define_calendar")?;
                if CalendarFrame::builtin(&frame.key).is_some() {
                    return Err(Error::Conflict(format!("`{}` is a built-in calendar", frame.key)));
                }
                if let chronotope_core::time::calendar::CalendarKind::Uniform { ticks_per_day, days_per_month, months_per_year, .. } = &frame.kind {
                    if *ticks_per_day <= 0 || *days_per_month == 0 || *months_per_year == 0 {
                        return Err(Error::invalid("uniform calendar units must be positive"));
                    }
                }
                let key = frame.key.clone();
                batch.cmds.push(Command::DefineCalendar { frame });
                self.commit_batch(p, BranchId::main(), "define_calendar", rev, batch, json!({ "key": key }))
            }
            WriteRequest::DefineFrame { frame } => {
                self.require_curator(p, "define_frame")?;
                self.store.frames.validate(&frame)?;
                let id = frame.id;
                batch.cmds.push(Command::DefineFrame { frame });
                self.commit_batch(p, BranchId::main(), "define_frame", rev, batch, json!({ "id": id }))
            }
            WriteRequest::DefineLicense { license } => {
                self.require_curator(p, "define_license")?;
                let key = license.key.clone();
                batch.cmds.push(Command::DefineLicense { license });
                self.commit_batch(p, BranchId::main(), "define_license", rev, batch, json!({ "key": key }))
            }
            WriteRequest::SetRankPolicy { policy } => {
                self.require_curator(p, "set_rank_policy")?;
                let (id, version) = (policy.id.clone(), policy.version);
                batch.cmds.push(Command::SetRankPolicy { policy });
                self.commit_batch(p, BranchId::main(), "set_rank_policy", rev, batch, json!({ "rank_policy_id": id, "rank_policy_version": version }))
            }
            WriteRequest::CreateBranch { name, parent, description } => {
                if self.store.branch_by_name.contains_key(&name) {
                    return Err(Error::Conflict(format!("branch `{name}` already exists")));
                }
                let parent = self.branch_or_main(&parent)?;
                let id = BranchId::new();
                batch.cmds.push(Command::CreateBranch {
                    branch: Branch {
                        id,
                        name: name.clone(),
                        parent: Some(parent),
                        fork_seq: self.store.head_seq,
                        kind: BranchKind::Data,
                        created_at: self.now(),
                        description,
                    },
                });
                self.commit_batch(p, parent, "create_branch", rev, batch, json!({ "id": id, "name": name, "fork_seq": self.store.head_seq }))
            }
            WriteRequest::MaterializeBranch { branch } => {
                let b = self.branch_or_main(&Some(branch))?;
                let n = self.materialize_branch(b)?;
                Ok(json!({ "branch": b, "processed": n }))
            }
            WriteRequest::DefineSequence { scope, kind, items, branch, canon, label } => {
                let scope = self.resolve_ref_str(&scope)?;
                let items = items.iter().map(|i| self.resolve_ref_str(i)).collect::<Result<Vec<_>>>()?;
                let canon = canon.map(|c| self.resolve_ref_str(&c)).transpose()?;
                let branch = self.branch_or_main(&branch)?;
                let id = SequenceId::new();
                batch.cmds.push(Command::DefineSequence { sequence: Sequence { id, scope, kind: SequenceKind::parse(&kind), items, branch, canon, label } });
                self.commit_batch(p, branch, "define_sequence", rev, batch, json!({ "id": id }))
            }
            WriteRequest::DefineTable { dataset, name, columns, primary_key, foreign_keys, storage } => {
                let dataset = self.resolve_ref(p, &dataset, rev, &mut batch)?;
                for k in &primary_key {
                    if !columns.iter().any(|c| &c.name == k) {
                        return Err(Error::invalid(format!("primary key column `{k}` is not defined")));
                    }
                }
                let id = TableId::new();
                batch.cmds.push(Command::DefineTable { table: TableDef { id, dataset, name, columns, primary_key, foreign_keys, storage } });
                self.commit_batch(p, BranchId::main(), "define_table", rev, batch, json!({ "id": id, "dataset": dataset }))
            }
            WriteRequest::AddTableRows { table, rows } => {
                let def = self.store.tables.get(&table).ok_or_else(|| Error::not_found(format!("table {table}")))?;
                if def.storage != TableStorage::Inline {
                    return Err(Error::invalid("rows can only be added to inline tables; external tables are read from their storage"));
                }
                for r in &rows {
                    for k in &def.primary_key {
                        if !r.contains_key(k) {
                            return Err(Error::invalid(format!("row is missing primary key column `{k}`")));
                        }
                    }
                }
                let n = rows.len();
                batch.cmds.push(Command::AddTableRows { table, rows });
                self.commit_batch(p, BranchId::main(), "add_table_rows", rev, batch, json!({ "table": table, "rows": n }))
            }
            WriteRequest::LinkRow { table, row_key, resource } => {
                if !self.store.tables.contains_key(&table) {
                    return Err(Error::not_found(format!("table {table}")));
                }
                let resource = self.resolve_ref_str(&resource)?;
                batch.cmds.push(Command::LinkRow { link: RowLink { table, row_key, resource } });
                self.commit_batch(p, BranchId::main(), "link_row", rev, batch, json!({ "resource": resource }))
            }
            WriteRequest::SetEmbedding { resource, model, version, vector } => {
                let resource = self.resolve_ref_str(&resource)?;
                let space = EmbeddingSpace::new(&model, &version, vector.len());
                if let Some(existing) = self.vectors.get(&space.key) {
                    if existing.space.dimension != vector.len() {
                        return Err(Error::invalid(format!("space {} has dimension {}", space.key, existing.space.dimension)));
                    }
                } else {
                    batch.cmds.push(Command::DefineEmbeddingSpace { space: space.clone() });
                }
                batch.cmds.push(Command::SetEmbedding { resource, space: space.key.clone(), vector, generated_at: self.now() });
                self.commit_batch(p, BranchId::main(), "set_embedding", rev, batch, json!({ "resource": resource, "space": space.key }))
            }
            WriteRequest::CreateKey {} => {
                let key_id = KeyId::new();
                batch.cmds.push(Command::CreateKey { key_id });
                self.commit_batch(p, BranchId::main(), "create_key", rev, batch, json!({ "key_id": key_id }))
            }
            WriteRequest::ShredKey { key_id } => {
                self.require_curator(p, "shred_key")?;
                if !self.store.keys.contains(&key_id) {
                    return Err(Error::not_found(format!("key {key_id}")));
                }
                batch.cmds.push(Command::ShredKey { key_id });
                self.commit_batch(p, BranchId::main(), "shred_key", rev, batch, json!({ "key_id": key_id, "shredded": true }))
            }
        }
    }

    fn link_source(&mut self, p: &Principal, input: LinkSource, rev: RevisionId, mut batch: Batch) -> Result<Json> {
        let acquired_at = parse_time(&input.acquisition.acquired_at, "acquisition.acquired_at")?;
        let locator = input.locator.clone().or(input.url.clone().map(|url| Locator::Url { url }));
        let (source_id, new_source) = match (input.source, &locator) {
            (Some(s), _) => {
                if !self.store.sources.contains_key(&s) {
                    return Err(Error::not_found(format!("source {s}")));
                }
                (s, false)
            }
            (None, Some(loc)) => match self.store.source_by_locator.get(&loc.key()) {
                Some(s) => (*s, false),
                None => (SourceId::new(), true),
            },
            (None, None) => return Err(Error::invalid("either `source`, `url` or `locator` is required")),
        };
        if let Some(root) = input.provenance_root {
            if !self.store.sources.contains_key(&root) {
                return Err(Error::not_found(format!("provenance_root source {root}")));
            }
        }
        if let Some(l) = &input.license {
            if !self.store.licenses.contains_key(l) {
                return Err(Error::not_found(format!("license `{l}`")));
            }
        }
        if new_source {
            let resource = input.resource.as_ref().map(|r| self.resolve_ref(p, r, rev, &mut batch)).transpose()?;
            let source_time = input.source_time.as_ref().map(|t| self.time_expr(t, &input.calendar, &mut batch)).transpose()?;
            batch.cmds.push(Command::RegisterSource {
                source: Source {
                    id: source_id,
                    kind: input.kind.unwrap_or(SourceKind::Other),
                    locator: locator.clone().expect("new sources have a locator"),
                    title: input.title.clone(),
                    resource,
                    publisher: None,
                    source_time,
                    origin: input.origin.unwrap_or_default(),
                    reliability: input.reliability,
                    provenance_root: input.provenance_root,
                    license: input.license.clone(),
                    visibility: Visibility::Public,
                    registered_at: self.now(),
                },
            });
        }
        let acq = &input.acquisition;
        let bytes = match (&acq.content, &acq.content_base64) {
            (Some(t), _) => Some(t.as_bytes().to_vec()),
            (None, Some(b)) => Some(base64::engine::general_purpose::STANDARD.decode(b).map_err(|e| Error::invalid(format!("content_base64: {e}")))?),
            _ => None,
        };
        let mut content_hash_v = None;
        let mut snapshot_ref = None;
        let mut deduplicated = false;
        if let Some(bytes) = bytes {
            // content_hash は常に平文のハッシュ。暗号化時は保存物（暗号文）のハッシュを snapshot_ref に持つ。
            let plain_hash = content_hash(&bytes);
            let (stored, encrypted_with) = match acq.encrypt_with {
                Some(k) => {
                    let (nonce, ct) = self.vault.encrypt_bytes(k, &bytes)?;
                    let mut v = hex::decode(&nonce).map_err(|e| Error::Storage(e.to_string()))?;
                    v.extend(ct);
                    (v, Some(k))
                }
                None => (bytes.clone(), None),
            };
            let (h, fresh) = self.objects.put(&stored)?;
            deduplicated = !fresh;
            snapshot_ref = Some(ObjectRef { hash: h, size: stored.len() as u64, media_type: acq.media_type.clone(), encrypted_with });
            content_hash_v = Some(if encrypted_with.is_some() { ContentHash("withheld:encrypted".into()) } else { plain_hash });
        }
        let acquisition_id = AcquisitionId::new();
        batch.cmds.push(Command::RegisterAcquisition {
            acquisition: Acquisition {
                id: acquisition_id,
                source: source_id,
                acquired_at,
                acquired_by: ActorRef {
                    id: acq.acquired_by.clone().unwrap_or_else(|| p.actor.id.clone()),
                    kind: if acq.acquired_by.is_some() { ActorKind::Crawler } else { p.actor.kind },
                },
                method: acq.method.unwrap_or(AcquisitionMethod::AgentBrowse),
                locator: acq
                    .locator
                    .clone()
                    .or(locator)
                    .or_else(|| self.store.sources.get(&source_id).map(|s| s.locator.clone()))
                    .ok_or_else(|| Error::invalid("acquisition locator is required"))?,
                content_hash: content_hash_v.clone(),
                snapshot_ref,
                recorded_at: self.now(),
            },
        });
        self.commit_batch(p, BranchId::main(), "link_source", rev, batch, json!({ "source": source_id, "new_source": new_source, "acquisition": acquisition_id, "content_hash": content_hash_v, "snapshot_deduplicated": deduplicated }))
    }
}
