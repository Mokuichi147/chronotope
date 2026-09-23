//! Chronotope のコアモデル。
//!
//! Canonical 層のデータ型（Resource / Assertion / Provenance / Identity / Revision など）と、
//! ストレージに依存しない時間モデル・空間モデル・ランキング方針を定義する。
//! このクレートは I/O を持たず、純粋なデータ型とアルゴリズムだけを提供する。

pub mod error;
pub mod id;
pub mod model;
pub mod rank;
pub mod space;
pub mod time;
pub mod vocab;

pub use error::{Error, Result};
pub use id::*;
