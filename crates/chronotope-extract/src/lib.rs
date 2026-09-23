//! 非構造化テキストから Assertion を作る抽出パイプライン。
//!
//! ```text
//! 文書（本文 + URL・公開日時・取得日時・ライセンス）
//!   → 抽出（Extraction: 実体・主張・観測値。時間表現は原文のまま）
//!   → 照合（既存 Resource へのリンク。一意でなければ新規 + possibly_same_as）
//!   → 書き込み（link_source → create_resource → propose_assertion / add_observation）
//! ```
//!
//! 抽出器は差し替え可能で、組み込みの [`rules::RuleExtractor`]（外部サービスを呼ばない規則ベース）のほか、
//! 任意のツール（人手・ローカル LLM など）が出力した [`schema::Extraction`] JSON をそのまま取り込める。
//! エンジン自体は LLM を呼ばない。

pub mod link;
pub mod pipeline;
pub mod rules;
pub mod schema;

pub use pipeline::{IngestOptions, IngestReport, ingest};
pub use rules::RuleExtractor;
pub use schema::{Document, Extraction};
