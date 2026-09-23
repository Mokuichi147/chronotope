//! 組み込み語彙（型・述語）と外部標準への Mapping。
//! ID は `type:<key>` / `predicate:<key>` から決定的に導出するため、どのノードでも同じ ID になる。

use crate::ResourceId;
use crate::model::{Label, LiteralKind, MatchKind, PredicateDef, PredicateRange, PredicateRole, TypeDef, VocabMapping, VocabStatus};

pub fn type_id(key: &str) -> ResourceId {
    ResourceId::named(&format!("type:{key}"))
}

pub fn predicate_id(key: &str) -> ResourceId {
    ResourceId::named(&format!("predicate:{key}"))
}

/// よく使う述語キー。
pub mod keys {
    pub const INSTANCE_OF: &str = "instance_of";
    pub const OCCURRED_AT: &str = "occurred_at";
    pub const START_TIME: &str = "start_time";
    pub const END_TIME: &str = "end_time";
    pub const PARTICIPATED_IN: &str = "participated_in";
    pub const TOOK_PLACE_AT: &str = "took_place_at";
    pub const LOCATED_IN: &str = "located_in";
    pub const CONTAINS: &str = "contains";
    pub const INSIDE: &str = "inside";
    pub const COORDINATES: &str = "coordinates";
    pub const SAME_AS: &str = "same_as";
    pub const POSSIBLY_SAME_AS: &str = "possibly_same_as";
    pub const DISTINCT_FROM: &str = "distinct_from";
    pub const PART_OF_WORK: &str = "part_of_work";
    pub const APPEARS_IN: &str = "appears_in";
    pub const ADAPTATION_OF: &str = "adaptation_of";
    pub const DIVERGES_FROM: &str = "diverges_from";
}

fn m(vocabulary: &str, iri: &str, match_kind: MatchKind) -> VocabMapping {
    VocabMapping { vocabulary: vocabulary.into(), iri: iri.into(), match_kind }
}

