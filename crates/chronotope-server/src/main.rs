//! chronotope CLI: HTTP Agent API サーバ・ベンチマーク・デモ・単発クエリ。

mod bench;
mod demo;
mod http;

use chronotope_core::BranchId;
use chronotope_core::model::Principal;
use chronotope_engine::{KbConfig, KnowledgeBase};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "chronotope", version, about = "Universal spatiotemporal knowledge base for AI agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// HTTP Agent API を起動する。
    Serve {
        #[arg(long, default_value = "./data")]
        data: PathBuf,
        #[arg(long, default_value = "127.0.0.1:7878")]
        addr: String,
        /// 永続化しない（開発用）。
        #[arg(long)]
        in_memory: bool,
        /// バックグラウンド Materializer の実行間隔。
        #[arg(long, default_value_t = 200)]
        materialize_interval_ms: u64,
        /// 1 回の Materializer 実行で処理する最大件数（書き込みロックの保持時間を抑える）。
        #[arg(long, default_value_t = 2000)]
        materialize_batch: usize,
        /// デモデータを投入してから起動する（in-memory 時のみ）。
        #[arg(long)]
        seed_demo: bool,
    },
    /// 合成データで Tier 0 / Tier 1 のレイテンシを測定する。
    Bench {
        #[arg(long, default_value_t = 100_000)]
        events: usize,
        #[arg(long, default_value_t = 2_000)]
        queries: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        /// 生成したデータを PostgreSQL 用の COPY 形式 SQL として書き出す。
        #[arg(long)]
        export_sql: Option<PathBuf>,
    },
    /// デモデータを投入し、代表的なクエリ結果を表示する。
    Demo,
    /// データディレクトリに対して単発のクエリ（JSON DSL）を実行する。
    Query {
        #[arg(long, default_value = "./data")]
        data: PathBuf,
        /// クエリ JSON（`-` で標準入力）。
        json: String,
    },
    /// Canonical 層と Projection を PostgreSQL / Citus スキーマ用の COPY 形式 SQL で出力する。
    ExportSql {
        #[arg(long, default_value = "./data")]
        data: PathBuf,
        /// データディレクトリの代わりにデモデータを使う。
        #[arg(long)]
        demo: bool,
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// 承認済み・再配布可能な Assertion を N-Triples で出力する。
    ExportRdf {
        #[arg(long, default_value = "./data")]
        data: PathBuf,
        #[arg(long, default_value = "main")]
        branch: String,
    },
}

fn main() -> anyhow_like::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { data, addr, in_memory, materialize_interval_ms, materialize_batch, seed_demo } => {
            let config = KbConfig { materialize_batch, ..KbConfig::default() };
            let mut kb = if in_memory { KnowledgeBase::in_memory(config) } else { KnowledgeBase::open(&data, config)? };
            if seed_demo {
                demo::seed(&mut kb)?;
            }
            kb.materialize_all()?;
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            rt.block_on(http::serve(kb, &addr, materialize_interval_ms))?;
        }
        Cmd::Bench { events, queries, seed, export_sql } => bench::run(events, queries, seed, export_sql)?,
        Cmd::ExportSql { data, demo, branch } => {
            let mut kb = if demo {
                let mut kb = KnowledgeBase::in_memory(KbConfig::default());
                demo::seed(&mut kb)?;
                kb
            } else {
                KnowledgeBase::open(&data, KbConfig::default())?
            };
            let b = kb.store().branch_id(&branch).ok_or_else(|| anyhow_like::msg(format!("unknown branch {branch}")))?;
            kb.materialize_branch(b)?;
            let stdout = std::io::stdout();
            let mut out = std::io::BufWriter::new(stdout.lock());
            kb.export_sql(&mut out, b)?;
        }
        Cmd::Demo => demo::run()?,
        Cmd::Query { data, json } => {
            let body = if json == "-" {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                s
            } else {
                json
            };
            let mut kb = KnowledgeBase::open(&data, KbConfig::default())?;
            kb.materialize_all()?;
            let r = kb.query_json(&Principal::anonymous(), &body)?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        Cmd::ExportRdf { data, branch } => {
            let mut kb = KnowledgeBase::open(&data, KbConfig::default())?;
            let b = kb.store().branch_id(&branch).ok_or_else(|| anyhow_like::msg(format!("unknown branch {branch}")))?;
            if b != BranchId::main() {
                kb.materialize_branch(b)?;
            }
            print!("{}", kb.export_ntriples(b)?);
        }
    }
    Ok(())
}

/// 依存を増やさないための最小限のエラー型。
mod anyhow_like {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
    pub fn msg(s: String) -> Box<dyn std::error::Error + Send + Sync> {
        s.into()
    }
}
