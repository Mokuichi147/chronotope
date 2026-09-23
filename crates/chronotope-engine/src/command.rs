//! Canonical 層を変更する唯一の経路である Command と、それをまとめた Immutable な Revision。
//!
//! Revision は追記専用ログ（[`crate::store::log`]）へ先に書き込んでから適用する（WAL）。
//! Command は ID・時刻をすべて確定した状態で記録するため、ログを再生すれば同じ状態が再現される。

use crate::vector::EmbeddingSpace;
use chronotope_core::model::*;
use chronotope_core::rank::RankPolicy;
use chronotope_core::space::SpatialReferenceFrame;
use chronotope_core::time::Tick;
use chronotope_core::time::calendar::CalendarFrame;
use chronotope_core::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // ログの直列化形式を単純に保つため Box 化しない
pub enum Command {
    DefineType {
        def: TypeDef,
    },
    DefinePredicate {
        def: PredicateDef,
    },
    SetPredicateStatus {
        id: ResourceId,
        status: VocabStatus,
    },
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
    CreateResource {
        resource: Resource,
    },
    UpdateResource {
        id: ResourceId,
        #[serde(default)]
        add_labels: Vec<Label>,
        #[serde(default)]
        add_descriptions: Vec<LocalizedText>,
        #[serde(default)]
        add_external_ids: Vec<ExternalId>,
        #[serde(default)]
        add_types: Vec<ResourceId>,
        #[serde(default)]
        remove_types: Vec<ResourceId>,
    },
    AddAssertion {
        assertion: Assertion,
    },
    ChangeStatus {
        id: AssertionId,
        change: StatusChange,
    },
    LinkSupersede {
        old: AssertionId,
        new: AssertionId,
    },
    SetHumanVerified {
        id: AssertionId,
        verified: bool,
    },
    AddEvidence {
        id: AssertionId,
        evidence: Evidence,
    },
    RegisterSource {
        source: Source,
    },
    RegisterAcquisition {
        acquisition: Acquisition,
    },
    RegisterDerivation {
        derivation: Derivation,
    },
    ProposeMerge {
        proposal: MergeProposal,
    },
    DecideMerge {
        id: MergeProposalId,
        approve: bool,
        by: ActorRef,
        at: Tick,
        #[serde(default)]
        redirect: Option<IdentityRedirect>,
    },
    CreateBranch {
        branch: Branch,
    },
    AddObservation {
        observation: Observation,
    },
    AddTrajectory {
        trajectory: Trajectory,
    },
    DefineSequence {
        sequence: Sequence,
    },
    DefineTable {
        table: TableDef,
    },
    AddTableRows {
        table: TableId,
        rows: Vec<serde_json::Map<String, serde_json::Value>>,
    },
    LinkRow {
        link: RowLink,
    },
    DefineEmbeddingSpace {
        space: EmbeddingSpace,
    },
    SetEmbedding {
        resource: ResourceId,
        space: String,
        vector: Vec<f32>,
        generated_at: Tick,
    },
    /// 鍵の作成を記録する（鍵素材はログに書かず KeyVault にのみ保存する）。
    CreateKey {
        key_id: KeyId,
    },
    /// 鍵を破棄する。以後その鍵で暗号化された値は復号できない。
    ShredKey {
        key_id: KeyId,
    },
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::DefineType { .. } => "define_type",
            Command::DefinePredicate { .. } => "define_predicate",
            Command::SetPredicateStatus { .. } => "set_predicate_status",
            Command::DefineCalendar { .. } => "define_calendar",
            Command::DefineFrame { .. } => "define_frame",
            Command::DefineLicense { .. } => "define_license",
            Command::SetRankPolicy { .. } => "set_rank_policy",
            Command::CreateResource { .. } => "create_resource",
            Command::UpdateResource { .. } => "update_resource",
            Command::AddAssertion { .. } => "add_assertion",
            Command::ChangeStatus { .. } => "change_status",
            Command::LinkSupersede { .. } => "link_supersede",
            Command::SetHumanVerified { .. } => "set_human_verified",
            Command::AddEvidence { .. } => "add_evidence",
            Command::RegisterSource { .. } => "register_source",
            Command::RegisterAcquisition { .. } => "register_acquisition",
            Command::RegisterDerivation { .. } => "register_derivation",
            Command::ProposeMerge { .. } => "propose_merge",
            Command::DecideMerge { .. } => "decide_merge",
            Command::CreateBranch { .. } => "create_branch",
            Command::AddObservation { .. } => "add_observation",
            Command::AddTrajectory { .. } => "add_trajectory",
            Command::DefineSequence { .. } => "define_sequence",
            Command::DefineTable { .. } => "define_table",
            Command::AddTableRows { .. } => "add_table_rows",
            Command::LinkRow { .. } => "link_row",
            Command::DefineEmbeddingSpace { .. } => "define_embedding_space",
            Command::SetEmbedding { .. } => "set_embedding",
            Command::CreateKey { .. } => "create_key",
            Command::ShredKey { .. } => "shred_key",
        }
    }
}

/// Immutable な変更単位。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Revision {
    pub id: RevisionId,
    /// 全体で単調増加する通番。
    pub seq: u64,
    pub branch: BranchId,
    pub actor: ActorRef,
    #[serde(default)]
    pub message: Option<String>,
    pub committed_at: Tick,
    pub commands: Vec<Command>,
}

/// ログ・API 向けの Revision 要約。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevisionHeader {
    pub id: RevisionId,
    pub seq: u64,
    pub branch: BranchId,
    pub actor: ActorRef,
    #[serde(default)]
    pub message: Option<String>,
    pub committed_at: Tick,
    pub commands: Vec<String>,
}

impl Revision {
    pub fn header(&self) -> RevisionHeader {
        RevisionHeader {
            id: self.id,
            seq: self.seq,
            branch: self.branch,
            actor: self.actor.clone(),
            message: self.message.clone(),
            committed_at: self.committed_at,
            commands: self.commands.iter().map(|c| c.name().to_string()).collect(),
        }
    }
}
