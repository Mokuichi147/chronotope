//! Acquisition / Provenance。情報源（Source）と取得行為（Acquisition）を分離する。

use super::security::{ActorRef, Visibility};
use crate::time::Tick;
use crate::time::expr::TemporalExpression;
use crate::{AcquisitionId, DerivationId, ResourceId, SourceId, TableId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    WebPage,
    Post,
    Pdf,
    Document,
    Book,
    Json,
    ApiResponse,
    Log,
    Image,
    Video,
    Audio,
    Subtitle,
    Table,
    Sensor,
    Human,
    Other,
}

/// 一次資料・二次資料の区別（ランキングの初期方針で使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceOrigin {
    Primary,
    Secondary,
    Tertiary,
    #[default]
    Unknown,
}

/// 位置指定子。URL・API・ファイル・テキスト範囲・メディア区間・表のセルなど。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Locator {
    Url {
        url: String,
    },
    Api {
        endpoint: String,
        #[serde(default)]
        request: Option<String>,
    },
    File {
        path: String,
    },
    TextSpan {
        start: u64,
        end: u64,
    },
    Page {
        page: u32,
        #[serde(default)]
        bbox: Option<[u32; 4]>,
    },
    MediaSegment {
        start_ms: u64,
        end_ms: u64,
    },
    JsonPointer {
        pointer: String,
    },
    TableCell {
        table: TableId,
        row_key: String,
        #[serde(default)]
        column: Option<String>,
    },
    Opaque {
        value: String,
    },
}

impl Locator {
    pub fn key(&self) -> String {
        match self {
            Locator::Url { url } => url.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        }
    }
}

/// content-addressed なハッシュ（`blake3:<hex>`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(pub String);

/// Object Storage 上のスナップショット参照。本体は DB に格納しない。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObjectRef {
    pub hash: ContentHash,
    pub size: u64,
    #[serde(default)]
    pub media_type: Option<String>,
    /// スナップショットを crypto-shredding 可能な鍵で暗号化している場合。
    #[serde(default)]
    pub encrypted_with: Option<crate::KeyId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub id: SourceId,
    pub kind: SourceKind,
    pub locator: Locator,
    #[serde(default)]
    pub title: Option<String>,
    /// この情報源を表す Resource（Document / Post など）。
    #[serde(default)]
    pub resource: Option<ResourceId>,
    #[serde(default)]
    pub publisher: Option<ResourceId>,
    /// source_time: 公開・作成された時間。
    #[serde(default)]
    pub source_time: Option<TemporalExpression>,
    #[serde(default)]
    pub origin: SourceOrigin,
    #[serde(default)]
    pub reliability: Option<f32>,
    /// 転載元をたどった一次情報。独立性判定で同一 root を 1 出典として数える。
    #[serde(default)]
    pub provenance_root: Option<SourceId>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub visibility: Visibility,
    pub registered_at: Tick,
}

impl Source {
    pub fn root(&self) -> SourceId {
        self.provenance_root.unwrap_or(self.id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionMethod {
    Crawl,
    Api,
    Upload,
    Manual,
    Sensor,
    Import,
    AgentBrowse,
}

/// 取得行為。同じ URL・API を複数回取得した場合も別 Acquisition とする。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Acquisition {
    pub id: AcquisitionId,
    pub source: SourceId,
    /// 外部のクローラー・エージェントがその情報を取得した時刻（DB 登録時刻ではない）。
    pub acquired_at: Tick,
    pub acquired_by: ActorRef,
    pub method: AcquisitionMethod,
    pub locator: Locator,
    #[serde(default)]
    pub content_hash: Option<ContentHash>,
    #[serde(default)]
    pub snapshot_ref: Option<ObjectRef>,
    /// DB に登録された時刻（システムメタデータ）。
    pub recorded_at: Tick,
}

/// AI 等による抽出の追跡。再抽出・古い抽出器由来の検索・根拠確認に使う。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Derivation {
    pub id: DerivationId,
    pub acquisition: AcquisitionId,
    pub extractor: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_version: Option<String>,
    #[serde(default)]
    pub schema_version: Option<String>,
    #[serde(default)]
    pub source_span: Option<Locator>,
    pub extracted_at: Tick,
    #[serde(default)]
    pub extraction_conf: Option<f32>,
}
