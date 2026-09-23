//! ストレージ境界。Canonical 本体はメモリ上に保持し、永続化は以下の差し替え可能な実装へ委ねる。
//! - [`log`]: Revision の追記専用ログ（再生で状態を復元する）
//! - [`object`]: Source snapshot の content-addressed Object Storage
//! - [`columnar`]: Observation / Trajectory / 表の行（DuckDB / Parquet に置き換える想定の境界）

pub mod columnar;
pub mod log;
pub mod object;
