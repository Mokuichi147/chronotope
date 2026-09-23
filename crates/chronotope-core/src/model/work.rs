//! Work Graph の順序。作品階層は Assertion（part_of_work / adaptation_of）で表し、
//! 順序（放送順・作中時系列順・推奨順など）は独立した Sequence として持つ。

use crate::{BranchId, ResourceId, SequenceId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SequenceKind {
    ReleaseOrder,
    WorkOrder,
    StoryOrder,
    RecommendedOrder,
    Custom(String),
}

impl SequenceKind {
    pub fn parse(s: &str) -> SequenceKind {
        match s {
            "release_order" => SequenceKind::ReleaseOrder,
            "work_order" => SequenceKind::WorkOrder,
            "story_order" => SequenceKind::StoryOrder,
            "recommended_order" => SequenceKind::RecommendedOrder,
            other => SequenceKind::Custom(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    pub id: SequenceId,
    /// 対象範囲（Series / Franchise など）。
    pub scope: ResourceId,
    pub kind: SequenceKind,
    pub items: Vec<ResourceId>,
    pub branch: BranchId,
    #[serde(default)]
    pub canon: Option<ResourceId>,
    #[serde(default)]
    pub label: Option<String>,
}
