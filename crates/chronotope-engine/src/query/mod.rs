//! Agent 向け読み取り API（JSON Query DSL）。DB 構造は直接公開しない。
//!
//! - `budget_ms` は必須。超過したら途中結果を `truncated: true` で返す。
//! - 3 段階ドリルダウン: `search`/`lookup`（Level 1 要約）→ `expand_claims`（Level 2 主張・異説・関係）
//!   → `get_acquisition`（Level 3 出典・取得・抽出）。
//! - 一意に決まらない結果は確定させない（`ambiguous`, `contested`, `comparable: false`）。

mod ops;
mod render;
mod search;

use crate::facts::Facts;
use crate::kb::{Freshness, KnowledgeBase};
use crate::view::{StatusSet, View};
use chronotope_core::model::{AssertionStatus, Principal};
use chronotope_core::space::Placement;
use chronotope_core::time::Tick;
use chronotope_core::*;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize)]
pub struct QueryRequest {
    /// 処理時間の上限（必須）。
    pub budget_ms: u64,
    #[serde(flatten)]
    pub op: QueryOp,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub canon: Option<String>,
    #[serde(default)]
    pub timeline: Option<String>,
    /// この時点で外部エージェントが知り得た情報（acquired_at ≤ t）だけを使う。
    #[serde(default)]
    pub as_known_at: Option<String>,
    #[serde(default)]
    pub status: Option<Vec<AssertionStatus>>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimeMode {
    /// 可能区間が重なる。
    #[default]
    Possibly,
    /// どの解釈でも重なる。
    Certainly,
    /// どの解釈でも窓に収まる。
    Within,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TimeFilter {
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    /// 自然言語・ISO の時間表現（`2026年9月`, `先週` など。相対表現は現在時刻基準）。
    #[serde(default)]
    pub expression: Option<String>,
    #[serde(default)]
    pub calendar: Option<String>,
    #[serde(default)]
    pub mode: TimeMode,
    #[serde(default)]
    pub include_undated: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Near {
    pub at: Placement,
    /// WGS84 はメートル、その他は基準 Frame の単位。
    pub radius: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SpaceFilter {
    #[serde(default)]
    pub within_place: Option<String>,
    #[serde(default)]
    pub bbox: Option<Placement>,
    #[serde(default)]
    pub near: Option<Near>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VectorQuery {
    #[serde(default)]
    pub space: Option<String>,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrderBy {
    #[default]
    Relevance,
    Rank,
    Time,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchSpec {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub time: Option<TimeFilter>,
    #[serde(default)]
    pub space: Option<SpaceFilter>,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub works: Vec<String>,
    #[serde(default)]
    pub min_rank: Option<f64>,
    #[serde(default)]
    pub contested_only: bool,
    #[serde(default)]
    pub vector: Option<VectorQuery>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub order_by: OrderBy,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SimilarityWeights {
    #[serde(default = "w_semantic")]
    pub semantic: f64,
    #[serde(default = "w_small")]
    pub temporal: f64,
    #[serde(default = "w_small")]
    pub spatial: f64,
    #[serde(default = "w_small")]
    pub entity: f64,
    #[serde(default = "w_small")]
    pub work: f64,
    #[serde(default = "w_small")]
    pub graph: f64,
    #[serde(default = "w_rank")]
    pub rank: f64,
}

fn w_semantic() -> f64 {
    0.4
}
fn w_small() -> f64 {
    0.1
}
fn w_rank() -> f64 {
    0.1
}

impl Default for SimilarityWeights {
    fn default() -> Self {
        SimilarityWeights {
            semantic: w_semantic(),
            temporal: w_small(),
            spatial: w_small(),
            entity: w_small(),
            work: w_small(),
            graph: w_small(),
            rank: w_rank(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Out,
    In,
    #[default]
    Both,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum QueryOp {
    /// Tier 0: ID・外部 ID・ラベル完全一致での参照。
    Lookup {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        external_id: Option<String>,
        #[serde(default)]
        label: Option<String>,
    },
    /// Tier 1: 構造・時空間・テキスト・ベクトル検索（Level 1 要約を返す）。
    Search(SearchSpec),
    /// 多特徴（意味・時間・空間・関係・作品・グラフ距離）の類似検索。
    Similar {
        id: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        weights: Option<SimilarityWeights>,
        #[serde(default)]
        space: Option<String>,
    },
    /// Level 2: 主張・異説・関係。
    ExpandClaims {
        id: String,
        #[serde(default)]
        predicates: Vec<String>,
        #[serde(default)]
        include_history: bool,
        #[serde(default)]
        include_incoming: bool,
        #[serde(default)]
        valid_at: Option<String>,
    },
    /// Level 3: 出典・取得・抽出。
    GetAcquisition {
        #[serde(default)]
        assertion: Option<AssertionId>,
        #[serde(default)]
        acquisition: Option<AcquisitionId>,
        #[serde(default)]
        include_snapshot: bool,
    },
    TemporalRelation {
        a: String,
        b: String,
    },
    Timeline {
        #[serde(default)]
        entity: Option<String>,
        #[serde(default)]
        work: Option<String>,
        #[serde(default)]
        place: Option<String>,
        #[serde(default)]
        types: Vec<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Neighbors {
        id: String,
        #[serde(default)]
        predicates: Vec<String>,
        #[serde(default)]
        direction: Direction,
        #[serde(default)]
        depth: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Tier 2: 異説・矛盾の一覧。
    Conflicts {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Freshness {},
    Observations {
        target: String,
        #[serde(default)]
        metric: Option<String>,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        to: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    PositionAt {
        target: String,
        at: String,
    },
    Sequence {
        scope: String,
        #[serde(default)]
        kind: Option<String>,
    },
    ResolveTemporal {
        text: String,
        #[serde(default)]
        calendar: Option<String>,
        #[serde(default)]
        reference: Option<String>,
    },
    ResolveSpatial {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        point: Option<Placement>,
        #[serde(default)]
        limit: Option<usize>,
    },
    MergeCandidates {
        #[serde(default)]
        limit: Option<usize>,
    },
    DerivedBy {
        #[serde(default)]
        extractor: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        model_version: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Vocabulary {
        #[serde(default)]
        kind: Option<String>,
    },
    TableRows {
        table: TableId,
        #[serde(default)]
        limit: Option<usize>,
    },
    Revisions {
        #[serde(default)]
        limit: Option<usize>,
    },
}

impl QueryOp {
    pub fn tier(&self) -> &'static str {
        match self {
            QueryOp::Lookup { .. } | QueryOp::Freshness {} | QueryOp::Vocabulary { .. } => "tier0",
            QueryOp::Conflicts { .. } | QueryOp::MergeCandidates { .. } | QueryOp::DerivedBy { .. } => "tier2",
            _ => "tier1",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResponse {
    pub results: Json,
    pub truncated: bool,
    pub tier: &'static str,
    pub elapsed_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<Freshness>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

pub struct Deadline {
    start: Instant,
    budget: Duration,
}

impl Deadline {
    pub fn new(ms: u64) -> Self {
        Deadline { start: Instant::now(), budget: Duration::from_millis(ms) }
    }
    pub fn expired(&self) -> bool {
        self.start.elapsed() >= self.budget
    }
    pub fn elapsed_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }
}

/// 1 回のクエリ実行の文脈。
pub(crate) struct QCtx<'a> {
    pub kb: &'a KnowledgeBase,
    pub principal: &'a Principal,
    pub view: View,
    pub deadline: Deadline,
    pub lang: Option<String>,
    pub warnings: RefCell<Vec<String>>,
    pub truncated: Cell<bool>,
    pub now: Tick,
}

impl QCtx<'_> {
    pub fn warn(&self, w: impl Into<String>) {
        let w = w.into();
        let mut ws = self.warnings.borrow_mut();
        if !ws.contains(&w) {
            ws.push(w);
        }
    }

    /// 予算超過なら truncated を立てて true。
    pub fn out_of_budget(&self) -> bool {
        if self.deadline.expired() {
            self.truncated.set(true);
            true
        } else {
            false
        }
    }

    pub fn lang(&self) -> Option<&str> {
        self.lang.as_deref().or(self.kb.config.default_lang.as_deref())
    }

    pub fn resolve(&self, s: &str) -> Result<ResourceId> {
        self.kb.resolve_ref_str(s)
    }

    pub fn facts(&self) -> Facts<'_> {
        Facts::new(&self.kb.store, &self.view, self.now, HashMap::new())
    }
}

impl KnowledgeBase {
    /// JSON 文字列のクエリを実行する。
    pub fn query_json(&self, principal: &Principal, body: &str) -> Result<QueryResponse> {
        let req: QueryRequest = serde_json::from_str(body).map_err(|e| Error::invalid(format!("query: {e}")))?;
        self.query(principal, &req)
    }

    pub fn query(&self, principal: &Principal, req: &QueryRequest) -> Result<QueryResponse> {
        if req.budget_ms == 0 {
            return Err(Error::invalid("budget_ms must be positive"));
        }
        let deadline = Deadline::new(req.budget_ms);
        let branch = match &req.branch {
            Some(b) => self.store.branch_id(b).ok_or_else(|| Error::not_found(format!("branch `{b}`")))?,
            None => BranchId::main(),
        };
        let mut view = View { principal: principal.clone(), ..View::projection(&self.store, branch)? };
        if let Some(c) = &req.canon {
            view.canon = Some(self.resolve_ref_str(c)?);
        }
        if let Some(t) = &req.timeline {
            view.timelines = Some(self.store.timeline_lineage(self.resolve_ref_str(t)?));
        }
        if let Some(t) = &req.as_known_at {
            view.as_known_at = Some(Tick::parse_iso(t)?);
        }
        if let Some(s) = &req.status {
            view.statuses = StatusSet::of(s);
        }
        let ctx =
            QCtx { kb: self, principal, view, deadline, lang: req.lang.clone(), warnings: RefCell::new(vec![]), truncated: Cell::new(false), now: self.now() };
        let results = match &req.op {
            QueryOp::Lookup { id, external_id, label } => ops::lookup(&ctx, branch, id.as_deref(), external_id.as_deref(), label.as_deref())?,
            QueryOp::Search(spec) => search::search(&ctx, branch, spec)?,
            QueryOp::Similar { id, limit, weights, space } => {
                search::similar(&ctx, branch, id, limit.unwrap_or(10), weights.clone().unwrap_or_default(), space.as_deref())?
            }
            QueryOp::ExpandClaims { id, predicates, include_history, include_incoming, valid_at } => {
                ops::expand_claims(&ctx, branch, id, predicates, *include_history, *include_incoming, valid_at.as_deref())?
            }
            QueryOp::GetAcquisition { assertion, acquisition, include_snapshot } => ops::get_acquisition(&ctx, *assertion, *acquisition, *include_snapshot)?,
            QueryOp::TemporalRelation { a, b } => ops::temporal_relation(&ctx, branch, a, b)?,
            QueryOp::Timeline { entity, work, place, types, limit } => {
                ops::timeline(&ctx, branch, entity.as_deref(), work.as_deref(), place.as_deref(), types, limit.unwrap_or(50))?
            }
            QueryOp::Neighbors { id, predicates, direction, depth, limit } => {
                ops::neighbors(&ctx, branch, id, predicates, *direction, depth.unwrap_or(1).min(3), limit.unwrap_or(50))?
            }
            QueryOp::Conflicts { id, limit } => ops::conflicts(&ctx, branch, id.as_deref(), limit.unwrap_or(50))?,
            QueryOp::Freshness {} => serde_json::to_value(self.freshness(branch)?).unwrap_or_default(),
            QueryOp::Observations { target, metric, from, to, limit } => {
                ops::observations(&ctx, target, metric.as_deref(), from.as_deref(), to.as_deref(), limit.unwrap_or(1000))?
            }
            QueryOp::PositionAt { target, at } => ops::position_at(&ctx, target, at)?,
            QueryOp::Sequence { scope, kind } => ops::sequence(&ctx, branch, scope, kind.as_deref())?,
            QueryOp::ResolveTemporal { text, calendar, reference } => ops::resolve_temporal(&ctx, text, calendar.as_deref(), reference.as_deref())?,
            QueryOp::ResolveSpatial { name, point, limit } => ops::resolve_spatial(&ctx, branch, name.as_deref(), point.as_ref(), limit.unwrap_or(10))?,
            QueryOp::MergeCandidates { limit } => ops::merge_candidates(&ctx, limit.unwrap_or(50))?,
            QueryOp::DerivedBy { extractor, model, model_version, limit } => {
                ops::derived_by(&ctx, extractor.as_deref(), model.as_deref(), model_version.as_deref(), limit.unwrap_or(100))?
            }
            QueryOp::Vocabulary { kind } => ops::vocabulary(&ctx, kind.as_deref()),
            QueryOp::TableRows { table, limit } => ops::table_rows(&ctx, table, limit.unwrap_or(100))?,
            QueryOp::Revisions { limit } => {
                serde_json::to_value(self.store.revisions.iter().rev().take(limit.unwrap_or(20)).collect::<Vec<_>>()).unwrap_or_default()
            }
        };
        let truncated = ctx.truncated.get();
        let warnings = ctx.warnings.into_inner();
        Ok(QueryResponse { results, truncated, tier: req.op.tier(), elapsed_ms: ctx.deadline.elapsed_ms(), freshness: self.freshness(branch).ok(), warnings })
    }
}
