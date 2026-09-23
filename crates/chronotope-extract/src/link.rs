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

/// 場所 `child` が `ancestor` の配下か（located_in / inside / contains をたどる）。
pub fn within(kb: &KnowledgeBase, child: ResourceId, ancestor: ResourceId) -> bool {
    let s = kb.store();
    let preds: Vec<(ResourceId, bool)> =
        ["located_in", "inside", "contains"].iter().filter_map(|k| s.predicate_by_key.get(*k).map(|p| (*p, *k == "contains"))).collect();
    let mut stack = vec![child];
    let mut seen = std::collections::HashSet::new();
    while let Some(x) = stack.pop() {
        if !seen.insert(x) || seen.len() > 64 {
            continue;
        }
        let mut parents: Vec<ResourceId> = vec![];
        for aid in s.by_subject.get(&x).into_iter().flatten() {
            let a = &s.assertions[aid];
            if preds.iter().any(|(p, rev)| *p == a.predicate && !rev) && a.status.is_live() {
                parents.extend(a.object.as_resource());
            }
        }
        if let Some((cp, _)) = preds.iter().find(|(_, rev)| *rev) {
            for aid in s.by_object_predicate.get(&(x, *cp)).into_iter().flatten() {
                if s.assertions[aid].status.is_live() {
                    parents.push(s.assertions[aid].subject);
                }
            }
        }
        for p in parents {
            let p = s.resolve_id(p);
            if p == ancestor {
                return true;
            }
            stack.push(p);
        }
    }
    false
}

/// 抽出器が「`parent` の配下」と記録した実体の照合。同名の別地域（`海辺市本町` を別県の `本町`）を避けるため、
/// 候補が `parent` の配下にある場合だけ既存に結び付ける。
pub fn link_within(kb: &KnowledgeBase, e: &EntityMention, parent: Option<ResourceId>) -> LinkDecision {
    let d = link(kb, e);
    match (d, parent) {
        (LinkDecision::Existing { resource, via: "label" }, Some(p)) if !within(kb, resource, p) => LinkDecision::New,
        (LinkDecision::Ambiguous { candidates }, Some(p)) => {
            let inside: Vec<ResourceId> = candidates.into_iter().filter(|c| within(kb, *c, p)).collect();
            match inside.as_slice() {
                [] => LinkDecision::New,
                [one] => LinkDecision::Existing { resource: *one, via: "label" },
                _ => LinkDecision::Ambiguous { candidates: inside },
            }
        }
        (d, _) => d,
    }
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
