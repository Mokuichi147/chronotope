//! 表データ（CSV / SQL / Parquet）。Graph へ無理に変換せず、元の Schema と JOIN 関係を保つ。
//! 必要な行だけ RowLink で Resource と共通 ID 接続する。

use crate::{ResourceId, TableId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    /// `int64`, `float64`, `text`, `timestamp`, `bool` ...
    pub dtype: String,
    #[serde(default)]
    pub unit: Option<String>,
    /// この列が指す Predicate（行を Assertion へ展開する際に使う）。
    #[serde(default)]
    pub predicate: Option<ResourceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub references: TableId,
    pub referenced_columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TableStorage {
    /// 行を列指向ストアに保持（小規模向け）。
    Inline,
    Parquet {
        uri: String,
    },
    Csv {
        uri: String,
    },
    /// 外部 SQL（DuckDB / PostgreSQL）のリレーション。
    Sql {
        dsn: String,
        relation: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableDef {
    pub id: TableId,
    /// 所属する Dataset Resource。
    pub dataset: ResourceId,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    #[serde(default)]
    pub primary_key: Vec<String>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKey>,
    pub storage: TableStorage,
}

/// 表の行と Resource の接続。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RowLink {
    pub table: TableId,
    pub row_key: String,
    pub resource: ResourceId,
}
