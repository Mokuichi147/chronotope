//! 空間モデル。
//!
//! 場所は緯度経度だけで表さず、Place の包含階層（World → Region → Area → Place → Subplace）と、
//! 座標を持つ場合は必ず SpatialReferenceFrame とセットで扱う。時間側の CalendarFrame と対称的に、
//! Frame は親 Frame への変換を持ち、共通祖先まで変換できる Frame 同士だけが座標比較可能になる。

use crate::{Error, FrameId, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoordinateSystem {
    /// 緯度経度（x = 経度, y = 緯度, z = 楕円体高）。
    Wgs84,
    /// 直交座標（ゲーム内座標・建物内ローカル座標など）。
    Cartesian,
    /// 格子（ダンジョンのマス目など）。`cell` は親単位でのマス幅。
    Grid { cell: f64 },
    /// その他の独自座標系。
    Custom { name: String },
    /// 座標を持たない（夢・精神世界など、包含・隣接関係のみ）。
    None,
}

/// アフィン変換（3×4 行列）。自 Frame の座標 → 親 Frame の座標。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Affine3 {
    pub m: [[f64; 4]; 3],
}

impl Affine3 {
    pub fn identity() -> Self {
        Affine3 { m: [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]] }
    }

    pub fn translate_scale(tx: f64, ty: f64, tz: f64, scale: f64) -> Self {
        Affine3 { m: [[scale, 0.0, 0.0, tx], [0.0, scale, 0.0, ty], [0.0, 0.0, scale, tz]] }
    }

    pub fn apply(&self, p: Point) -> Point {
        let v = [p.x, p.y, p.z.unwrap_or(0.0)];
        let r = |i: usize| self.m[i][0] * v[0] + self.m[i][1] * v[1] + self.m[i][2] * v[2] + self.m[i][3];
        Point { x: r(0), y: r(1), z: p.z.map(|_| r(2)) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialReferenceFrame {
    pub id: FrameId,
    pub name: String,
    #[serde(default)]
    pub parent: Option<FrameId>,
    pub coordinate_system: CoordinateSystem,
    pub dimensionality: u8,
    /// UCUM 単位（`m`, `deg`, `[tile]` など）。
    #[serde(default)]
    pub unit: Option<String>,
    /// 親 Frame への変換。無い場合、親とは座標比較できない（包含関係のみ）。
    #[serde(default)]
    pub transform: Option<Affine3>,
}

impl SpatialReferenceFrame {
    pub fn wgs84() -> Self {
        SpatialReferenceFrame {
            id: FrameId::named("wgs84"),
            name: "WGS84".into(),
            parent: None,
            coordinate_system: CoordinateSystem::Wgs84,
            dimensionality: 2,
            unit: Some("deg".into()),
            transform: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub z: Option<f64>,
}

impl Point {
    pub fn xy(x: f64, y: f64) -> Self {
        Point { x, y, z: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Geometry {
    Point { at: Point },
    BBox { min: Point, max: Point },
}

impl Geometry {
    pub fn bbox(&self) -> (Point, Point) {
        match self {
            Geometry::Point { at } => (*at, *at),
            Geometry::BBox { min, max } => (*min, *max),
        }
    }

    pub fn centroid(&self) -> Point {
        let (a, b) = self.bbox();
        Point { x: (a.x + b.x) / 2.0, y: (a.y + b.y) / 2.0, z: a.z.zip(b.z).map(|(p, q)| (p + q) / 2.0) }
    }

    fn map(&self, f: impl Fn(Point) -> Point) -> Geometry {
        match self {
            Geometry::Point { at } => Geometry::Point { at: f(*at) },
            Geometry::BBox { min, max } => {
                let (a, b) = (f(*min), f(*max));
                Geometry::BBox { min: Point::xy(a.x.min(b.x), a.y.min(b.y)), max: Point::xy(a.x.max(b.x), a.y.max(b.y)) }
            }
        }
    }
}

/// 座標付きの位置（必ず Frame とセット）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub frame: FrameId,
    pub geometry: Geometry,
}

/// Frame の登録簿。Frame 間の変換と比較可能性を判断する。
#[derive(Debug, Clone, Default)]
pub struct FrameRegistry {
    frames: HashMap<FrameId, SpatialReferenceFrame>,
}

impl FrameRegistry {
    pub fn new() -> Self {
        let mut r = FrameRegistry::default();
        r.insert(SpatialReferenceFrame::wgs84());
        r
    }

    pub fn insert(&mut self, f: SpatialReferenceFrame) {
        self.frames.insert(f.id, f);
    }

    pub fn get(&self, id: &FrameId) -> Option<&SpatialReferenceFrame> {
        self.frames.get(id)
    }

    pub fn all(&self) -> impl Iterator<Item = &SpatialReferenceFrame> {
        self.frames.values()
    }

    /// 親 Frame を登録する前に循環しないか確認する。
    pub fn validate(&self, f: &SpatialReferenceFrame) -> Result<()> {
        let mut cur = f.parent;
        let mut depth = 0;
        while let Some(p) = cur {
            if p == f.id {
                return Err(Error::Cycle(format!("frame {} is its own ancestor", f.id)));
            }
            depth += 1;
            if depth > 64 {
                return Err(Error::DepthExceeded("frame hierarchy deeper than 64".into()));
            }
            cur = self.frames.get(&p).ok_or_else(|| Error::not_found(format!("parent frame {p}")))?.parent;
        }
        Ok(())
    }

    /// 変換で辿れる最上位 Frame（座標比較の基準）と、そこまでの変換後ジオメトリ。
    pub fn to_root(&self, p: &Placement) -> Option<Placement> {
        let mut frame = self.frames.get(&p.frame)?;
        let mut geom = p.geometry.clone();
        for _ in 0..64 {
            match (frame.parent, frame.transform) {
                (Some(parent), Some(t)) => {
                    geom = geom.map(|pt| t.apply(pt));
                    frame = self.frames.get(&parent)?;
                }
                _ => return Some(Placement { frame: frame.id, geometry: geom }),
            }
        }
        None
    }

    /// 2 点間の距離（共通の基準 Frame で比較。比較不能なら None）。
    /// WGS84 は大圏距離（メートル）、その他は基準 Frame の単位でのユークリッド距離。
    pub fn distance(&self, a: &Placement, b: &Placement) -> Option<f64> {
        let (ra, rb) = (self.to_root(a)?, self.to_root(b)?);
        if ra.frame != rb.frame {
            return None;
        }
        let (pa, pb) = (ra.geometry.centroid(), rb.geometry.centroid());
        let frame = self.frames.get(&ra.frame)?;
        Some(match frame.coordinate_system {
            CoordinateSystem::Wgs84 => haversine_m(pa, pb),
            CoordinateSystem::None => return None,
            _ => {
                let dz = pa.z.zip(pb.z).map(|(x, y)| x - y).unwrap_or(0.0);
                ((pa.x - pb.x).powi(2) + (pa.y - pb.y).powi(2) + dz * dz).sqrt()
            }
        })
    }

    pub fn is_geodetic(&self, id: &FrameId) -> bool {
        matches!(self.frames.get(id).map(|f| &f.coordinate_system), Some(CoordinateSystem::Wgs84))
    }
}

/// 大圏距離（メートル）。x = 経度, y = 緯度。
pub fn haversine_m(a: Point, b: Point) -> f64 {
    const R: f64 = 6_371_008.8;
    let (la1, la2) = (a.y.to_radians(), b.y.to_radians());
    let dla = la2 - la1;
    let dlo = (b.x - a.x).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * R * h.sqrt().asin()
}

/// 場所の階層レベル（型として付与する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceLevel {
    World,
    Region,
    Area,
    Place,
    Subplace,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_distance() {
        let mut reg = FrameRegistry::new();
        let map = SpatialReferenceFrame {
            id: FrameId::named("game-map"),
            name: "map".into(),
            parent: None,
            coordinate_system: CoordinateSystem::Cartesian,
            dimensionality: 2,
            unit: Some("m".into()),
            transform: None,
        };
        let room = SpatialReferenceFrame {
            id: FrameId::named("room"),
            name: "room".into(),
            parent: Some(map.id),
            coordinate_system: CoordinateSystem::Grid { cell: 2.0 },
            dimensionality: 2,
            unit: Some("[tile]".into()),
            transform: Some(Affine3::translate_scale(100.0, 0.0, 0.0, 2.0)),
        };
        reg.insert(map.clone());
        reg.validate(&room).unwrap();
        reg.insert(room.clone());
        let a = Placement { frame: room.id, geometry: Geometry::Point { at: Point::xy(0.0, 0.0) } };
        let b = Placement { frame: map.id, geometry: Geometry::Point { at: Point::xy(103.0, 4.0) } };
        assert!((reg.distance(&a, &b).unwrap() - 5.0).abs() < 1e-9);
        let tokyo = Placement { frame: FrameId::named("wgs84"), geometry: Geometry::Point { at: Point::xy(139.767, 35.681) } };
        assert!(reg.distance(&a, &tokyo).is_none(), "different worlds are incomparable");
        let osaka = Placement { frame: FrameId::named("wgs84"), geometry: Geometry::Point { at: Point::xy(135.495, 34.702) } };
        let d = reg.distance(&tokyo, &osaka).unwrap();
        assert!((390_000.0..410_000.0).contains(&d), "{d}");
    }
}
