//! HTTP Agent API。
//!
//! - `POST /v1/query`  … JSON Query DSL（`budget_ms` 必須）
//! - `POST /v1/write`  … 意味的書き込み API（`op` で操作を指定）
//! - `GET  /v1/resources/{id}` … Tier 0 参照
//! - `GET  /v1/freshness?branch=` … Projection の鮮度
//! - `GET  /v1/export/rdf?branch=` … N-Triples（再配布可能な承認済み主張のみ）
//! - `POST /v1/materialize` … Materializer を即時実行
//!
//! 認証は前段のゲートウェイで行う前提で、主体はヘッダ
//! `x-chronotope-principal` / `x-chronotope-kind` / `x-chronotope-groups` / `x-chronotope-curator` から作る。
//! ヘッダが無い場合は匿名エージェント（公開情報のみ・proposed でしか書けない）として扱う。

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chronotope_core::model::{ActorKind, ActorRef, Principal};
use chronotope_core::{BranchId, Error};
use chronotope_engine::{KnowledgeBase, QueryRequest, WriteRequest};
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

type Shared = Arc<RwLock<KnowledgeBase>>;

struct ApiError(Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Invalid(_) | Error::Parse(_) => StatusCode::BAD_REQUEST,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::Conflict(_) | Error::Cycle(_) => StatusCode::CONFLICT,
            Error::Incomparable(_) | Error::DepthExceeded(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Error::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": { "code": self.0.code(), "message": self.0.to_string() } }))).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError(e)
    }
}

fn principal(h: &HeaderMap) -> Principal {
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty());
    let kind = match get("x-chronotope-kind") {
        Some("human") => ActorKind::Human,
        Some("crawler") => ActorKind::Crawler,
        Some("sensor") => ActorKind::Sensor,
        _ => ActorKind::Agent,
    };
    let id = get("x-chronotope-principal").unwrap_or("anonymous").to_string();
    let groups = get("x-chronotope-groups").map(|g| g.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    // AI エージェントはキュレーターになれない。
    let curator = kind == ActorKind::Human && get("x-chronotope-curator") == Some("true");
    Principal { actor: ActorRef { id, kind }, groups, curator }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, Error> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError(Error::Storage(format!("worker panicked: {e}"))))?.map_err(ApiError)
}

async fn query(State(kb): State<Shared>, headers: HeaderMap, body: String) -> Result<impl IntoResponse, ApiError> {
    let p = principal(&headers);
    let req: QueryRequest = serde_json::from_str(&body).map_err(|e| ApiError(Error::invalid(format!("query: {e}"))))?;
    let r = blocking(move || kb.read().query(&p, &req)).await?;
    Ok(Json(r))
}

async fn write(State(kb): State<Shared>, headers: HeaderMap, body: String) -> Result<impl IntoResponse, ApiError> {
    let p = principal(&headers);
    let req: WriteRequest = serde_json::from_str(&body).map_err(|e| ApiError(Error::invalid(format!("write: {e}"))))?;
    let r = blocking(move || kb.write().write(&p, req)).await?;
    Ok(Json(r))
}

async fn resource(State(kb): State<Shared>, headers: HeaderMap, Path(id): Path<String>) -> Result<impl IntoResponse, ApiError> {
    let p = principal(&headers);
    let body = json!({ "op": "lookup", "id": id, "budget_ms": 50 }).to_string();
    let r = blocking(move || kb.read().query_json(&p, &body)).await?;
    Ok(Json(r))
}

#[derive(Deserialize)]
struct BranchParam {
    #[serde(default)]
    branch: Option<String>,
}

fn branch_of(kb: &KnowledgeBase, b: &Option<String>) -> Result<BranchId, Error> {
    match b {
        None => Ok(BranchId::main()),
        Some(name) => kb.store().branch_id(name).ok_or_else(|| Error::not_found(format!("branch `{name}`"))),
    }
}

async fn freshness(State(kb): State<Shared>, Query(q): Query<BranchParam>) -> Result<impl IntoResponse, ApiError> {
    let r = blocking(move || {
        let kb = kb.read();
        kb.freshness(branch_of(&kb, &q.branch)?)
    })
    .await?;
    Ok(Json(r))
}

async fn export_rdf(State(kb): State<Shared>, Query(q): Query<BranchParam>) -> Result<impl IntoResponse, ApiError> {
    let r = blocking(move || {
        let kb = kb.read();
        kb.export_ntriples(branch_of(&kb, &q.branch)?)
    })
    .await?;
    Ok(([("content-type", "application/n-triples; charset=utf-8")], r))
}

async fn materialize(State(kb): State<Shared>) -> Result<impl IntoResponse, ApiError> {
    let n = blocking(move || kb.write().materialize_all()).await?;
    Ok(Json(json!({ "processed": n })))
}

pub async fn serve(kb: KnowledgeBase, addr: &str, interval_ms: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let batch = kb.config().materialize_batch;
    let shared: Shared = Arc::new(RwLock::new(kb));
    let bg = shared.clone();
    // 結果整合: 書き込みは無効化キューに積むだけで、ここで少しずつ Projection を更新する。
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(interval_ms.max(10)));
        loop {
            tick.tick().await;
            let kb = bg.clone();
            let res = tokio::task::spawn_blocking(move || {
                let mut kb = kb.write();
                kb.materialize(batch)
            })
            .await;
            match res {
                Ok(Ok(stats)) => {
                    let processed: usize = stats.iter().map(|(_, s)| s.processed).sum();
                    if processed > 0 {
                        tracing::debug!(processed, "materialized");
                    }
                }
                Ok(Err(e)) => tracing::error!(error = %e, "materializer failed"),
                Err(e) => tracing::error!(error = %e, "materializer panicked"),
            }
        }
    });
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/query", post(query))
        .route("/v1/write", post(write))
        .route("/v1/resources/{id}", get(resource))
        .route("/v1/freshness", get(freshness))
        .route("/v1/export/rdf", get(export_rdf))
        .route("/v1/materialize", post(materialize))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "chronotope agent API listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
