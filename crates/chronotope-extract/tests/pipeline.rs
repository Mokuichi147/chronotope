use chronotope_core::model::{ActorKind, ActorRef, Principal};
use chronotope_core::time::Tick;
use chronotope_engine::{KbConfig, KnowledgeBase, WriteRequest};
use chronotope_extract::link::LinkDecision;
use chronotope_extract::{Document, Extraction, IngestOptions, RuleExtractor, ingest};
use serde_json::{Value, json};

const ARTICLE: &str = "【2026年9月21日】20日午後3時半ごろ、東京駅丸の内口の広場で防災イベントが開かれ、主催した東京都によると約1200人が参加した。会場には防災担当の山田花子大臣も姿を見せた。";

fn curator() -> Principal {
    Principal::curator("alice")
}

fn crawler() -> Principal {
    Principal { actor: ActorRef { id: "news-crawler".into(), kind: ActorKind::Crawler }, groups: Default::default(), curator: false }
}

fn w(kb: &mut KnowledgeBase, body: Value) -> Value {
    kb.write(&curator(), serde_json::from_value::<WriteRequest>(body).unwrap()).unwrap()
}

fn q(kb: &KnowledgeBase, body: Value) -> Value {
    serde_json::to_value(kb.query_json(&crawler(), &body.to_string()).unwrap()).unwrap()
}

/// 日本 > 東京 > 東京駅 と、同名の人物 2 人がいる KB。
fn kb() -> (KnowledgeBase, String) {
    let mut kb = KnowledgeBase::in_memory(KbConfig::default());
    kb.set_clock(|| Tick::from_civil(2026, 9, 23, 0, 0, 0, 0));
    let mk = |kb: &mut KnowledgeBase, types: &[&str], label: &str| {
        w(kb, json!({ "op": "create_resource", "resource": { "types": types, "label": label, "lang": "ja" } }))["id"].as_str().unwrap().to_string()
    };
    let japan = mk(&mut kb, &["Country"], "日本");
    let tokyo = mk(&mut kb, &["City"], "東京");
    let station = mk(&mut kb, &["Station"], "東京駅");
    for (s, o) in [(&tokyo, &japan), (&station, &tokyo)] {
        w(&mut kb, json!({ "op": "propose_assertion", "subject": s, "predicate": "located_in", "object": { "resource": o }, "status": "accepted" }));
    }
    mk(&mut kb, &["Person"], "山田花子");
    mk(&mut kb, &["Person"], "山田花子");
    kb.materialize_all().unwrap();
    (kb, station)
}

fn doc() -> Document {
    serde_json::from_value(json!({ "text": ARTICLE, "url": "https://news.example.jp/2026/09/21/bousai", "title": "東京駅で防災イベント", "acquired_at": "2026-09-21T03:10:00Z", "license": "proprietary" })).unwrap()
}

