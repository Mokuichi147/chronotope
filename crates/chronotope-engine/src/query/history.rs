//! 会話履歴の読み取り（`history_search` / `history_get` / `history_context` / `history_conversations`）と、
//! スナップショット本文のページ取得。
//!
//! - 所有者は既定で呼び出し元（委任されていれば委任元）。他の所有者を指定しても可視性で絞る。
//! - 検索は候補探しに使い、回答・引用は `history_get` で取得した原文に基づける。
//! - 本文はバイト単位の範囲で返し、`next_offset` をたどれば欠落なく最後まで読める。

use super::QCtx;
use crate::history::Unindexed;
use crate::store::object::content_hash;
use crate::text::{MappedText, gram_query_tokens, query_terms};
use base64::Engine as _;
use chronotope_core::model::*;
use chronotope_core::time::Tick;
use chronotope_core::*;
use roaring::RoaringBitmap;
use serde::Deserialize;
use serde_json::{Value as Json, json};

pub(crate) const DEFAULT_PAGE_BYTES: u64 = 64 * 1024;
pub(crate) const MAX_PAGE_BYTES: u64 = 1024 * 1024;

/// 本文の返し方。`auto` は UTF-8 として読めればテキスト、読めなければ base64。
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContentEncoding {
    #[default]
    Auto,
    Text,
    Base64,
}

