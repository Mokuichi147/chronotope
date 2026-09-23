//! Assertion — Resource についての意味的な主張。真実とはみなさず、異説・訂正・否定を並存させる。

use super::security::{ActorRef, ProtectedValue, Visibility};
use crate::space::Placement;
use crate::time::Tick;
use crate::time::expr::TemporalExpression;
use crate::{AcquisitionId, AssertionId, BranchId, DerivationId, ResourceId, RevisionId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Value {
    Resource(ResourceId),
    Text {
        text: String,
        #[serde(default)]
        lang: Option<String>,
    },
    /// 数量。単位は UCUM（`m`, `kg`, `Cel`, `{likes}`）。
    Quantity {
        amount: f64,
        #[serde(default)]
        unit: Option<String>,
    },
    Bool(bool),
    Time(TemporalExpression),
    Geo(Placement),
    Json(serde_json::Value),
    /// crypto-shredding 可能な暗号化値。
    Protected(ProtectedValue),
    /// 値が不明であることが分かっている。
    Unknown,
}

impl Value {
    pub fn as_resource(&self) -> Option<ResourceId> {
        match self {
            Value::Resource(r) => Some(*r),
            _ => None,
        }
    }

    pub fn as_time(&self) -> Option<&TemporalExpression> {
        match self {
            Value::Time(t) => Some(t),
            _ => None,
        }
    }

    /// 異説判定用の同値キー（同じ主張か否か）。
    pub fn identity_key(&self) -> String {
        match self {
            Value::Resource(r) => format!("r:{r}"),
            Value::Text { text, .. } => format!("t:{}", text.trim().to_lowercase()),
            Value::Quantity { amount, unit } => format!("q:{amount}:{}", unit.as_deref().unwrap_or("")),
            Value::Bool(b) => format!("b:{b}"),
            Value::Time(t) => format!("time:{}", serde_json::to_string(&t.ast).unwrap_or_default()),
            Value::Geo(p) => format!("g:{}", serde_json::to_string(p).unwrap_or_default()),
            Value::Json(j) => format!("j:{j}"),
            Value::Protected(p) => format!("p:{}", p.ciphertext),
            Value::Unknown => "unknown".into(),
        }
    }
}

/// 状態。削除ではなく状態変化として履歴に残す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionStatus {
    Accepted,
    Proposed,
    Disputed,
    Retracted,
    Superseded,
}

impl AssertionStatus {
    /// 通常検索で「生きている」主張か。
    pub fn is_live(self) -> bool {
        matches!(self, AssertionStatus::Accepted | AssertionStatus::Proposed | AssertionStatus::Disputed)
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "accepted" => AssertionStatus::Accepted,
            "proposed" => AssertionStatus::Proposed,
            "disputed" => AssertionStatus::Disputed,
            "retracted" => AssertionStatus::Retracted,
            "superseded" => AssertionStatus::Superseded,
            _ => return None,
        })
    }
}

/// 肯定・否定は confidence とは別軸。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Polarity {
    #[default]
    Affirmed,
    Negated,
}

/// 信頼度の構成要素。単一スカラーにはまとめない。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ConfidenceComponents {
    /// 情報源の信頼性（0..1）。
    #[serde(default)]
    pub source_reliability: Option<f32>,
    /// 抽出の確からしさ（0..1）。
    #[serde(default)]
    pub extraction_conf: Option<f32>,
    /// 独立した裏付けの強さ（0..1、Materializer が算出）。
    #[serde(default)]
    pub corroboration: f32,
    /// 具体性（0..1、時間の粒度などから算出）。
    #[serde(default)]
    pub specificity: Option<f32>,
    #[serde(default)]
    pub human_verified: bool,
    /// 独立した provenance_root の数（転載は同一 root として 1 つに数える）。
    #[serde(default)]
    pub independent_sources: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RankTier {
    AiOnly = 0,
    Secondary = 1,
    Primary = 2,
    Corroborated = 3,
    HumanVerified = 4,
}

/// Materialize されたランク。どの方針のどの版でいつ計算したかを保持する。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputedRank {
    pub value: f64,
    pub tier: RankTier,
    pub rank_policy_id: String,
    pub rank_policy_version: u32,
    pub rank_computed_at: Tick,
}

/// 根拠。Acquisition（取得行為）と、AI 等による抽出（Derivation）への参照。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Evidence {
    pub acquisition: AcquisitionId,
    #[serde(default)]
    pub derivation: Option<DerivationId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusChange {
    pub from: AssertionStatus,
    pub to: AssertionStatus,
    pub revision: RevisionId,
    pub by: ActorRef,
    /// KB に記録された時刻。
    pub recorded_at: Tick,
    /// 状態変化の根拠となった情報の取得時刻（過去時点検索で使う）。
    #[serde(default)]
    pub basis_acquired_at: Option<Tick>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assertion {
    pub id: AssertionId,
    pub subject: ResourceId,
    pub predicate: ResourceId,
    pub object: Value,
    #[serde(default)]
    pub polarity: Polarity,
    /// valid_time: この主張が成立していた時間（temporal_scope）。
    #[serde(default)]
    pub valid_time: Option<TemporalExpression>,
    /// spatial_scope: この主張が成立する場所。
    #[serde(default)]
    pub spatial_scope: Option<ResourceId>,
    pub branch: BranchId,
    #[serde(default)]
    pub timeline: Option<ResourceId>,
    #[serde(default)]
    pub canon: Option<ResourceId>,
    pub status: AssertionStatus,
    #[serde(default)]
    pub confidence: ConfidenceComponents,
    #[serde(default)]
    pub rank: Option<ComputedRank>,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub supersedes: Option<AssertionId>,
    #[serde(default)]
    pub superseded_by: Option<AssertionId>,
    /// copy-on-write: 親ブランチの Assertion をこのブランチで上書きしている場合の元 ID。
    #[serde(default)]
    pub overrides: Option<AssertionId>,
    pub asserted_by: ActorRef,
    pub created_revision: RevisionId,
    pub created_at: Tick,
    /// 根拠のうち最も早い acquired_at（根拠が無ければ created_at）。過去時点検索に使う。
    pub first_known_at: Tick,
    #[serde(default)]
    pub status_history: Vec<StatusChange>,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

impl Assertion {
    /// `t` 時点で知り得た状態（t 以前の根拠で起きた状態変化だけを適用）。
    /// `t` 時点でまだ知られていなければ None。
    pub fn status_as_known_at(&self, t: Tick) -> Option<AssertionStatus> {
        if self.first_known_at > t {
            return None;
        }
        let initial = self.status_history.first().map(|c| c.from).unwrap_or(self.status);
        let mut st = initial;
        for c in &self.status_history {
            let when = c.basis_acquired_at.unwrap_or(c.recorded_at);
            if when <= t {
                st = c.to;
            }
        }
        Some(st)
    }
}
