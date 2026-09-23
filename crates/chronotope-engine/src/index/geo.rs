//! 座標の格子索引。座標は必ず Frame とセットで、変換で辿れる基準 Frame ごとに格子を持つ。
//! WGS84 は度単位の格子、その他の座標系は基準単位の格子を使う。

use chronotope_core::FrameId;
use chronotope_core::space::{FrameRegistry, Placement, Point};
use roaring::RoaringBitmap;
use std::collections::HashMap;

#[derive(Default)]
pub struct GeoIndex {
    cells: HashMap<(FrameId, i64, i64), RoaringBitmap>,
    docs: HashMap<u32, (FrameId, Point, Point, f64)>,
}

fn cell_size(frames: &FrameRegistry, root: &FrameId) -> f64 {
    if frames.is_geodetic(root) { 0.25 } else { 64.0 }
}

impl GeoIndex {
    pub fn insert(&mut self, frames: &FrameRegistry, doc: u32, p: &Placement) {
        self.remove(doc);
        let Some(root) = frames.to_root(p) else { return };
        let (min, max) = root.geometry.bbox();
        let cs = cell_size(frames, &root.frame);
        let (x0, x1) = ((min.x / cs).floor() as i64, (max.x / cs).floor() as i64);
        let (y0, y1) = ((min.y / cs).floor() as i64, (max.y / cs).floor() as i64);
        // 巨大な範囲は格子に展開せず、Frame 全体の走査対象にする（キー i64::MIN）。
        if (x1 - x0 + 1) * (y1 - y0 + 1) > 4096 {
            self.cells.entry((root.frame, i64::MIN, i64::MIN)).or_default().insert(doc);
        } else {
            for x in x0..=x1 {
                for y in y0..=y1 {
                    self.cells.entry((root.frame, x, y)).or_default().insert(doc);
                }
            }
        }
        self.docs.insert(doc, (root.frame, min, max, cs));
    }

    pub fn remove(&mut self, doc: u32) {
        if let Some((frame, min, max, cs)) = self.docs.remove(&doc) {
            let (x0, x1) = ((min.x / cs).floor() as i64, (max.x / cs).floor() as i64);
            let (y0, y1) = ((min.y / cs).floor() as i64, (max.y / cs).floor() as i64);
            if let Some(b) = self.cells.get_mut(&(frame, i64::MIN, i64::MIN)) {
                b.remove(doc);
            }
            if (x1 - x0 + 1) * (y1 - y0 + 1) <= 4096 {
                for x in x0..=x1 {
                    for y in y0..=y1 {
                        if let Some(b) = self.cells.get_mut(&(frame, x, y)) {
                            b.remove(doc);
                        }
                    }
                }
            }
        }
    }

    /// 基準 Frame 上の bbox と交差する文書。比較不能な Frame の場合は None。
    pub fn intersecting(&self, frames: &FrameRegistry, q: &Placement) -> Option<RoaringBitmap> {
        let root = frames.to_root(q)?;
        let (min, max) = root.geometry.bbox();
        let cs = cell_size(frames, &root.frame);
        let (x0, x1) = ((min.x / cs).floor() as i64, (max.x / cs).floor() as i64);
        let (y0, y1) = ((min.y / cs).floor() as i64, (max.y / cs).floor() as i64);
        let mut cand = RoaringBitmap::new();
        if (x1 - x0 + 1) * (y1 - y0 + 1) > 65_536 {
            for (d, (f, ..)) in &self.docs {
                if *f == root.frame {
                    cand.insert(*d);
                }
            }
        } else {
            for x in x0..=x1 {
                for y in y0..=y1 {
                    if let Some(b) = self.cells.get(&(root.frame, x, y)) {
                        cand |= b;
                    }
                }
            }
        }
        if let Some(b) = self.cells.get(&(root.frame, i64::MIN, i64::MIN)) {
            cand |= b;
        }
        Some(
            cand.into_iter()
                .filter(|d| {
                    let (_, a, b, _) = self.docs[d];
                    a.x <= max.x && b.x >= min.x && a.y <= max.y && b.y >= min.y
                })
                .collect(),
        )
    }
}
