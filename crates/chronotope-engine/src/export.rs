//! RDF（N-Triples）エクスポート。
//!
//! 再配布が許可された（license.redistributable）公開 Assertion だけを出力する。
//! 述語は外部語彙の exact mapping があればその IRI、無ければ `urn:chronotope:predicate:<key>` を使う。
//! 主張の状態・出典はトリプル単体では表せないため、承認済み・肯定の主張のみを直接トリプルにし、
//! それ以外は出力しない（異説を事実として書き出さないため）。

use crate::kb::KnowledgeBase;
use crate::view::{StatusSet, View};
use chronotope_core::model::*;
use chronotope_core::*;
use std::fmt::Write;

fn iri_res(id: &ResourceId) -> String {
    format!("<urn:chronotope:res:{}>", id.0.simple())
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o
}

fn lit(s: &str, lang: Option<&str>) -> String {
    match lang {
        Some(l) => format!("\"{}\"@{}", esc(s), l),
        None => format!("\"{}\"", esc(s)),
    }
}

const RDFS_LABEL: &str = "<http://www.w3.org/2000/01/rdf-schema#label>";
const RDF_TYPE: &str = "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

impl KnowledgeBase {
    pub fn export_ntriples(&self, branch: BranchId) -> Result<String> {
        let s = &self.store;
        let view = View { statuses: StatusSet::of(&[AssertionStatus::Accepted]), ..View::projection(s, branch)? };
        let redistributable = |l: &Option<String>| l.as_ref().is_none_or(|k| s.licenses.get(k).is_some_and(|x| x.redistributable));
        let pred_iri = |p: &PredicateDef| -> String {
            p.mappings
                .iter()
                .find(|m| m.match_kind == MatchKind::Exact && m.iri.starts_with("http"))
                .map(|m| format!("<{}>", m.iri))
                .unwrap_or_else(|| format!("<urn:chronotope:predicate:{}>", p.key))
        };
        let type_iri = |t: &ResourceId| -> String {
            match s.types.get(t) {
                Some(def) => def
                    .mappings
                    .iter()
                    .find(|m| m.iri.starts_with("http"))
                    .map(|m| format!("<{}>", m.iri))
                    .unwrap_or_else(|| format!("<urn:chronotope:type:{}>", def.key)),
                None => iri_res(t),
            }
        };
        let mut out = String::new();
        let mut ids: Vec<&ResourceId> = s.resources.keys().collect();
        ids.sort();
        for id in ids {
            let r = &s.resources[id];
            if s.resolve_id(*id) != *id || !r.visibility.is_public() || !redistributable(&r.license) {
                continue;
            }
            let subj = iri_res(id);
            for l in &r.labels {
                let _ = writeln!(out, "{subj} {RDFS_LABEL} {} .", lit(&l.text, l.lang.as_deref()));
            }
            for t in &r.types {
                let _ = writeln!(out, "{subj} {RDF_TYPE} {} .", type_iri(t));
            }
            for (a, _) in s.claims_about(*id, &view) {
                if a.polarity != Polarity::Affirmed
                    || !a.visibility.is_public()
                    || !redistributable(&a.license)
                    || a.valid_time.is_some()
                    || a.canon.is_some()
                    || a.timeline.is_some()
                {
                    continue;
                }
                let Some(p) = s.predicates.get(&a.predicate) else { continue };
                let obj = match &a.object {
                    Value::Resource(o) => iri_res(&s.resolve_id(*o)),
                    Value::Text { text, lang } => lit(text, lang.as_deref()),
                    Value::Quantity { amount, unit } => match unit {
                        Some(u) => format!("\"{amount} {}\"^^<http://unitsofmeasure.org/ucum#>", esc(u)),
                        None => format!("\"{amount}\"^^<{XSD}double>"),
                    },
                    Value::Bool(b) => format!("\"{b}\"^^<{XSD}boolean>"),
                    Value::Time(t) => lit(&t.raw_text, None),
                    Value::Json(j) => lit(&j.to_string(), None),
                    Value::Geo(_) | Value::Protected(_) | Value::Unknown => continue,
                };
                let _ = writeln!(out, "{subj} {} {obj} .", pred_iri(p));
            }
        }
        Ok(out)
    }
}
