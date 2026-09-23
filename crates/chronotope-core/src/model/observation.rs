//! Observation（時間変化する数値）と Trajectory（移動体の軌跡）。
//! 大量履歴は列指向ストア（DuckDB / Parquet）へ分離できるよう、本体とは別に保持する。

use crate::space::{Placement, Point};
use crate::time::Tick;
use crate::{AcquisitionId, BranchId, FrameId, ObservationId, ResourceId, TrajectoryId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub id: ObservationId,
    pub target: ResourceId,
    /// 指標名（`likes`, `population`, `temperature`）。
    pub metric: String,
    pub observed_at: Tick,
    pub value: f64,
    /// UCUM 単位。
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub acquisition: Option<AcquisitionId>,
    pub branch: BranchId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Interpolation {
    #[default]
    Linear,
    Step,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TrajectorySample {
    pub t: Tick,
    pub pos: Point,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TrajectoryStorage {
    Inline,
    /// Parquet 等の外部ファイル（samples は空で、読み出しは列指向ストアが担う）。
    External {
        uri: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trajectory {
    pub id: TrajectoryId,
    pub target: ResourceId,
    pub reference_frame: FrameId,
    /// 時刻順に並んだサンプル。
    pub samples: Vec<TrajectorySample>,
    #[serde(default)]
    pub interpolation: Interpolation,
    #[serde(default)]
    pub acquisition: Option<AcquisitionId>,
    pub storage: TrajectoryStorage,
}

impl Trajectory {
    pub fn time_span(&self) -> Option<(Tick, Tick)> {
        Some((self.samples.first()?.t, self.samples.last()?.t))
    }

    /// 時刻 `t` の位置（範囲外や補間なしで該当サンプルが無い場合は None）。
    pub fn position_at(&self, t: Tick) -> Option<Placement> {
        let i = self.samples.partition_point(|s| s.t <= t);
        let pos = match (i, self.interpolation) {
            (0, _) => return None,
            (i, _) if self.samples[i - 1].t == t => self.samples[i - 1].pos,
            (i, _) if i == self.samples.len() => return None,
            (i, Interpolation::Step) => self.samples[i - 1].pos,
            (_, Interpolation::None) => return None,
            (i, Interpolation::Linear) => {
                let (a, b) = (self.samples[i - 1], self.samples[i]);
                let f = (t.0 - a.t.0) as f64 / (b.t.0 - a.t.0) as f64;
                Point { x: a.pos.x + (b.pos.x - a.pos.x) * f, y: a.pos.y + (b.pos.y - a.pos.y) * f, z: a.pos.z.zip(b.pos.z).map(|(p, q)| p + (q - p) * f) }
            }
        };
        Some(Placement { frame: self.reference_frame, geometry: crate::space::Geometry::Point { at: pos } })
    }
}
