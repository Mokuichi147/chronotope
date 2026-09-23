//! Embedder の境界。本番では外部モデル（API / ローカル推論）で生成したベクトルを
//! `set_embedding` で登録する。組み込みの HashingEmbedder は依存なしで動く字句ベースの近似で、
//! 開発・テスト・フォールバック用。

use crate::text::tokenize;

pub trait Embedder: Send + Sync {
    fn model_id(&self) -> &str;
    fn version(&self) -> &str;
    fn dimension(&self) -> usize;
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// 特徴ハッシュ（トークン → 次元・符号）による埋め込み。
pub struct HashingEmbedder {
    pub dimension: usize,
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        HashingEmbedder { dimension: 256 }
    }
}

impl Embedder for HashingEmbedder {
    fn model_id(&self) -> &str {
        "hashing"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn dimension(&self) -> usize {
        self.dimension
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dimension];
        for tok in tokenize(text) {
            let h = blake3::hash(tok.as_bytes());
            let b = h.as_bytes();
            let idx = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize % self.dimension;
            let sign = if b[4] & 1 == 0 { 1.0 } else { -1.0 };
            v[idx] += sign;
        }
        v
    }
}
