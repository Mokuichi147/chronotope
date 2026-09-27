//! 会話履歴（[`ConversationEvent`]）の保持と本文索引。
//!
//! イベントは Revision ログの `record_event` Command から再生され、コミットと同時に
//! 本文の索引まで更新する（Projection のような結果整合ではない）。そのため保存直後の
//! 検索でも索引の遅れは無く、索引できなかったイベント（UTF-8 でない本文・破棄された鍵・
//! 欠落したスナップショット）は理由付きで別に数え、検索結果から「存在しない」と誤認させない。

use crate::text::{TextIndex, gram_tokens};
use chronotope_core::KeyId;
use chronotope_core::model::ConversationEvent;
use roaring::RoaringBitmap;
use std::collections::{BTreeMap, HashMap};

/// 本文を索引できなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unindexed {
    NotUtf8,
    Shredded,
    Missing,
}

impl Unindexed {
    pub fn as_str(self) -> &'static str {
        match self {
            Unindexed::NotUtf8 => "content is not UTF-8 text",
            Unindexed::Shredded => "content key has been shredded",
            Unindexed::Missing => "snapshot object is missing",
        }
    }
}

/// (所有者, 会話 ID)。
pub type ConversationKey = (String, String);

#[derive(Default)]
pub struct HistoryStore {
    events: Vec<ConversationEvent>,
    by_event: HashMap<(String, String), u32>,
    by_owner: HashMap<String, RoaringBitmap>,
    by_conversation: HashMap<ConversationKey, BTreeMap<u64, u32>>,
    /// (所有者, 会話 ID, call_id) → 呼び出しと結果。
    by_call: HashMap<(String, String, String), Vec<u32>>,
    /// (所有者, 訂正対象の event_id) → 訂正したイベント。
    superseded_by: HashMap<(String, String), Vec<u32>>,
    by_key: HashMap<KeyId, Vec<u32>>,
    text: TextIndex,
    unindexed: HashMap<u32, Unindexed>,
}

impl HistoryStore {
    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ConversationEvent> {
        self.events.iter()
    }

    pub fn get(&self, doc: u32) -> Option<&ConversationEvent> {
        self.events.get(doc as usize)
    }

    pub fn doc(&self, owner: &str, event_id: &str) -> Option<u32> {
        self.by_event.get(&(owner.to_string(), event_id.to_string())).copied()
    }

    pub fn event(&self, owner: &str, event_id: &str) -> Option<&ConversationEvent> {
        self.doc(owner, event_id).and_then(|d| self.get(d))
    }

    /// 会話内の同じ記録順を持つイベント。
    pub fn at_sequence(&self, owner: &str, conversation: &str, sequence: u64) -> Option<u32> {
        self.conversation(owner, conversation).and_then(|m| m.get(&sequence).copied())
    }

    pub fn conversation(&self, owner: &str, conversation: &str) -> Option<&BTreeMap<u64, u32>> {
        self.by_conversation.get(&(owner.to_string(), conversation.to_string()))
    }

    pub fn conversations_of(&self, owner: &str) -> impl Iterator<Item = (&str, &BTreeMap<u64, u32>)> {
        self.by_conversation.iter().filter(move |((o, _), _)| o == owner).map(|((_, c), m)| (c.as_str(), m))
    }

    pub fn owner_docs(&self, owner: &str) -> RoaringBitmap {
        self.by_owner.get(owner).cloned().unwrap_or_default()
    }

    pub fn call_peers(&self, ev: &ConversationEvent) -> &[u32] {
        match &ev.fields.call_id {
            Some(c) => self.by_call.get(&(ev.owner.clone(), ev.fields.conversation.clone(), c.clone())).map(Vec::as_slice).unwrap_or(&[]),
            None => &[],
        }
    }

    pub fn superseded_by(&self, ev: &ConversationEvent) -> &[u32] {
        self.superseded_by.get(&(ev.owner.clone(), ev.event_id.clone())).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn unindexed(&self, doc: u32) -> Option<Unindexed> {
        self.unindexed.get(&doc).copied()
    }

    /// トークンをすべて含む文書。トークンが無ければ None（絞り込まない）。
    pub fn text_candidates(&self, toks: Vec<String>) -> Option<RoaringBitmap> {
        self.text.search_tokens(toks)
    }

    /// イベントを追加する。`content` は索引する本文（索引できなければ理由）。
    pub fn insert(&mut self, ev: ConversationEvent, content: Result<&str, Unindexed>, key: Option<KeyId>) -> u32 {
        let doc = self.events.len() as u32;
        let owner = ev.owner.clone();
        let conv = ev.fields.conversation.clone();
        self.by_event.insert((owner.clone(), ev.event_id.clone()), doc);
        self.by_owner.entry(owner.clone()).or_default().insert(doc);
        self.by_conversation.entry((owner.clone(), conv.clone())).or_default().insert(ev.fields.sequence, doc);
        if let Some(c) = &ev.fields.call_id {
            self.by_call.entry((owner.clone(), conv, c.clone())).or_default().push(doc);
        }
        if let Some(s) = &ev.fields.supersedes {
            self.superseded_by.entry((owner, s.clone())).or_default().push(doc);
        }
        if let Some(k) = key {
            self.by_key.entry(k).or_default().push(doc);
        }
        match content {
            Ok(text) => self.text.set_tokens(doc, gram_tokens(text)),
            Err(why) => {
                self.unindexed.insert(doc, why);
            }
        }
        self.events.push(ev);
        doc
    }

    /// 鍵の破棄に合わせて、その鍵で暗号化した本文を索引から外す。
    pub fn shred(&mut self, key: KeyId) {
        for doc in self.by_key.get(&key).cloned().unwrap_or_default() {
            self.text.remove(doc);
            self.unindexed.insert(doc, Unindexed::Shredded);
        }
    }
}
