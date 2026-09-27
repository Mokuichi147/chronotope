//! 会話・ツール実行の原文イベント。
//!
//! 原文（受け付けた入力・応答・ツールの引数と結果）は Acquisition のスナップショットとして
//! バイト列のまま保存し、ここには発言者・生成元・記録順・因果関係のメタデータだけを持つ。
//! 会話全体は所有者だけが見える Source、各イベントはその Acquisition になる。
//! 要約や抽出した知識はイベントを参照する派生データとして扱い、原文を置き換えない。

use super::security::ActorRef;
use crate::time::Tick;
use crate::{AcquisitionId, SourceId};
use serde::{Deserialize, Serialize};

/// イベントの形。誰が生成したかは [`EventOrigin`] で別に持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Message,
    ToolCall,
    ToolResult,
    /// 圧縮・要約など、他のイベントから作った派生テキスト（`derived_from` で元を指す）。
    Summary,
    Attachment,
    Other,
}

/// 実際の生成元。API 上の role（`api_role`）とは独立で、入力・生成を受け持つコードが設定する。
/// 例えば自動継続の指示は API 上 `role: user` でも `runtime` であり、ユーザー本人の発言ではない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventOrigin {
    /// 人が入力した（インターフェースが受け付けた）もの。
    Human,
    /// モデルの出力。
    Model,
    /// ツールの実行結果。
    Tool,
    /// 実行環境が差し込んだ指示・通知（自動継続・失敗通知・委任用のプロンプトなど）。
    Runtime,
    /// システムプロンプトなどの設定。
    System,
    /// 別のエージェント（委任元・委任先）。
    Agent,
    /// 移行した過去の履歴などで生成元が分からない。推測で埋めない。
    Unknown,
}

impl EventOrigin {
    pub fn is_human(self) -> bool {
        matches!(self, EventOrigin::Human)
    }
}

/// ツール呼び出し・結果などの状態。中断や結果不明を成功と区別する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    Ok,
    Error,
    Interrupted,
    /// 呼び出したが結果が分からない（応答が失われたなど）。
    Unknown,
}

/// クライアントが送る、イベントの内容を決める項目。再送時はこの一致で同じイベントか判定する。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventFields {
    pub conversation: String,
    #[serde(default)]
    pub turn: Option<String>,
    /// 会話内の記録順。時刻ではなくこの値で順序を決める。
    pub sequence: u64,
    pub kind: EventKind,
    pub origin: EventOrigin,
    /// API 上の role（`user` / `assistant` / `tool` など）。発言者の判定には使わない。
    #[serde(default)]
    pub api_role: Option<String>,
    /// 発言者（人・モデル・ツール名など）。
    #[serde(default)]
    pub speaker: Option<String>,
    /// インターフェースが受け付けた・生成された時刻。移行時に分からなければ省略する。
    #[serde(default)]
    pub received_at: Option<Tick>,
    #[serde(default)]
    pub response_id: Option<String>,
    /// ツールの呼び出しと結果を結ぶ ID。
    #[serde(default)]
    pub call_id: Option<String>,
    /// 因果関係の親（委任元のイベントなど）。
    #[serde(default)]
    pub parent_event: Option<String>,
    #[serde(default)]
    pub status: Option<EventStatus>,
    /// 訂正・編集の対象。過去のイベントは上書きしない。
    #[serde(default)]
    pub supersedes: Option<String>,
    /// 要約などの元になったイベント。
    #[serde(default)]
    pub derived_from: Vec<String>,
    /// クライアント固有の補足（モデル名・ツール名など）。
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationEvent {
    /// 所有者。認証済みの主体（委任されていれば委任元）から決まり、リクエストでは指定できない。
    pub owner: String,
    /// クライアントが保存前に採番する、変更しない ID（所有者の範囲で一意）。
    pub event_id: String,
    #[serde(flatten)]
    pub fields: EventFields,
    /// 会話を表す Source。
    pub source: SourceId,
    /// 原文のスナップショットを持つ Acquisition。
    pub acquisition: AcquisitionId,
    /// 原文のバイト数（暗号化前）。
    #[serde(default)]
    pub size: u64,
    pub recorded_by: ActorRef,
    /// DB に記録した時刻。
    pub recorded_at: Tick,
}

impl ConversationEvent {
    /// 並べ替え・期間指定に使う時刻（受信時刻が無ければ記録時刻）。
    pub fn occurred_at(&self) -> Tick {
        self.fields.received_at.unwrap_or(self.recorded_at)
    }
}