/// 本文の `[offset, offset + length)` バイトを返す。テキストで返す場合は UTF-8 の文字境界で終え、
/// 続きの位置を `next_offset` で示す（文字の途中で切って置換文字にすることはしない）。
pub(crate) fn content_page(bytes: &[u8], offset: u64, length: Option<u64>, encoding: ContentEncoding) -> Result<Json> {
    let total = bytes.len();
    let start =
        usize::try_from(offset).ok().filter(|s| *s <= total).ok_or_else(|| Error::invalid(format!("offset {offset} is beyond the content size {total}")))?;
    let want = length.unwrap_or(DEFAULT_PAGE_BYTES).clamp(1, MAX_PAGE_BYTES) as usize;
    let mut end = start.saturating_add(want).min(total);
    let text = match encoding {
        ContentEncoding::Base64 => None,
        ContentEncoding::Auto => std::str::from_utf8(bytes).ok(),
        ContentEncoding::Text => Some(std::str::from_utf8(bytes).map_err(|_| Error::invalid("content is not UTF-8 text; use encoding `base64`"))?),
    };
    let mut j = match text {
        Some(s) => {
            if !s.is_char_boundary(start) {
                return Err(Error::invalid(format!("offset {start} is inside a UTF-8 character; continue from `next_offset` or use encoding `base64`")));
            }
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            if end == start && start < total {
                // 1 文字より短い長さを指定された場合も 1 文字は返す。
                end = start + s[start..].chars().next().map_or(0, char::len_utf8);
            }
            json!({ "encoding": "utf-8", "text": &s[start..end] })
        }
        None => json!({ "encoding": "base64", "base64": base64::engine::general_purpose::STANDARD.encode(&bytes[start..end]) }),
    };
    j["range"] = json!({ "unit": "byte", "start": start, "end": end });
    j["total_size"] = json!(total);
    j["next_offset"] = if end < total { json!(end) } else { Json::Null };
    Ok(j)
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryOrder {
    /// 新しい順（受信時刻、無ければ記録時刻）。
    #[default]
    Newest,
    Oldest,
    /// 会話内の記録順（会話を 1 つ指定したときだけ）。
    Sequence,
}

impl HistoryOrder {
    fn tag(self) -> &'static str {
        match self {
            HistoryOrder::Newest => "n",
            HistoryOrder::Oldest => "o",
            HistoryOrder::Sequence => "s",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HistorySearch {
    /// 本文の検索語。空白区切りの語をすべて含むイベント（正規化後の部分一致）。
    #[serde(default)]
    pub text: Option<String>,
    /// `text` を正規化せず、原文の連続した文字列としてそのまま照合する。
    #[serde(default)]
    pub exact: bool,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub conversation: Option<String>,
    #[serde(default)]
    pub conversations: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<EventKind>,
    #[serde(default)]
    pub origins: Vec<EventOrigin>,
    #[serde(default)]
    pub call_id: Option<String>,
    /// 期間（受信時刻、無ければ記録時刻）。`[from, to)`。
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub order: HistoryOrder,
    #[serde(default)]
    pub limit: Option<usize>,
    /// 前回の結果の `next_cursor`。
    #[serde(default)]
    pub cursor: Option<String>,
}

fn owner_scope<'a>(ctx: &'a QCtx, owner: Option<&'a str>) -> &'a str {
    owner.unwrap_or_else(|| ctx.principal.owner_id())
}

fn visible(ctx: &QCtx, ev: &ConversationEvent) -> bool {
    ctx.kb.store.sources.get(&ev.source).is_some_and(|s| ctx.principal.can_see(&s.visibility))
}

/// 閲覧できるイベント。無い・見えない場合は区別せず not_found（他人の履歴の有無を漏らさない）。
fn find_event<'a>(ctx: &'a QCtx, owner: &str, event_id: &str) -> Result<(u32, &'a ConversationEvent)> {
    let h = &ctx.kb.history;
    h.doc(owner, event_id)
        .and_then(|d| h.get(d).map(|e| (d, e)))
        .filter(|(_, e)| visible(ctx, e))
        .ok_or_else(|| Error::not_found(format!("event `{event_id}`")))
}

fn event_json(ctx: &QCtx, doc: u32, ev: &ConversationEvent) -> Json {
    let h = &ctx.kb.history;
    let acq = ctx.kb.store.acquisitions.get(&ev.acquisition);
    let f = &ev.fields;
    let ids = |docs: &[u32]| docs.iter().filter(|d| **d != doc).filter_map(|d| h.get(*d)).map(|e| e.event_id.clone()).collect::<Vec<_>>();
    json!({
        "event_id": ev.event_id,
        "owner": ev.owner,
        "conversation": f.conversation,
        "turn": f.turn,
        "sequence": f.sequence,
        "kind": f.kind,
        "origin": f.origin,
        "api_role": f.api_role,
        "speaker": f.speaker,
        "received_at": f.received_at.map(Tick::to_iso),
        "recorded_at": ev.recorded_at.to_iso(),
        "recorded_by": ev.recorded_by,
        "response_id": f.response_id,
        "call_id": f.call_id,
        "call_peers": ids(h.call_peers(ev)),
        "parent_event": f.parent_event,
        "status": f.status,
        "supersedes": f.supersedes,
        "superseded_by": ids(h.superseded_by(ev)),
        "derived_from": f.derived_from,
        "metadata": f.metadata,
        "acquisition": ev.acquisition,
        "content_hash": acq.and_then(|a| a.content_hash.clone()),
        "media_type": acq.and_then(|a| a.snapshot_ref.as_ref()).and_then(|r| r.media_type.clone()),
        "size": ev.size,
        "text_indexed": h.unindexed(doc).is_none(),
        "not_indexed_reason": h.unindexed(doc).map(Unindexed::as_str),
    })
}

/// イベントの原文（読めなければ理由）。
fn event_content(ctx: &QCtx, ev: &ConversationEvent) -> Result<std::result::Result<Vec<u8>, Unindexed>> {
    match ctx.kb.store.acquisitions.get(&ev.acquisition).and_then(|a| a.snapshot_ref.as_ref()) {
        Some(r) => ctx.kb.read_snapshot(r),
        None => Ok(Err(Unindexed::Missing)),
    }
}

/// `[start, end)` の前後を `around` 文字ずつ含めた抜粋。
fn excerpt(s: &str, start: usize, end: usize, around: usize) -> Json {
    let from = s[..start].char_indices().rev().nth(around.saturating_sub(1)).map_or(0, |(i, _)| i);
    let to = s[end..].char_indices().nth(around).map_or(s.len(), |(i, _)| end + i);
    json!({ "text": &s[from..to], "range": { "unit": "byte", "start": from, "end": to }, "complete": from == 0 && to == s.len() })
}

fn sort_key(order: HistoryOrder, ev: &ConversationEvent) -> i64 {
    match order {
        HistoryOrder::Sequence => i64::try_from(ev.fields.sequence).unwrap_or(i64::MAX),
        _ => ev.occurred_at().0,
    }
}

fn parse_cursor(order: HistoryOrder, c: &str) -> Result<(i64, u32)> {
    let bad = || Error::invalid(format!("bad cursor `{c}` (cursors are only valid for the same query and order)"));
    let mut it = c.splitn(3, ':');
    if it.next() != Some(order.tag()) {
        return Err(bad());
    }
    let k = it.next().and_then(|x| x.parse().ok()).ok_or_else(bad)?;
    let d = it.next().and_then(|x| x.parse().ok()).ok_or_else(bad)?;
    Ok((k, d))
}

fn index_status(ctx: &QCtx) -> Json {
    // 本文の索引はコミットと同時に更新するため、記録済みのイベントはすべて検索対象になっている。
    json!({ "consistent": true, "revision": ctx.kb.store.head_seq, "events": ctx.kb.history.len() })
}

pub(super) fn search(ctx: &QCtx, spec: &HistorySearch) -> Result<Json> {
    let h = &ctx.kb.history;
    let owner = owner_scope(ctx, spec.owner.as_deref());
    let limit = spec.limit.unwrap_or(20).clamp(1, 200);
    let mut convs: Vec<&String> = spec.conversations.iter().chain(spec.conversation.as_ref()).collect();
    convs.sort();
    convs.dedup();
    if spec.order == HistoryOrder::Sequence && convs.len() != 1 {
        return Err(Error::invalid("order `sequence` needs exactly one conversation"));
    }
    let from = spec.from.as_deref().map(Tick::parse_iso).transpose()?;
    let to = spec.to.as_deref().map(Tick::parse_iso).transpose()?;
    let text = spec.text.as_deref().filter(|t| !t.trim().is_empty());
    let terms = match (text, spec.exact) {
        (Some(t), false) => query_terms(t),
        _ => vec![],
    };

    let mut scope = h.owner_docs(owner);
    if !convs.is_empty() {
        let mut in_convs = RoaringBitmap::new();
        for c in &convs {
            if let Some(m) = h.conversation(owner, c) {
                in_convs.extend(m.values().copied());
            }
        }
        scope &= in_convs;
    }
    let passes = |ev: &ConversationEvent| {
        let t = ev.occurred_at();
        visible(ctx, ev)
            && (spec.kinds.is_empty() || spec.kinds.contains(&ev.fields.kind))
            && (spec.origins.is_empty() || spec.origins.contains(&ev.fields.origin))
            && (spec.call_id.is_none() || ev.fields.call_id == spec.call_id)
            && from.is_none_or(|f| t >= f)
            && to.is_none_or(|x| t < x)
    };
    // 本文を索引できなかったイベントは、本文検索の候補から漏れる。件数を返して「無かった」と誤認させない。
    let mut not_searchable = 0u64;
    let mut cand = scope.clone();
    if let Some(t) = text {
        let toks: Vec<String> = if spec.exact { gram_query_tokens(t) } else { terms.iter().flat_map(|x| gram_query_tokens(x)).collect() };
        if let Some(bm) = h.text_candidates(toks) {
            cand &= bm;
        }
        for d in scope.iter() {
            if h.unindexed(d).is_some() && h.get(d).is_some_and(passes) {
                not_searchable += 1;
            }
        }
    }
    let mut keyed: Vec<(i64, u32)> = cand.iter().filter_map(|d| h.get(d).filter(|e| passes(e)).map(|e| (sort_key(spec.order, e), d))).collect();
    keyed.sort_unstable();
    if spec.order == HistoryOrder::Newest {
        keyed.reverse();
    }
    let after = spec.cursor.as_deref().map(|c| parse_cursor(spec.order, c)).transpose()?;
    let begin = match after {
        Some(c) if spec.order == HistoryOrder::Newest => keyed.partition_point(|k| *k >= c),
        Some(c) => keyed.partition_point(|k| *k <= c),
        None => 0,
    };

    let mut hits = vec![];
    let mut last: Option<(i64, u32)> = None;
    let mut unreadable = 0u64;
    let mut stopped_early = false;
    for &(k, d) in &keyed[begin..] {
        if hits.len() >= limit {
            stopped_early = true;
            break;
        }
        if ctx.out_of_budget() {
            stopped_early = true;
            break;
        }
        last = Some((k, d));
        let Some(ev) = h.get(d) else { continue };
        let content = match event_content(ctx, ev)? {
            Ok(b) => String::from_utf8(b).ok(),
            Err(_) => None,
        };
        let mut j = event_json(ctx, d, ev);
        match (text, content.as_deref()) {
            (Some(q), Some(s)) => {
                let found = if spec.exact { s.find(q).map(|at| (at, at + q.len())) } else { MappedText::new(s).find_all(&terms) };
                let Some((ms, me)) = found else { continue };
                j["match"] = json!({ "unit": "byte", "start": ms, "end": me });
                j["excerpt"] = excerpt(s, ms, me, 60);
            }
            (Some(_), None) => {
                unreadable += 1;
                continue;
            }
            (None, Some(s)) => j["excerpt"] = excerpt(s, 0, 0, 160),
            (None, None) => j["excerpt"] = Json::Null,
        }
        hits.push(j);
    }
    if not_searchable + unreadable > 0 {
        ctx.warn(format!(
            "{} event(s) in scope could not be searched by text (not UTF-8, shredded or missing); no match does not prove that nothing was said",
            not_searchable.max(unreadable)
        ));
    }
    let next_cursor = if stopped_early { last.map(|(k, d)| format!("{}:{k}:{d}", spec.order.tag())) } else { None };
    Ok(json!({
        "owner": owner,
        "events": hits,
        "has_more": next_cursor.is_some(),
        "next_cursor": next_cursor,
        "candidates": keyed.len(),
        "not_text_searchable": not_searchable,
        "index": index_status(ctx),
    }))
}

pub(super) fn get(ctx: &QCtx, event: &str, owner: Option<&str>, offset: u64, length: Option<u64>, encoding: ContentEncoding) -> Result<Json> {
    let h = &ctx.kb.history;
    let owner = owner_scope(ctx, owner);
    let (doc, ev) = find_event(ctx, owner, event)?;
    let content = match event_content(ctx, ev)? {
        Ok(bytes) => {
            let mut page = content_page(&bytes, offset, length, encoding)?;
            // 取得したページをつなげた結果を照合するための、原文全体のハッシュ。
            page["content_hash"] = json!(content_hash(&bytes));
            page
        }
        Err(why) => json!({ "withheld": why.as_str() }),
    };
    let brief = |id: &String| match h.event(owner, id).filter(|e| visible(ctx, e)) {
        Some(e) => {
            json!({ "event_id": e.event_id, "sequence": e.fields.sequence, "kind": e.fields.kind, "origin": e.fields.origin, "status": e.fields.status })
        }
        None => json!({ "event_id": id, "missing": true }),
    };
    let peers = |docs: &[u32]| docs.iter().filter(|d| **d != doc).filter_map(|d| h.get(*d)).map(|e| brief(&e.event_id)).collect::<Vec<_>>();
    Ok(json!({
        "event": event_json(ctx, doc, ev),
        "content": content,
        "related": {
            "call": peers(h.call_peers(ev)),
            "superseded_by": peers(h.superseded_by(ev)),
            "supersedes": ev.fields.supersedes.as_ref().map(brief),
            "parent": ev.fields.parent_event.as_ref().map(brief),
            "derived_from": ev.fields.derived_from.iter().map(brief).collect::<Vec<_>>(),
        },
    }))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn context(
    ctx: &QCtx,
    event: Option<&str>,
    conversation: Option<&str>,
    sequence: Option<u64>,
    owner: Option<&str>,
    before: usize,
    after: usize,
    max_content_bytes: u64,
) -> Result<Json> {
    let h = &ctx.kb.history;
    let owner = owner_scope(ctx, owner);
    let (conv, anchor): (String, Option<u64>) = match (event, conversation) {
        (Some(e), _) => {
            let (_, ev) = find_event(ctx, owner, e)?;
            (ev.fields.conversation.clone(), Some(ev.fields.sequence))
        }
        (None, Some(c)) => (c.to_string(), sequence),
        (None, None) => return Err(Error::invalid("history_context needs `event`, or `conversation` (and optionally `sequence`)")),
    };
    let not_found = || Error::not_found(format!("conversation `{conv}`"));
    let m = h.conversation(owner, &conv).ok_or_else(not_found)?;
    if !m.values().next().and_then(|d| h.get(*d)).is_some_and(|e| visible(ctx, e)) {
        return Err(not_found());
    }
    let before = before.min(200);
    let after = after.min(200);
    // anchor が無ければ会話の末尾（最新の `before` 件）。
    type Window = Vec<(u64, u32)>;
    let (mut prev, next): (Window, Window) = match anchor {
        Some(s) => (m.range(..s).rev().take(before).map(|(k, v)| (*k, *v)).collect(), m.range(s..).take(after + 1).map(|(k, v)| (*k, *v)).collect()),
        None => (m.iter().rev().take(before).map(|(k, v)| (*k, *v)).collect(), vec![]),
    };
    let has_more_before = prev.last().is_some_and(|(k, _)| m.range(..*k).next().is_some());
    let has_more_after = next.last().is_some_and(|(k, _)| m.range(k + 1..).next().is_some());
    prev.reverse();
    let window: Vec<(u64, u32)> = prev.into_iter().chain(next).collect();
    let max_content_bytes = max_content_bytes.min(DEFAULT_PAGE_BYTES);
    let mut events = vec![];
    for &(_, d) in &window {
        let Some(ev) = h.get(d) else { continue };
        let mut j = event_json(ctx, d, ev);
        if max_content_bytes > 0 {
            j["content"] = match event_content(ctx, ev)? {
                Ok(b) => content_page(&b, 0, Some(max_content_bytes), ContentEncoding::Auto)?,
                Err(why) => json!({ "withheld": why.as_str() }),
            };
        }
        events.push(j);
    }
    // 記録順の欠け（未同期・未送信のイベントがある可能性）。
    let gaps: Vec<Json> = window.windows(2).filter(|w| w[1].0 > w[0].0 + 1).map(|w| json!({ "from": w[0].0 + 1, "to": w[1].0 - 1 })).collect();
    if !gaps.is_empty() {
        ctx.warn("the sequence has gaps; some events may not have been recorded (or synchronized) yet");
    }
    Ok(json!({
        "owner": owner,
        "conversation": conv,
        "anchor_sequence": anchor,
        "anchor_found": anchor.is_none_or(|s| m.contains_key(&s)),
        "events": events,
        "gaps": gaps,
        "has_more_before": has_more_before,
        "has_more_after": has_more_after,
        "index": index_status(ctx),
    }))
}

pub(super) fn conversations(ctx: &QCtx, owner: Option<&str>, limit: usize) -> Result<Json> {
    let h = &ctx.kb.history;
    let owner = owner_scope(ctx, owner);
    let mut out: Vec<(Tick, Json)> = vec![];
    for (conv, m) in h.conversations_of(owner) {
        let evs: Vec<&ConversationEvent> = m.values().filter_map(|d| h.get(*d)).collect();
        let Some(first) = evs.first().filter(|e| visible(ctx, e)) else { continue };
        let last = evs.last().expect("non-empty");
        let latest = evs.iter().map(|e| e.occurred_at()).max().unwrap_or(first.recorded_at);
        let src = ctx.kb.store.sources.get(&first.source);
        let expected = last.fields.sequence.saturating_sub(first.fields.sequence) + 1;
        out.push((
            latest,
            json!({
                "conversation": conv,
                "title": src.and_then(|s| s.title.clone()),
                "source": first.source,
                "visibility": src.map(|s| &s.visibility),
                "events": evs.len(),
                "first_sequence": first.fields.sequence,
                "last_sequence": last.fields.sequence,
                "missing_sequences": expected - evs.len() as u64,
                "first_at": evs.iter().map(|e| e.occurred_at()).min().map(Tick::to_iso),
                "last_at": latest.to_iso(),
            }),
        ));
    }
    out.sort_by_key(|x| std::cmp::Reverse(x.0));
    let total = out.len();
    Ok(json!({
        "owner": owner,
        "conversations": out.into_iter().take(limit.clamp(1, 1000)).map(|(_, j)| j).collect::<Vec<_>>(),
        "total": total,
        "index": index_status(ctx),
    }))
}
