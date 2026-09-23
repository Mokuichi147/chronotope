//! ランキング方針。一般的な初期方針は
//! 人間確認 > 独立した複数出典による裏付け > 一次資料 > 二次資料 > AI 抽出のみ
//! だが絶対規則にはせず、`tier_weight` と各成分の重みで調整できる。

use crate::model::{ComputedRank, ConfidenceComponents, RankTier, SourceOrigin};
use crate::time::Tick;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankPolicy {
    pub id: String,
    pub version: u32,
    /// Tier の重み（0 にすると成分スコアのみで順位付け）。
    pub tier_weight: f64,
    pub w_source: f64,
    pub w_extraction: f64,
    pub w_corroboration: f64,
    pub w_specificity: f64,
    /// 何個の独立出典で裏付けが飽和するか。
    pub corroboration_saturation: u32,
    /// Accepted 以外の状態に掛ける係数。
    pub proposed_factor: f64,
    pub disputed_factor: f64,
}

impl Default for RankPolicy {
    fn default() -> Self {
        RankPolicy {
            id: "default".into(),
            version: 1,
            tier_weight: 1.0,
            w_source: 0.35,
            w_extraction: 0.2,
            w_corroboration: 0.3,
            w_specificity: 0.15,
            corroboration_saturation: 3,
            proposed_factor: 0.8,
            disputed_factor: 0.5,
        }
    }
}

/// ランク計算の入力。
#[derive(Debug, Clone)]
pub struct RankInput<'a> {
    pub confidence: &'a ConfidenceComponents,
    /// 根拠の中で最も一次に近い出典区分。
    pub best_origin: SourceOrigin,
    /// 根拠がすべて AI 抽出由来か（根拠が無い AI の主張も含む）。
    pub ai_only: bool,
    pub status: crate::model::AssertionStatus,
}

impl RankPolicy {
    pub fn corroboration(&self, independent_sources: u32) -> f32 {
        let extra = independent_sources.saturating_sub(1) as f32;
        (extra / self.corroboration_saturation.max(1) as f32).min(1.0)
    }

    pub fn tier(&self, i: &RankInput) -> RankTier {
        let c = i.confidence;
        if c.human_verified {
            RankTier::HumanVerified
        } else if c.independent_sources >= 2 {
            RankTier::Corroborated
        } else if i.ai_only {
            RankTier::AiOnly
        } else if i.best_origin == SourceOrigin::Primary {
            RankTier::Primary
        } else {
            RankTier::Secondary
        }
    }

    pub fn compute(&self, i: &RankInput, now: Tick) -> ComputedRank {
        let c = i.confidence;
        let default_source = match i.best_origin {
            SourceOrigin::Primary => 0.8,
            SourceOrigin::Secondary => 0.6,
            SourceOrigin::Tertiary => 0.45,
            SourceOrigin::Unknown => 0.4,
        };
        let mut num = 0.0;
        let mut den = 0.0;
        let mut add = |w: f64, v: Option<f32>| {
            if let Some(v) = v {
                num += w * v.clamp(0.0, 1.0) as f64;
                den += w;
            }
        };
        add(self.w_source, Some(c.source_reliability.unwrap_or(default_source)));
        add(self.w_extraction, c.extraction_conf);
        add(self.w_corroboration, Some(c.corroboration));
        add(self.w_specificity, c.specificity);
        let mut score = if den > 0.0 { num / den } else { 0.0 };
        score *= match i.status {
            crate::model::AssertionStatus::Accepted => 1.0,
            crate::model::AssertionStatus::Proposed => self.proposed_factor,
            crate::model::AssertionStatus::Disputed => self.disputed_factor,
            _ => 0.0,
        };
        let tier = self.tier(i);
        // score は [0, 1) に収め、tier の境界をまたがないようにする。
        let value = self.tier_weight * tier as u8 as f64 + score.min(0.999_999);
        ComputedRank { value, tier, rank_policy_id: self.id.clone(), rank_policy_version: self.version, rank_computed_at: now }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AssertionStatus;

    #[test]
    fn tiers_order() {
        let p = RankPolicy::default();
        let mk = |hv: bool, n: u32, origin: SourceOrigin, ai: bool| {
            let c = ConfidenceComponents { human_verified: hv, independent_sources: n, corroboration: p.corroboration(n), ..Default::default() };
            p.compute(&RankInput { confidence: &c, best_origin: origin, ai_only: ai, status: AssertionStatus::Accepted }, Tick(0)).value
        };
        let human = mk(true, 1, SourceOrigin::Secondary, false);
        let corroborated = mk(false, 2, SourceOrigin::Secondary, false);
        let primary = mk(false, 1, SourceOrigin::Primary, false);
        let secondary = mk(false, 1, SourceOrigin::Secondary, false);
        let ai = mk(false, 1, SourceOrigin::Primary, true);
        assert!(human > corroborated && corroborated > primary && primary > secondary && secondary > ai);
    }
}
