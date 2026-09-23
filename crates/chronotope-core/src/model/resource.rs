use super::security::{ActorRef, Visibility};
use crate::time::Tick;
use crate::{ResourceId, RevisionId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LabelKind {
    #[default]
    Preferred,
    Alias,
    Former,
    Abbreviation,
}

/// 多言語ラベル。`lang` は BCP 47（`ja`, `en`, `ja-Latn` など）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Label {
    pub text: String,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub kind: LabelKind,
}

impl Label {
    pub fn preferred(text: &str, lang: Option<&str>) -> Self {
        Label { text: text.into(), lang: lang.map(Into::into), kind: LabelKind::Preferred }
    }
    pub fn alias(text: &str, lang: Option<&str>) -> Self {
        Label { text: text.into(), lang: lang.map(Into::into), kind: LabelKind::Alias }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LocalizedText {
    pub text: String,
    #[serde(default)]
    pub lang: Option<String>,
}

/// 外部 ID（`wikidata:Q1490`, `isbn:...`, `twitter:...`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExternalId {
    pub scheme: String,
    pub value: String,
}

impl ExternalId {
    pub fn parse(s: &str) -> Option<Self> {
        let (scheme, value) = s.split_once(':')?;
        (!scheme.is_empty() && !value.is_empty()).then(|| ExternalId { scheme: scheme.to_ascii_lowercase(), value: value.into() })
    }
}

/// 最上位概念。Event / Entity / Place / Work / Document / Post / Dataset / Concept ...
///
/// 型（`types`）は ResourceType 関係として複数持てる（東京駅 = Place + Building + Station + TransportFacility）。
/// 出典で食い違う・時間で変わる・版で分岐する意味情報はここに置かず Assertion にする。
/// ラベル・外部 ID は Identity Resolution の索引として列に保持する。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    #[serde(default)]
    pub types: BTreeSet<ResourceId>,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub descriptions: Vec<LocalizedText>,
    #[serde(default)]
    pub external_ids: Vec<ExternalId>,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub license: Option<String>,
    pub created_at: Tick,
    pub created_by: ActorRef,
    pub created_revision: RevisionId,
}

impl Resource {
    /// 指定言語の優先ラベル。無ければ言語指定なし → 任意の優先ラベル → 任意のラベルの順に落とす。
    pub fn label(&self, lang: Option<&str>) -> Option<&str> {
        let pref = |l: &&Label| l.kind == LabelKind::Preferred;
        let lang_match = |l: &&Label| lang.is_some() && l.lang.as_deref() == lang;
        self.labels
            .iter()
            .filter(pref)
            .find(lang_match)
            .or_else(|| self.labels.iter().find(lang_match))
            .or_else(|| self.labels.iter().filter(pref).find(|l| l.lang.is_none()))
            .or_else(|| self.labels.iter().find(pref))
            .or_else(|| self.labels.first())
            .map(|l| l.text.as_str())
    }

    pub fn description(&self, lang: Option<&str>) -> Option<&str> {
        self.descriptions.iter().find(|d| lang.is_some() && d.lang.as_deref() == lang).or_else(|| self.descriptions.first()).map(|d| d.text.as_str())
    }
}
