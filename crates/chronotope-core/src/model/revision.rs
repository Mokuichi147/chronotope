//! Revision（KB 自体の変更履歴、Immutable）と Branch（データ・設定の派生、copy-on-write）。
//! Canon（正史・公式設定系統）と Timeline（世界内部の時間分岐）は Resource として表し、
//! Assertion の `canon` / `timeline` で参照する。

use crate::BranchId;
use crate::time::Tick;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BranchKind {
    #[default]
    Data,
    Config,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Branch {
    pub id: BranchId,
    pub name: String,
    /// 親ブランチ（copy-on-write の参照先）。
    #[serde(default)]
    pub parent: Option<BranchId>,
    /// 分岐時点の Revision 通番。親ブランチのこれ以前の変更だけが見える。
    pub fork_seq: u64,
    #[serde(default)]
    pub kind: BranchKind,
    pub created_at: Tick,
    #[serde(default)]
    pub description: Option<String>,
}

impl Branch {
    pub fn main() -> Self {
        Branch { id: BranchId::main(), name: "main".into(), parent: None, fork_seq: 0, kind: BranchKind::Data, created_at: Tick(0), description: None }
    }
}