#[test]
fn rule_extraction_is_linked_and_ingested() {
    let (mut kb, station) = kb();
    let x = RuleExtractor::from_kb(&kb).extract(&doc());

    // dry-run は何も書き込まない
    let before = kb.store().head_seq;
    let plan = ingest(&mut kb, &crawler(), &doc(), &x, &IngestOptions { dry_run: true }).unwrap();
    assert_eq!(kb.store().head_seq, before);
    assert_eq!(plan.proposed_predicates, vec!["organized_by"]);

    let r = ingest(&mut kb, &crawler(), &doc(), &x, &IngestOptions::default()).unwrap();
    let outcome = |label: &str| r.entities.iter().find(|e| e.label == label).unwrap_or_else(|| panic!("{label} missing: {:#?}", r.entities)).clone();
    assert_eq!(outcome("東京駅").decision, LinkDecision::Existing { resource: station.parse().unwrap(), via: "extractor" });
    assert!(matches!(outcome("山田花子").decision, LinkDecision::Ambiguous { ref candidates } if candidates.len() == 2));
    assert_eq!(outcome("東京都").decision, LinkDecision::New);
    assert!(r.warnings.iter().any(|w| w.contains("organized_by")));
    assert!(r.warnings.iter().any(|w| w.contains("possibly_same_as")));
    kb.materialize_all().unwrap();

    // 見出しの日付（公開日）を基準に「20日」を解決し、階層をたどって「日本国内」で見つかる
    let s = q(
        &kb,
        json!({ "op": "search", "budget_ms": 500, "types": ["Event"], "time": { "expression": "2026-09-20" }, "space": { "within_place": outcome("東京駅").resource.unwrap() } }),
    );
    let hit = &s["results"][0];
    assert_eq!(hit["label"], "東京駅の防災イベント");
    assert_eq!(hit["time"]["earliest_start"], "2026-09-20T15:00:00+09:00");
    assert_eq!(hit["time"]["latest_end"], "2026-09-20T16:01:00+09:00");

    let event = outcome("東京駅の防災イベント").resource.unwrap();
    let c = q(&kb, json!({ "op": "expand_claims", "budget_ms": 500, "id": event, "include_incoming": true }));
    let preds: Vec<&str> = c["results"]["claims"].as_array().unwrap().iter().map(|g| g["predicate"].as_str().unwrap()).collect();
    assert!(preds.contains(&"occurred_at") && preds.contains(&"took_place_at") && preds.contains(&"organized_by"), "{preds:?}");
    assert_eq!(c["results"]["incoming"].as_array().unwrap().len(), 1, "participant");
    assert_eq!(c["results"]["observation_metrics"][0], "attendees");
    // 規則ベース（モデルなし）なので AI 抽出のみ扱いにはならない
    let occ = c["results"]["claims"].as_array().unwrap().iter().find(|g| g["predicate"] == "occurred_at").unwrap()["preferred"].clone();
    assert_ne!(occ["tier"], "ai_only");
    let prov = q(&kb, json!({ "op": "get_acquisition", "budget_ms": 500, "assertion": occ["id"] }));
    let e0 = &prov["results"]["evidence"][0];
    assert_eq!(e0["derivation"]["extractor"], "chronotope-rules");
    let span = &e0["derivation"]["source_span"];
    let chars: Vec<char> = ARTICLE.chars().collect();
    let cited: String = chars[span["start"].as_u64().unwrap() as usize..span["end"].as_u64().unwrap() as usize].iter().collect();
    assert_eq!(cited, "20日午後3時半ごろ", "span points at the original text");
    assert_eq!(e0["acquisition"]["acquired_at"], "2026-09-21T03:10:00Z");
    // 同名人物は統合されず、候補として残る
    let person = outcome("山田花子").resource.unwrap();
    let l = q(&kb, json!({ "op": "lookup", "budget_ms": 50, "id": person }));
    assert_eq!(l["results"]["matches"][0]["possibly_same_as"].as_array().unwrap().len(), 2);
}

