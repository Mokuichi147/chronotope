//! Chronotope エンジン。
//!
//! ```text
//! Raw Data → Acquisition / Provenance → Canonical Knowledge Model
//!          → Resolver / Materializer → Search Projection → Indexes → Agent API
//! ```
//!
//! Canonical 層（[`canonical`]）は正しさと表現力を優先し、検索は必ず Projection（[`projection`]）
//! と索引を経由する。Canonical Graph 自体を検索エンジンとしては使わない。

pub mod canonical;
pub mod command;
pub mod crypto;
pub mod embed;
pub mod export;
pub mod facts;
pub mod index;
pub mod kb;
pub mod materialize;
pub mod projection;
pub mod query;
pub mod sqlexport;
pub mod store;
pub mod text;
pub mod vector;
pub mod view;
pub mod write;

pub use kb::{Freshness, KbConfig, KnowledgeBase};
pub use query::{QueryRequest, QueryResponse};
pub use write::WriteRequest;
