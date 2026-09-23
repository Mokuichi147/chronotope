//! Identity Resolution。誤マージの影響が大きいため、
//! possibly_same_as → merge candidate → 確認 → identity_redirect の順に進める。

use super::security::ActorRef;
use crate::time::Tick;
use crate::{AssertionId, MergeProposalId, ResourceId, RevisionId};
use serde::{Deserialize, Serialize};

/// 統合後も古い ID を有効にするためのリダイレクト。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdentityRedirect {
    pub from: ResourceId,
    pub to: ResourceId,
    pub revision: RevisionId,
    pub approved_by: ActorRef,
    pub at: Tick,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStatus {
    Proposed,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergeProposal {
    pub id: MergeProposalId,
    /// 統合される側（リダイレクト元）。
    pub from: ResourceId,
    /// 統合先。
    pub into: ResourceId,
    pub proposed_by: ActorRef,
    pub proposed_at: Tick,
    #[serde(default)]
    pub reason: Option<String>,
    /// 根拠となる possibly_same_as などの Assertion。
    #[serde(default)]
    pub evidence: Vec<AssertionId>,
    pub status: MergeStatus,
    #[serde(default)]
    pub decided_by: Option<ActorRef>,
    #[serde(default)]
    pub decided_at: Option<Tick>,
}