/// (key, 親, 日本語ラベル, 英語ラベル, wikidata)
type TypeSpec = (&'static str, &'static [&'static str], &'static str, &'static str, Option<&'static str>);
const TYPES: &[TypeSpec] = &[
    ("Resource", &[], "リソース", "Resource", None),
    ("Event", &["Resource"], "出来事", "Event", Some("Q1190554")),
    ("Entity", &["Resource"], "実体", "Entity", None),
    ("Person", &["Entity"], "人物", "Person", Some("Q5")),
    ("Character", &["Entity"], "キャラクター", "Fictional character", Some("Q95074")),
    ("Organization", &["Entity"], "組織", "Organization", Some("Q43229")),
    ("Object", &["Entity"], "物体", "Physical object", Some("Q223557")),
    ("Vehicle", &["Object"], "乗り物", "Vehicle", Some("Q42889")),
    ("Place", &["Resource"], "場所", "Place", Some("Q17334923")),
    ("World", &["Place"], "世界", "World", None),
    ("Region", &["Place"], "地域", "Region", Some("Q82794")),
    ("Country", &["Region"], "国", "Country", Some("Q6256")),
    ("City", &["Region"], "都市", "City", Some("Q515")),
    ("Area", &["Place"], "エリア", "Area", None),
    ("Subplace", &["Place"], "サブプレイス", "Subplace", None),
    ("Building", &["Place"], "建物", "Building", Some("Q41176")),
    ("Room", &["Subplace"], "部屋", "Room", Some("Q180516")),
    ("Station", &["Place"], "駅", "Station", Some("Q719456")),
    ("TransportFacility", &["Place"], "交通施設", "Transport facility", None),
    ("Dungeon", &["Area"], "ダンジョン", "Dungeon", None),
    ("VirtualSpace", &["Place"], "仮想空間", "Virtual space", None),
    ("Dream", &["Place"], "夢", "Dream", None),
    ("Dimension", &["World"], "異次元", "Dimension", None),
    ("Work", &["Resource"], "作品", "Creative work", Some("Q17537576")),
    ("Franchise", &["Work"], "フランチャイズ", "Media franchise", Some("Q196600")),
    ("Series", &["Work"], "シリーズ", "Series", Some("Q7725310")),
    ("Season", &["Work"], "シーズン", "Season", Some("Q3464665")),
    ("Episode", &["Work"], "エピソード", "Episode", Some("Q1983062")),
    ("Movie", &["Work"], "映画", "Film", Some("Q11424")),
    ("OVA", &["Work"], "OVA", "Original video animation", Some("Q220898")),
    ("Volume", &["Work"], "巻", "Volume", Some("Q1238720")),
    ("Chapter", &["Work"], "章", "Chapter", Some("Q1980247")),
    ("Game", &["Work"], "ゲーム", "Video game", Some("Q7889")),
    ("Book", &["Work"], "本", "Book", Some("Q571")),
    ("Document", &["Resource"], "文書", "Document", Some("Q49848")),
    ("WebPage", &["Document"], "Webページ", "Web page", Some("Q36774")),
    ("Post", &["Document"], "投稿", "Post", None),
    ("Image", &["Document"], "画像", "Image", Some("Q478798")),
    ("Video", &["Document"], "動画", "Video", Some("Q34508")),
    ("Audio", &["Document"], "音声", "Audio", None),
    ("Dataset", &["Resource"], "データセット", "Dataset", Some("Q1172284")),
    ("Table", &["Dataset"], "表", "Table", None),
    ("Concept", &["Resource"], "概念", "Concept", Some("Q151885")),
    ("Predicate", &["Concept"], "述語", "Predicate", None),
    ("Type", &["Concept"], "型", "Type", None),
    ("Canon", &["Concept"], "正史系統", "Canon", None),
    ("Timeline", &["Concept"], "世界線", "Timeline", None),
];

pub fn builtin_types() -> Vec<TypeDef> {
    TYPES
        .iter()
        .map(|(key, parents, ja, en, wd)| TypeDef {
            id: type_id(key),
            key: key.to_string(),
            labels: vec![Label::preferred(ja, Some("ja")), Label::preferred(en, Some("en"))],
            parents: parents.iter().map(|p| type_id(p)).collect(),
            status: VocabStatus::Accepted,
            mappings: wd.map(|q| vec![m("wikidata", &format!("http://www.wikidata.org/entity/{q}"), MatchKind::Close)]).unwrap_or_default(),
        })
        .collect()
}

struct P {
    key: &'static str,
    ja: &'static str,
    role: PredicateRole,
    range: PredicateRange,
    inverse: Option<&'static str>,
    transitive: bool,
    symmetric: bool,
    functional: bool,
    allen: Option<&'static str>,
    maps: &'static [(&'static str, &'static str, MatchKind)],
}

const fn p(key: &'static str, ja: &'static str, role: PredicateRole) -> P {
    P { key, ja, role, range: PredicateRange::Any, inverse: None, transitive: false, symmetric: false, functional: false, allen: None, maps: &[] }
}

const WD: &str = "wikidata";
const OT: &str = "owl-time";
const CRM: &str = "cidoc-crm";
const OWL: &str = "owl";

fn predicates_spec() -> Vec<P> {
    use MatchKind::*;
    use PredicateRole::*;
    let res = || PredicateRange::Resource { types: vec![] };
    let time = || PredicateRange::Literal { literal: LiteralKind::Time };
    let qty = || PredicateRange::Literal { literal: LiteralKind::Quantity };
    let temporal = |key: &'static str, ja: &'static str, allen: &'static str, inv: &'static str, iri: &'static str| P {
        range: res(),
        allen: Some(allen),
        inverse: Some(inv),
        transitive: matches!(allen, "before" | "after" | "during" | "contains"),
        symmetric: allen == "equals",
        maps: Box::leak(vec![(OT, iri, Exact)].into_boxed_slice()),
        ..p(key, ja, TemporalRelation)
    };
    vec![
        P {
            range: res(),
            maps: &[(WD, "P31", Exact), ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#type", Exact)],
            ..p(keys::INSTANCE_OF, "型", Typing)
        },
        P {
            range: time(),
            functional: true,
            maps: &[(WD, "P585", Close), (OT, "http://www.w3.org/2006/time#hasTime", Close), (CRM, "P4_has_time-span", Close)],
            ..p(keys::OCCURRED_AT, "発生時期", EventTime)
        },
        P {
            range: time(),
            functional: true,
            maps: &[(WD, "P580", Exact), (OT, "http://www.w3.org/2006/time#hasBeginning", Close)],
            ..p(keys::START_TIME, "開始時期", EventTime)
        },
        P {
            range: time(),
            functional: true,
            maps: &[(WD, "P582", Exact), (OT, "http://www.w3.org/2006/time#hasEnd", Close)],
            ..p(keys::END_TIME, "終了時期", EventTime)
        },
        P {
            range: res(),
            inverse: Some("has_participant"),
            maps: &[(WD, "P1344", Exact), (CRM, "P11i_participated_in", Exact)],
            ..p(keys::PARTICIPATED_IN, "参加", General)
        },
        P {
            range: res(),
            inverse: Some(keys::PARTICIPATED_IN),
            maps: &[(WD, "P710", Exact), (CRM, "P11_had_participant", Exact)],
            ..p("has_participant", "参加者", General)
        },
        P { range: res(), maps: &[(WD, "P276", Exact), (CRM, "P7_took_place_at", Exact)], ..p(keys::TOOK_PLACE_AT, "発生場所", Location) },
        P {
            range: res(),
            transitive: true,
            inverse: Some(keys::CONTAINS),
            maps: &[(WD, "P131", Close), (CRM, "P89_falls_within", Exact)],
            ..p(keys::LOCATED_IN, "所在", SpatialContainment)
        },
        P {
            range: res(),
            transitive: true,
            inverse: Some(keys::LOCATED_IN),
            maps: &[(WD, "P150", Close), (CRM, "P89i_contains", Exact)],
            ..p(keys::CONTAINS, "包含", SpatialContainment)
        },
        P {
            range: res(),
            transitive: true,
            inverse: Some(keys::CONTAINS),
            maps: &[(CRM, "P89_falls_within", Close)],
            ..p(keys::INSIDE, "内部", SpatialContainment)
        },
        P { range: res(), symmetric: true, maps: &[(WD, "P47", Close), (CRM, "P122_borders_with", Close)], ..p("adjacent", "隣接", SpatialRelation) },
        P { range: res(), symmetric: true, ..p("connected", "接続", SpatialRelation) },
        P { range: res(), symmetric: true, ..p("near", "近傍", SpatialRelation) },
        P { range: res(), inverse: Some("south_of"), ..p("north_of", "北", SpatialRelation) },
        P { range: res(), inverse: Some("north_of"), ..p("south_of", "南", SpatialRelation) },
        P { range: res(), inverse: Some("west_of"), ..p("east_of", "東", SpatialRelation) },
        P { range: res(), inverse: Some("east_of"), ..p("west_of", "西", SpatialRelation) },
        P { range: PredicateRange::Any, ..p("between", "間", SpatialRelation) },
        P { range: res(), ..p("portal_to", "ポータル", SpatialRelation) },
        P {
            range: PredicateRange::Literal { literal: LiteralKind::Geo },
            functional: true,
            maps: &[(WD, "P625", Exact)],
            ..p(keys::COORDINATES, "座標", Coordinates)
        },
        temporal("before", "より前", "before", "after", "http://www.w3.org/2006/time#before"),
        temporal("after", "より後", "after", "before", "http://www.w3.org/2006/time#after"),
        temporal("meets", "直前", "meets", "met_by", "http://www.w3.org/2006/time#intervalMeets"),
        temporal("met_by", "直後", "met_by", "meets", "http://www.w3.org/2006/time#intervalMetBy"),
        temporal("overlaps", "重複", "overlaps", "overlapped_by", "http://www.w3.org/2006/time#intervalOverlaps"),
        temporal("overlapped_by", "被重複", "overlapped_by", "overlaps", "http://www.w3.org/2006/time#intervalOverlappedBy"),
        temporal("during", "期間中", "during", "temporally_contains", "http://www.w3.org/2006/time#intervalDuring"),
        temporal("temporally_contains", "期間に含む", "contains", "during", "http://www.w3.org/2006/time#intervalContains"),
        temporal("starts", "同時開始", "starts", "started_by", "http://www.w3.org/2006/time#intervalStarts"),
        temporal("started_by", "被同時開始", "started_by", "starts", "http://www.w3.org/2006/time#intervalStartedBy"),
        temporal("finishes", "同時終了", "finishes", "finished_by", "http://www.w3.org/2006/time#intervalFinishes"),
        temporal("finished_by", "被同時終了", "finished_by", "finishes", "http://www.w3.org/2006/time#intervalFinishedBy"),
        temporal("simultaneous", "同時", "equals", "simultaneous", "http://www.w3.org/2006/time#intervalEquals"),
        P {
            range: res(),
            transitive: true,
            inverse: Some("has_part"),
            maps: &[(WD, "P361", Exact), (CRM, "P9i_forms_part_of", Close)],
            ..p("part_of", "一部", General)
        },
        P { range: res(), transitive: true, inverse: Some("part_of"), maps: &[(WD, "P527", Exact)], ..p("has_part", "構成要素", General) },
        P { range: res(), inverse: Some("caused_by"), maps: &[(WD, "P1542", Exact)], ..p("causes", "原因となる", General) },
        P { range: res(), inverse: Some("causes"), maps: &[(WD, "P828", Exact)], ..p("caused_by", "原因", General) },
        P { range: res(), symmetric: true, ..p("alternative_of", "別説", General) },
        P {
            range: res(),
            symmetric: true,
            transitive: true,
            maps: &[(OWL, "http://www.w3.org/2002/07/owl#sameAs", Exact)],
            ..p(keys::SAME_AS, "同一", Identity)
        },
        P { range: res(), symmetric: true, ..p(keys::POSSIBLY_SAME_AS, "同一の可能性", Identity) },
        P {
            range: res(),
            symmetric: true,
            maps: &[(WD, "P1889", Exact), (OWL, "http://www.w3.org/2002/07/owl#differentFrom", Exact)],
            ..p(keys::DISTINCT_FROM, "別物", Identity)
        },
        P { range: qty(), functional: true, maps: &[(WD, "P2048", Exact)], ..p("height", "高さ", General) },
        P { range: qty(), maps: &[(WD, "P1082", Exact)], ..p("population", "人口", General) },
        P { range: time(), functional: true, maps: &[(WD, "P569", Exact)], ..p("birth_date", "生年月日", General) },
        P { range: time(), functional: true, maps: &[(WD, "P570", Exact)], ..p("death_date", "没年月日", General) },
        P { range: PredicateRange::Literal { literal: LiteralKind::Text }, maps: &[(WD, "P2561", Close)], ..p("name", "名称", General) },
        P { range: res(), transitive: true, maps: &[(WD, "P179", Close), (WD, "P361", Close)], ..p(keys::PART_OF_WORK, "所属作品", WorkMembership) },
        P { range: res(), maps: &[(WD, "P1441", Exact)], ..p(keys::APPEARS_IN, "登場作品", WorkMembership) },
        P { range: res(), maps: &[(WD, "P840", Exact)], ..p("set_in", "舞台", Location) },
        P { range: res(), inverse: Some("adapted_as"), maps: &[(WD, "P144", Exact)], ..p(keys::ADAPTATION_OF, "原作", Adaptation) },
        P { range: res(), inverse: Some(keys::ADAPTATION_OF), maps: &[(WD, "P4969", Exact)], ..p("adapted_as", "翻案作品", Adaptation) },
        P { range: res(), inverse: Some("followed_by"), maps: &[(WD, "P155", Exact)], ..p("follows", "前作", General) },
        P { range: res(), inverse: Some("follows"), maps: &[(WD, "P156", Exact)], ..p("followed_by", "次作", General) },
        P { range: res(), maps: &[(WD, "P463", Exact)], ..p("member_of", "所属", General) },
        P { range: res(), maps: &[(WD, "P50", Close), (CRM, "P94i_was_created_by", Close)], ..p("created_by", "作者", General) },
        P { range: res(), ..p(keys::DIVERGES_FROM, "分岐元", General) },
        P { range: res(), maps: &[("prov-o", "http://www.w3.org/ns/prov#wasDerivedFrom", Exact)], ..p("derived_from", "派生元", General) },
    ]
}

pub fn builtin_predicates() -> Vec<PredicateDef> {
    predicates_spec()
        .into_iter()
        .map(|s| PredicateDef {
            id: predicate_id(s.key),
            key: s.key.to_string(),
            labels: vec![Label::preferred(s.ja, Some("ja")), Label::preferred(&s.key.replace('_', " "), Some("en"))],
            domain: vec![],
            range: s.range,
            inverse: s.inverse.map(predicate_id),
            transitive: s.transitive,
            symmetric: s.symmetric,
            functional: s.functional,
            status: VocabStatus::Accepted,
            role: s.role,
            allen: s.allen.map(Into::into),
            mappings: s
                .maps
                .iter()
                .map(|(v, iri, k)| {
                    let iri = if *v == WD && iri.starts_with('P') { format!("http://www.wikidata.org/prop/direct/{iri}") } else { iri.to_string() };
                    m(v, &iri, *k)
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocab_consistency() {
        let preds = builtin_predicates();
        let ids: std::collections::HashSet<_> = preds.iter().map(|p| p.id).collect();
        for p in &preds {
            if let Some(inv) = p.inverse {
                assert!(ids.contains(&inv), "inverse of {} missing", p.key);
            }
            if p.role == PredicateRole::TemporalRelation {
                assert!(crate::time::allen::AllenRelation::from_name(p.allen.as_deref().unwrap()).is_some(), "{}", p.key);
            }
        }
        let types = builtin_types();
        let tids: std::collections::HashSet<_> = types.iter().map(|t| t.id).collect();
        for t in &types {
            for parent in &t.parents {
                assert!(tids.contains(parent));
            }
        }
        assert!(types.len() <= 64, "types_mask uses u64");
    }
}
