//! 抽出した実体と既存 Resource の照合（エンティティリンキング）。
//!
//! 誤った統合は影響が大きいため、ラベル完全一致かつ型が両立する候補が 1 件のときだけ既存を使う。
//! 候補が複数なら新規作成し、候補それぞれへ possibly_same_as を付けて判断を保留する（自動統合しない）。

use crate::schema::EntityMention;
use chronotope_core::ResourceId;
use chronotope_core::vocab::type_id;
use chronotope_engine::KnowledgeBase;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum LinkDecision {
    Existing { resource: ResourceId, via: &'static str },
    New,
    Ambiguous { candidates: Vec<ResourceId> },
}

fn closure(kb: &KnowledgeBase, keys: &[String]) -> BTreeSet<ResourceId> {
    let s = kb.store();
    s.type_closure(keys.iter().filter_map(|k| s.type_id(k)))
}

/// 型が両立するか（根の Resource 以外に共通の型がある）。
fn compatible(kb: &KnowledgeBase, mention_types: &BTreeSet<ResourceId>, candidate: ResourceId) -> bool {
    let s = kb.store();
    let Some(r) = s.resource(candidate) else { return false };
    let root = type_id("Resource");
    let cand = s.type_closure(r.types.iter().copied());
    cand.iter().any(|t| *t != root && mention_types.contains(t))
}

pub fn link(kb: &KnowledgeBase, e: &EntityMention) -> LinkDecision {
    if let Some(r) = &e.resource {
        if let Ok(id) = kb.resolve_ref_str(r) {
            return LinkDecision::Existing { resource: id, via: "extractor" };
        }
    }
    // 出来事は一般名（「防災イベント」）で書かれることが多く、ラベルでは照合しない。
    if e.types.iter().any(|t| t == "Event") {
        return LinkDecision::New;
    }
    let types = closure(kb, &e.types);
    let cands: Vec<ResourceId> = kb.store().find_by_label(&e.label).into_iter().filter(|c| compatible(kb, &types, *c)).collect();
    match cands.as_slice() {
        [] => LinkDecision::New,
        [one] => LinkDecision::Existing { resource: *one, via: "label" },
        many => LinkDecision::Ambiguous { candidates: many.to_vec() },
    }
}
