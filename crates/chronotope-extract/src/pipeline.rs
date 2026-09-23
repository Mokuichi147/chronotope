//! 抽出結果を書き込み API へ変換して取り込む。
//!
//! 取り込み主体の権限はそのまま適用される（AI・クローラーの主張は proposed から始まる）。
//! すべての主張に、取得（Acquisition）と抽出（Derivation: 抽出器・モデル・スキーマ版・本文中の位置・確信度）を付ける。

use crate::link::{LinkDecision, link};
use crate::schema::*;
use chronotope_core::model::Principal;
use chronotope_core::time::Tick;
use chronotope_core::{Error, Result};
use chronotope_engine::{KnowledgeBase, WriteRequest};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct IngestOptions {
    /// 書き込まずに、照合結果と書き込み予定だけを返す。
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityOutcome {
    #[serde(rename = "ref")]
    pub reference: String,
    pub label: String,
    pub types: Vec<String>,
    #[serde(flatten)]
    pub decision: LinkDecision,
    /// 取り込み後の Resource（新規作成した場合はその ID）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IngestReport {
    pub dry_run: bool,
    pub extraction: Extraction,
    pub entities: Vec<EntityOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Value>,
    pub assertions: Vec<Value>,
    pub observations: Vec<Value>,
    pub proposed_predicates: Vec<String>,
    pub warnings: Vec<String>,
}

fn write(kb: &mut KnowledgeBase, p: &Principal, body: Value) -> Result<Value> {
    let req: WriteRequest = serde_json::from_value(body.clone()).map_err(|e| Error::invalid(format!("internal write request {body}: {e}")))?;
    kb.write(p, req)
}

fn validate(kb: &KnowledgeBase, x: &Extraction) -> Result<()> {
    let refs: HashMap<&str, &EntityMention> = x.entities.iter().map(|e| (e.reference.as_str(), e)).collect();
    if refs.len() != x.entities.len() {
        return Err(Error::invalid("duplicate entity ref in extraction"));
    }
    for e in &x.entities {
        if e.label.trim().is_empty() {
            return Err(Error::invalid(format!("entity {} has an empty label", e.reference)));
        }
        for t in &e.types {
            if kb.store().type_id(t).is_none() {
                return Err(Error::invalid(format!("entity {}: unknown type `{t}`", e.reference)));
            }
        }
    }
    let known = |r: &str| refs.contains_key(r);
    for c in &x.claims {
        if !known(&c.subject) {
            return Err(Error::invalid(format!("claim subject `{}` is not an entity ref", c.subject)));
        }
        if let ObjectMention::Ref { reference } = &c.object {
            if !known(reference) {
                return Err(Error::invalid(format!("claim object `{reference}` is not an entity ref")));
            }
        }
    }
    for o in &x.observations {
        if !known(&o.target) {
            return Err(Error::invalid(format!("observation target `{}` is not an entity ref", o.target)));
        }
    }
    Ok(())
}

fn range_of(o: &ObjectMention) -> Value {
    match o {
        ObjectMention::Ref { .. } => json!({ "kind": "resource" }),
        ObjectMention::Time { .. } => json!({ "kind": "literal", "literal": "time" }),
        ObjectMention::Quantity { .. } => json!({ "kind": "literal", "literal": "quantity" }),
        ObjectMention::Text { .. } => json!({ "kind": "literal", "literal": "text" }),
    }
}

/// 文書と抽出結果を取り込む。
pub fn ingest(kb: &mut KnowledgeBase, p: &Principal, doc: &Document, x: &Extraction, opts: &IngestOptions) -> Result<IngestReport> {
    validate(kb, x)?;
    let mut warnings = vec![];
    let mut entities: Vec<EntityOutcome> = x
        .entities
        .iter()
        .map(|e| EntityOutcome { reference: e.reference.clone(), label: e.label.clone(), types: e.types.clone(), decision: link(kb, e), resource: None })
        .collect();
    for o in &entities {
        if let LinkDecision::Ambiguous { candidates } = &o.decision {
            warnings.push(format!(
                "`{}` matches {} existing resources; created a new one and marked possibly_same_as instead of choosing",
                o.label,
                candidates.len()
            ));
        }
    }
    let missing_predicates: Vec<String> = {
        let mut v: Vec<String> = x.claims.iter().map(|c| c.predicate.clone()).filter(|k| kb.store().predicate(k).is_none()).collect();
        v.sort();
        v.dedup();
        v
    };
    if opts.dry_run {
        return Ok(IngestReport {
            dry_run: true,
            extraction: x.clone(),
            entities,
            source: None,
            assertions: vec![],
            observations: vec![],
            proposed_predicates: missing_predicates,
            warnings,
        });
    }

    // 1. 取得の記録（本文をスナップショットとして保存）
    let acquired_at = doc.acquired_at.clone().unwrap_or_else(|| kb.now().to_iso());
    let source_time = doc.published.clone().or_else(|| x.source_time.clone());
    let locator = match &doc.url {
        Some(u) => json!({ "type": "url", "url": u }),
        None => json!({ "type": "opaque", "value": format!("text:blake3:{}", blake3::hash(doc.text.as_bytes()).to_hex()) }),
    };
    let mut src = json!({
        "op": "link_source",
        "locator": locator,
        "kind": doc.kind.clone().unwrap_or_else(|| "web_page".into()),
        "origin": doc.origin.clone().unwrap_or_else(|| "secondary".into()),
        "acquisition": { "acquired_at": acquired_at, "content": doc.text, "media_type": "text/plain" },
    });
    for (k, v) in [("title", &doc.title), ("license", &doc.license), ("source_time", &source_time)] {
        if let Some(v) = v {
            src[k] = json!(v);
        }
    }
    if source_time.is_some() {
        src["calendar"] = json!(doc.calendar());
    }
    let source = write(kb, p, src)?;
    let acquisition = source["acquisition"].as_str().ok_or_else(|| Error::Storage("link_source returned no acquisition".into()))?.to_string();

    // 2. 実体（既存にリンク、無ければ作成、曖昧なら作成 + possibly_same_as）
    let mut ids: HashMap<String, String> = HashMap::new();
    let lang = doc.lang.clone().unwrap_or_else(|| "ja".into());
    for (o, e) in entities.iter_mut().zip(&x.entities) {
        let id = match &o.decision {
            LinkDecision::Existing { resource, .. } => resource.to_string(),
            LinkDecision::New | LinkDecision::Ambiguous { .. } => {
                let mut r = json!({ "types": e.types, "label": e.label, "lang": lang });
                if let Some(d) = &e.description {
                    r["description"] = json!(d);
                }
                if let Some(m) = e.mention.as_ref().filter(|m| **m != e.label) {
                    r["aliases"] = json!([m]);
                }
                write(kb, p, json!({ "op": "create_resource", "resource": r }))?["id"].as_str().unwrap_or_default().to_string()
            }
        };
        o.resource = Some(id.clone());
        ids.insert(o.reference.clone(), id);
    }

    let extracted_at = kb.now().to_iso();
    let derivation = |span: Option<[usize; 2]>, conf: Option<f32>| {
        let mut d = json!({
            "extractor": x.extractor.name,
            "schema_version": x.extractor.schema_version,
            "extracted_at": extracted_at,
        });
        for (k, v) in [("model", &x.extractor.model), ("model_version", &x.extractor.model_version)] {
            if let Some(v) = v {
                d[k] = json!(v);
            }
        }
        if let Some([s, e]) = span {
            d["source_span"] = json!({ "type": "text_span", "start": s, "end": e });
        }
        if let Some(c) = conf {
            d["extraction_conf"] = json!(c);
        }
        d
    };
    let mut assertions = vec![];
    for o in &entities {
        if let LinkDecision::Ambiguous { candidates } = &o.decision {
            for c in candidates {
                let r = write(
                    kb,
                    p,
                    json!({ "op": "propose_assertion", "subject": ids[&o.reference], "predicate": "possibly_same_as", "object": { "resource": c.to_string() }, "evidence": [{ "acquisition": acquisition, "derivation": derivation(None, None) }] }),
                )?;
                assertions.push(json!({ "predicate": "possibly_same_as", "subject": o.reference, "object": c, "result": r }));
            }
        }
    }

    // 3. 語彙に無い述語は提案する（proposed のまま使う）
    for k in &missing_predicates {
        let rng = x.claims.iter().find(|c| &c.predicate == k).map(|c| range_of(&c.object)).unwrap_or(json!({ "kind": "any" }));
        write(kb, p, json!({ "op": "propose_predicate", "key": k, "range": rng }))?;
        warnings.push(format!("predicate `{k}` was not in the vocabulary and has been proposed; it needs curator approval"));
    }

    // 4. 主張
    let calendar = doc.calendar();
    for c in &x.claims {
        let object = match &c.object {
            ObjectMention::Ref { reference } => json!({ "resource": ids[reference] }),
            ObjectMention::Time { time, calendar: cal } => json!({ "time": time, "calendar": cal.clone().unwrap_or_else(|| calendar.clone()) }),
            ObjectMention::Quantity { quantity, unit } => json!({ "quantity": quantity, "unit": unit }),
            ObjectMention::Text { text } => json!({ "text": text, "lang": lang }),
        };
        let mut body = json!({
            "op": "propose_assertion",
            "subject": ids[&c.subject],
            "predicate": c.predicate,
            "object": object,
            "evidence": [{ "acquisition": acquisition, "derivation": derivation(c.span, c.confidence) }],
        });
        if let Some(l) = &doc.license {
            body["license"] = json!(l);
        }
        match write(kb, p, body) {
            Ok(r) => assertions.push(json!({ "predicate": c.predicate, "subject": c.subject, "result": r })),
            Err(e) => warnings.push(format!("claim {} {} skipped: {e}", c.subject, c.predicate)),
        }
    }

    // 5. 観測値（観測時刻は情報源の時刻、無ければ取得時刻）
    let observed_at = source_time.as_deref().and_then(|t| Tick::parse_iso(t).ok()).map(|t| t.to_iso()).unwrap_or_else(|| acquired_at.clone());
    let mut observations = vec![];
    for o in &x.observations {
        let r = write(
            kb,
            p,
            json!({ "op": "add_observation", "target": ids[&o.target], "metric": o.metric, "observed_at": observed_at, "value": o.value, "unit": o.unit, "acquisition": acquisition }),
        )?;
        if o.approximate {
            warnings.push(format!("observation {} = {} is approximate (約/およそ)", o.metric, o.value));
        }
        observations.push(json!({ "target": o.target, "metric": o.metric, "value": o.value, "result": r }));
    }

    Ok(IngestReport {
        dry_run: false,
        extraction: x.clone(),
        entities,
        source: Some(source),
        assertions,
        observations,
        proposed_predicates: missing_predicates,
        warnings,
    })
}
