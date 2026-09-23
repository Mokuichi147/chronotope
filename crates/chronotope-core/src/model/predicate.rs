//! 語彙（Predicate / Type）。Predicate 自体も Resource として管理し、外部標準との Mapping を持つ。

use super::resource::Label;
use crate::ResourceId;
use serde::{Deserialize, Serialize};

/// 語彙の状態。AI エージェントは `Proposed` までしか作れない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VocabStatus {
    Proposed,
    Accepted,
    Deprecated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    Exact,
    Close,
    Broad,
    Narrow,
    Related,
}

/// 外部語彙との対応（OWL-Time / PROV-O / CIDOC CRM / Wikidata / UCUM / schema.org ...）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VocabMapping {
    pub vocabulary: String,
    pub iri: String,
    pub match_kind: MatchKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiteralKind {
    Text,
    Quantity,
    Bool,
    Time,
    Geo,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PredicateRange {
    /// Resource を取る（`types` が空なら任意の型）。
    Resource {
        #[serde(default)]
        types: Vec<ResourceId>,
    },
    Literal {
        literal: LiteralKind,
    },
    Any,
}

/// 述語の役割。Projection / Resolver がどの特徴量へ展開するかを決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredicateRole {
    General,
    /// 出来事の時間（event_time）: occurred_at / start_time / end_time。
    EventTime,
    /// Event 間の時間関係（Allen）。
    TemporalRelation,
    /// 出来事・物の所在（space_ids へ）。
    Location,
    /// 場所の包含階層（space_ancestor_ids へ）。
    SpatialContainment,
    /// その他の空間関係（隣接・接続・ポータル等）。
    SpatialRelation,
    /// 作品階層（work_ids へ）。
    WorkMembership,
    /// 作品間の翻案・派生。
    Adaptation,
    /// 同一性（same_as / possibly_same_as / distinct_from）。
    Identity,
    /// 型付け（instance_of）。
    Typing,
    /// 座標。
    Coordinates,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredicateDef {
    pub id: ResourceId,
    /// 機械可読キー（`participated_in`）。
    pub key: String,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub domain: Vec<ResourceId>,
    pub range: PredicateRange,
    #[serde(default)]
    pub inverse: Option<ResourceId>,
    #[serde(default)]
    pub transitive: bool,
    #[serde(default)]
    pub symmetric: bool,
    /// 主語ごとに値が 1 つであるべき（複数あれば contested）。
    #[serde(default)]
    pub functional: bool,
    pub status: VocabStatus,
    pub role: PredicateRole,
    /// 時間関係述語の場合の Allen 関係名（`before` など）。
    #[serde(default)]
    pub allen: Option<String>,
    #[serde(default)]
    pub mappings: Vec<VocabMapping>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypeDef {
    pub id: ResourceId,
    pub key: String,
    #[serde(default)]
    pub labels: Vec<Label>,
    /// 上位型（subclass_of）。
    #[serde(default)]
    pub parents: Vec<ResourceId>,
    pub status: VocabStatus,
    #[serde(default)]
    pub mappings: Vec<VocabMapping>,
}
