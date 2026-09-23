//! 列指向データ（Observation 履歴・Trajectory・表の行）の境界。
//! 初期実装はメモリ上。大量データでは DuckDB / Parquet 実装へ差し替える。

use chronotope_core::model::{Observation, Trajectory};
use chronotope_core::time::Tick;
use chronotope_core::{ResourceId, TableId};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};

pub trait ObservationStore: Send + Sync {
    fn insert(&mut self, o: Observation);
    fn range(&self, target: ResourceId, metric: Option<&str>, from: Tick, to: Tick, limit: usize) -> Vec<Observation>;
    fn latest(&self, target: ResourceId, metric: &str, at_or_before: Tick) -> Option<Observation>;
    fn metrics(&self, target: ResourceId) -> Vec<String>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Default)]
pub struct MemoryObservationStore {
    by_target: HashMap<ResourceId, BTreeMap<(String, Tick, chronotope_core::ObservationId), Observation>>,
    count: usize,
}

impl ObservationStore for MemoryObservationStore {
    fn insert(&mut self, o: Observation) {
        self.count += 1;
        self.by_target.entry(o.target).or_default().insert((o.metric.clone(), o.observed_at, o.id), o);
    }

    fn range(&self, target: ResourceId, metric: Option<&str>, from: Tick, to: Tick, limit: usize) -> Vec<Observation> {
        let Some(m) = self.by_target.get(&target) else { return vec![] };
        m.values().filter(|o| metric.is_none_or(|x| o.metric == x) && o.observed_at >= from && o.observed_at < to).take(limit).cloned().collect()
    }

    fn latest(&self, target: ResourceId, metric: &str, at_or_before: Tick) -> Option<Observation> {
        let m = self.by_target.get(&target)?;
        m.values().filter(|o| o.metric == metric && o.observed_at <= at_or_before).max_by_key(|o| o.observed_at).cloned()
    }

    fn metrics(&self, target: ResourceId) -> Vec<String> {
        let mut v: Vec<String> = self.by_target.get(&target).map(|m| m.keys().map(|k| k.0.clone()).collect()).unwrap_or_default();
        v.dedup();
        v
    }

    fn len(&self) -> usize {
        self.count
    }
}

#[derive(Default)]
pub struct ColumnarStore {
    pub observations: Box<MemoryObservationStore>,
    pub trajectories: HashMap<ResourceId, Vec<Trajectory>>,
    pub table_rows: HashMap<TableId, Vec<Map<String, Value>>>,
}

impl ColumnarStore {
    pub fn rows(&self, table: &TableId) -> &[Map<String, Value>] {
        self.table_rows.get(table).map(Vec::as_slice).unwrap_or(&[])
    }
}
