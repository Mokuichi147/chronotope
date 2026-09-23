//! 抽出の入出力形式。
//!
//! 位置（span）はすべて本文の **文字** 単位（Unicode スカラー値）の `[start, end)`。

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: &str = "extract-v1";

/// 取り込む文書。本文に加えて、出典と時間解決に必要な情報を持つ。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub text: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// 公開日時（source_time）。「20日」「昨日」などの相対表現の基準になる。
    #[serde(default)]
    pub published: Option<String>,
    /// 取得日時（acquired_at）。省略時は現在時刻。
    #[serde(default)]
    pub acquired_at: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    /// 時間表現を解釈する暦（既定 `gregorian+09:00`）。
    #[serde(default)]
    pub calendar: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    /// SourceKind（既定 `web_page`）。
    #[serde(default)]
    pub kind: Option<String>,
    /// SourceOrigin（既定 `secondary`）。
    #[serde(default)]
    pub origin: Option<String>,
}

impl Document {
    pub fn calendar(&self) -> String {
        self.calendar.clone().unwrap_or_else(|| "gregorian+09:00".into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractorInfo {
    /// 抽出器名（`chronotope-rules`, `my-llm-extractor` など）。
    pub name: String,
    /// LLM を使った場合のモデル名。規則ベースや人手なら None。
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_version: Option<String>,
    #[serde(default = "schema_version")]
    pub schema_version: String,
}

fn schema_version() -> String {
    SCHEMA_VERSION.into()
}

/// 文書中に現れた実体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntityMention {
    /// 抽出結果内だけで通用する参照名（`E1`, `P1` など）。
    #[serde(rename = "ref")]
    pub reference: String,
    pub types: Vec<String>,
    /// 正規化した名前（登録時のラベル）。
    pub label: String,
    /// 本文中の表記（`東京駅丸の内口の広場` など）。
    #[serde(default)]
    pub mention: Option<String>,
    #[serde(default)]
    pub span: Option<[usize; 2]>,
    #[serde(default)]
    pub description: Option<String>,
    /// 抽出器が既存 Resource を特定できた場合の ID（`res_...` / `wikidata:Q..`）。
    #[serde(default)]
    pub resource: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ObjectMention {
    Ref {
        #[serde(rename = "ref")]
        reference: String,
    },
    /// 時間表現は原文のまま（例: `20日午後3時半ごろ`）。解決はエンジンが公開日時を基準に行う。
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
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaimMention {
    pub subject: String,
    /// 既存語彙の述語キー。未定義なら取り込み時に提案（proposed）される。
    pub predicate: String,
    pub object: ObjectMention,
    #[serde(default)]
    pub span: Option<[usize; 2]>,
    #[serde(default)]
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationMention {
    pub target: String,
    pub metric: String,
    pub value: f64,
    #[serde(default)]
    pub unit: Option<String>,
    /// 「約」「およそ」などの概数。
    #[serde(default)]
    pub approximate: bool,
    #[serde(default)]
    pub span: Option<[usize; 2]>,
}

/// 抽出結果（中間形式）。任意の抽出器がこの形の JSON を出せば取り込める。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Extraction {
    pub extractor: ExtractorInfo,
    /// 本文から推定した公開日時（見出しの日付など）。Document.published が無いときに使う。
    #[serde(default)]
    pub source_time: Option<String>,
    #[serde(default)]
    pub entities: Vec<EntityMention>,
    #[serde(default)]
    pub claims: Vec<ClaimMention>,
    #[serde(default)]
    pub observations: Vec<ObservationMention>,
}