#[test]
fn external_extraction_json_is_accepted() {
    let (mut kb, _) = kb();
    // 人手やローカル LLM など任意の抽出器の出力（同じ中間形式）
    let x: Extraction = serde_json::from_value(json!({
        "extractor": { "name": "local-llm-extractor", "model": "some-local-model", "model_version": "q4" },
        "source_time": "2026-09-21",
        "entities": [
            { "ref": "E1", "types": ["Event"], "label": "東京駅の防災イベント" },
            { "ref": "P1", "types": ["Station"], "label": "東京駅", "mention": "東京駅丸の内口の広場" }
        ],
        "claims": [
            { "subject": "E1", "predicate": "occurred_at", "object": { "time": "20日午後3時半ごろ" }, "span": [12, 22], "confidence": 0.9 },
            { "subject": "E1", "predicate": "took_place_at", "object": { "ref": "P1" }, "span": [23, 33], "confidence": 0.8 }
        ]
    }))
    .unwrap();
    let mut d = doc();
    d.acquired_at = Some("2026-09-21T04:00:00Z".into());
    let r = ingest(&mut kb, &crawler(), &d, &x, &IngestOptions::default()).unwrap();
    assert!(matches!(r.entities[1].decision, LinkDecision::Existing { via: "label", .. }));
    kb.materialize_all().unwrap();
    let event = r.entities[0].resource.clone().unwrap();
    let c = q(&kb, json!({ "op": "expand_claims", "budget_ms": 500, "id": event }));
    let occ = c["results"]["claims"].as_array().unwrap().iter().find(|g| g["predicate"] == "occurred_at").unwrap()["preferred"].clone();
    assert_eq!(occ["tier"], "ai_only", "model-based extraction is ranked as AI-only until corroborated");

    // 参照先の無い主張は取り込み前に拒否する
    let bad: Extraction = serde_json::from_value(
        json!({ "extractor": { "name": "x" }, "claims": [{ "subject": "E9", "predicate": "occurred_at", "object": { "time": "2026" } }] }),
    )
    .unwrap();
    assert!(ingest(&mut kb, &crawler(), &d, &bad, &IngestOptions::default()).is_err());
}

#[test]
fn places_under_a_known_parent_link_only_within_it() {
    let mut kb = KnowledgeBase::in_memory(KbConfig::default());
    let mk = |kb: &mut KnowledgeBase, types: &[&str], label: &str| {
        w(kb, json!({ "op": "create_resource", "resource": { "types": types, "label": label, "lang": "ja" } }))["id"].as_str().unwrap().to_string()
    };
    let (a_pref, a_city, b_pref, b_ward) =
        (mk(&mut kb, &["Region"], "山川県"), mk(&mut kb, &["City"], "海辺市"), mk(&mut kb, &["Region"], "谷原県"), mk(&mut kb, &["City"], "本町"));
    for (s, o) in [(&a_city, &a_pref), (&b_ward, &b_pref)] {
        w(&mut kb, json!({ "op": "propose_assertion", "subject": s, "predicate": "located_in", "object": { "resource": o }, "status": "accepted" }));
    }
    // 谷原県の本町ではなく、海辺市の配下の（KB に無い）本町
    let x: Extraction = serde_json::from_value(json!({
        "extractor": { "name": "t" },
        "entities": [
            { "ref": "E1", "types": ["Event"], "label": "火災" },
            { "ref": "P1", "types": ["City"], "label": "海辺市", "resource": a_city },
            { "ref": "P2", "types": ["City"], "label": "本町" }
        ],
        "claims": [
            { "subject": "E1", "predicate": "took_place_at", "object": { "ref": "P2" } },
            { "subject": "P2", "predicate": "located_in", "object": { "ref": "P1" } }
        ]
    }))
    .unwrap();
    let d: Document = serde_json::from_value(json!({ "text": "海辺市本町で火災が発生した。", "acquired_at": "2026-01-01T00:00:00Z" })).unwrap();
    let r = ingest(&mut kb, &crawler(), &d, &x, &IngestOptions { dry_run: true }).unwrap();
    assert_eq!(r.entities[2].decision, LinkDecision::New, "{:?}", r.entities[2]);
}

#[test]
fn works_with_the_real_clock() {
    // 実時計の取得・抽出時刻はミリ秒を含む。固定時計のテストでは見えない失敗を防ぐ。
    let mut kb = KnowledgeBase::in_memory(KbConfig::default());
    let d: Document = serde_json::from_value(json!({ "text": "2026年9月20日、東京駅で防災イベントが開かれた。" })).unwrap();
    let x = RuleExtractor::from_kb(&kb).extract(&d);
    let r = ingest(&mut kb, &crawler(), &d, &x, &IngestOptions::default()).unwrap();
    assert!(r.warnings.iter().all(|w| !w.contains("skipped")), "{:?}", r.warnings);
    assert_eq!(r.assertions.len(), 2, "occurred_at + took_place_at");
}
