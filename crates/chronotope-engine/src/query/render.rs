//! 応答の整形。Level 1 要約は約 80 トークンに収める。

use super::QCtx;
use crate::facts::ClaimEval;
use crate::projection::ProjectionRow;
use chronotope_core::model::*;
use chronotope_core::time::range::ResolvedTemporal;
use chronotope_core::time::resolve::Unresolved;
use chronotope_core::*;
use serde_json::{Value as Json, json};

/// 要約の目安トークン数。
pub const SUMMARY_TOKENS: usize = 80;

/// ざっくりしたトークン数見積もり（CJK は 1 文字 ≈ 1 トークン、ASCII は 4 文字 ≈ 1 トークン）。
pub fn estimate_tokens(s: &str) -> usize {
    let (mut ascii, mut other) = (0usize, 0usize);
    for c in s.chars() {
        if c.is_ascii() { ascii += 1 } else { other += 1 }
    }
    other + ascii.div_ceil(4)
}

pub fn truncate_tokens(s: &str, max: usize) -> String {
    if estimate_tokens(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for c in s.chars() {
        out.push(c);
        if estimate_tokens(&out) >= max.saturating_sub(1) {
            break;
        }
    }
    out.push('…');
    out
}

/// f32 由来の表示誤差（0.9200000166…）を小数 3 桁に丸める。
fn round_floats(v: Json) -> Json {
    match v {
        Json::Number(n) if n.is_f64() => json!((n.as_f64().unwrap_or(0.0) * 1000.0).round() / 1000.0),
        Json::Object(m) => Json::Object(m.into_iter().map(|(k, v)| (k, round_floats(v))).collect()),
        other => other,
    }
}

pub fn time_json(ctx: &QCtx, t: &ResolvedTemporal, raw: Option<&str>) -> Json {
    let cal = ctx.kb.store.calendar(&t.calendar_frame);
    let fmt = |x: chronotope_core::time::Tick| cal.as_ref().map(|c| c.format(x)).unwrap_or_else(|| x.to_iso());
    let mut j = json!({
        "raw": raw,
        "earliest_start": fmt(t.range.earliest_start),
        "latest_end": fmt(t.range.latest_end),
        "granularity": t.granularity,
        "axis": t.axis,
        "calendar": t.calendar_frame,
    });
    if let Some((a, b)) = t.range.certain_span() {
        j["certainly_during"] = json!([fmt(a), fmt(b)]);
    }
    if let Some(r) = &t.recurrence {
        j["recurrence"] = json!(r);
    }
    j
}

pub fn unresolved_json(u: &Unresolved, raw: Option<&str>) -> Json {
    json!({ "raw": raw, "unresolved": u.kind, "reason": u.message })
}

impl QCtx<'_> {
    pub fn label_of(&self, id: ResourceId) -> String {
        let s = &self.kb.store;
        if let Some(r) = s.resource(id) {
            return r.label(self.lang()).unwrap_or("").to_string();
        }
        if let Some(p) = s.predicates.get(&id) {
            return p.key.clone();
        }
        if let Some(t) = s.types.get(&id) {
            return t.key.clone();
        }
        id.to_string()
    }

    pub fn type_keys(&self, ids: &[ResourceId]) -> Vec<String> {
        ids.iter().filter_map(|t| self.kb.store.types.get(t)).map(|t| t.key.clone()).collect()
    }

    fn type_label(&self, id: &ResourceId) -> Option<String> {
        let t = self.kb.store.types.get(id)?;
        let lang = self.lang();
        Some(
            t.labels.iter().find(|l| lang.is_some() && l.lang.as_deref() == lang).or(t.labels.first()).map(|l| l.text.clone()).unwrap_or_else(|| t.key.clone()),
        )
    }

    /// 値の表示文字列（保護値は復号、破棄済みなら [shredded]）。
    pub fn display_value(&self, v: &Value) -> String {
        match self.kb.reveal(v) {
            Value::Resource(r) => self.label_of(r),
            Value::Text { text, .. } => text,
            Value::Quantity { amount, unit } => match unit {
                Some(u) => format!("{amount} {u}"),
                None => amount.to_string(),
            },
            Value::Bool(b) => b.to_string(),
            Value::Time(t) => t.raw_text,
            Value::Geo(p) => {
                let c = p.geometry.centroid();
                format!("({:.5}, {:.5}) @{}", c.x, c.y, self.kb.store.frames.get(&p.frame).map(|f| f.name.as_str()).unwrap_or("?"))
            }
            Value::Json(j) => truncate_tokens(&j.to_string(), 40),
            Value::Protected(_) => "[protected]".into(),
            Value::Unknown => "unknown".into(),
        }
    }

    pub fn value_json(&self, v: &Value) -> Json {
        match self.kb.reveal(v) {
            Value::Resource(r) => json!({ "resource": r, "label": self.label_of(r) }),
            Value::Time(t) => {
                json!({ "time": t.raw_text, "calendar": t.calendar_frame, "parsed": !matches!(t.ast, chronotope_core::time::expr::TimeAst::Unparsed) })
            }
            other => serde_json::to_value(&other).unwrap_or_default(),
        }
    }

    /// Level 1 要約。
    pub fn summary(&self, row: &ProjectionRow) -> Json {
        let lang = self.lang();
        let label = row.label(lang).to_string();
        let type_labels: Vec<String> = row.direct_types.iter().filter_map(|t| self.type_label(t)).collect();
        let places: Vec<String> = row.space_ids.iter().filter(|p| **p != row.canonical_id).take(3).map(|p| self.label_of(*p)).collect();
        let time = match (&row.temporal, &row.temporal_unresolved) {
            (Some(t), _) => {
                let mut j = time_json(self, t, row.temporal_raw.as_deref());
                if row.temporal_contested {
                    j["contested"] = json!(true);
                }
                Some(j)
            }
            (None, Some(u)) => Some(unresolved_json(u, row.temporal_raw.as_deref())),
            _ => None,
        };
        let mut text = label.clone();
        if !type_labels.is_empty() {
            text.push_str(&format!("（{}）", type_labels.join("・")));
        }
        if let Some(raw) = &row.temporal_raw {
            text.push_str(&format!(" 時期: {raw}"));
            if row.temporal_contested {
                text.push_str("［異説あり］");
            }
        }
        if !places.is_empty() {
            text.push_str(&format!(" 場所: {}", places.join("、")));
        }
        if let Some(d) = &row.description {
            text.push_str(" — ");
            text.push_str(d);
        }
        let mut j = json!({
            "id": row.canonical_id,
            "label": label,
            "types": self.type_keys(&row.direct_types),
            "summary": truncate_tokens(&text, SUMMARY_TOKENS),
            "rank": (row.rank * 1000.0).round() / 1000.0,
            "contested": row.contested,
            "stale": row.stale,
        });
        if let Some(t) = time {
            j["time"] = t;
        }
        if !places.is_empty() {
            j["places"] = json!(places);
        }
        if !row.redistributable {
            j["redistributable"] = json!(false);
        }
        j
    }

    pub fn claim_json(&self, a: &Assertion, st: AssertionStatus, eval: Option<&ClaimEval>) -> Json {
        let s = &self.kb.store;
        let pred = s.predicates.get(&a.predicate);
        let license = a.license.as_ref().and_then(|l| s.licenses.get(l));
        let mut j = json!({
            "id": a.id,
            "predicate": pred.map(|p| p.key.clone()).unwrap_or_else(|| a.predicate.to_string()),
            "value": self.display_value(&a.object),
            "object": self.value_json(&a.object),
            "status": st,
            "polarity": a.polarity,
            "branch": a.branch,
            "evidence_count": a.evidence.len(),
            "first_known_at": a.first_known_at.to_iso(),
            "asserted_by": a.asserted_by,
        });
        if let Some(e) = eval {
            j["rank"] = json!((e.rank.value * 1000.0).round() / 1000.0);
            j["tier"] = json!(e.rank.tier);
            j["rank_policy"] = json!(format!("{}@{}", e.rank.rank_policy_id, e.rank.rank_policy_version));
            j["confidence"] = round_floats(json!(e.confidence));
        }
        if let Some(vt) = &a.valid_time {
            j["valid_time"] = json!(vt.raw_text);
        }
        if let Some(c) = a.canon {
            j["canon"] = json!({ "id": c, "label": self.label_of(c) });
        }
        if let Some(t) = a.timeline {
            j["timeline"] = json!({ "id": t, "label": self.label_of(t) });
        }
        if let Some(sc) = a.spatial_scope {
            j["spatial_scope"] = json!({ "id": sc, "label": self.label_of(sc) });
        }
        if let Some(l) = license {
            j["license"] = json!(l.key);
            j["redistributable"] = json!(l.redistributable);
        }
        if let Some(x) = a.supersedes {
            j["supersedes"] = json!(x);
        }
        if let Some(x) = a.superseded_by {
            j["superseded_by"] = json!(x);
        }
        if let Some(n) = &a.note {
            j["note"] = json!(n);
        }
        j
    }
}
