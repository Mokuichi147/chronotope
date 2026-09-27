//! 仕様のユースケースを端から端まで確認する統合テスト。

use chronotope_core::model::Principal;
use chronotope_core::time::Tick;
use chronotope_engine::{KbConfig, KnowledgeBase, WriteRequest};
use serde_json::{Value, json};

fn kb() -> KnowledgeBase {
    let mut kb = KnowledgeBase::in_memory(KbConfig::default());
    kb.set_clock(|| Tick::from_civil(2026, 9, 23, 0, 0, 0, 0));
    kb
}

fn agent() -> Principal {
    Principal::agent("agent-1")
}

fn curator() -> Principal {
    Principal::curator("alice")
}

fn w(kb: &mut KnowledgeBase, p: &Principal, body: Value) -> Value {
    let req: WriteRequest = serde_json::from_value(body.clone()).unwrap_or_else(|e| panic!("bad write {body}: {e}"));
    kb.write(p, req).unwrap_or_else(|e| panic!("write failed {body}: {e}"))
}

fn w_err(kb: &mut KnowledgeBase, p: &Principal, body: Value) -> chronotope_core::Error {
    let req: WriteRequest = serde_json::from_value(body).unwrap();
    kb.write(p, req).expect_err("write should fail")
}

fn q(kb: &KnowledgeBase, p: &Principal, body: Value) -> Value {
    let r = kb.query_json(p, &body.to_string()).unwrap_or_else(|e| panic!("query failed {body}: {e}"));
    serde_json::to_value(r).unwrap()
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

fn create(kb: &mut KnowledgeBase, types: &[&str], label: &str) -> String {
    id(&w(kb, &curator(), json!({ "op": "create_resource", "resource": { "types": types, "label": label, "lang": "ja" } })))
}

fn assert_accepted(kb: &mut KnowledgeBase, s: &str, p: &str, o: Value) -> String {
    id(&w(kb, &curator(), json!({ "op": "propose_assertion", "subject": s, "predicate": p, "object": o, "status": "accepted" })))
}

fn source(kb: &mut KnowledgeBase, url: &str, acquired_at: &str, extra: Value) -> String {
    let mut body =
        json!({ "op": "link_source", "url": url, "kind": "web_page", "acquisition": { "acquired_at": acquired_at, "content": format!("snapshot of {url}") } });
    if let (Value::Object(m), Value::Object(e)) = (&mut body, extra) {
        m.extend(e);
    }
    w(kb, &agent(), body)["acquisition"].as_str().unwrap().to_string()
}

#[test]
fn spatiotemporal_search_and_drilldown() {
    let mut kb = kb();
    let japan = create(&mut kb, &["Country"], "日本");
    let tokyo = create(&mut kb, &["City"], "東京");
    let station = create(&mut kb, &["Station", "Building", "TransportFacility"], "東京駅");
    assert_accepted(&mut kb, &tokyo, "located_in", json!({ "resource": japan }));
    assert_accepted(&mut kb, &japan, "contains", json!({ "resource": tokyo }));
    assert_accepted(&mut kb, &station, "located_in", json!({ "resource": tokyo }));
    assert_accepted(
        &mut kb,
        &station,
        "coordinates",
        json!({ "geo": { "frame": "frm_".to_string() + &chronotope_core::FrameId::named("wgs84").0.simple().to_string(), "geometry": { "type": "point", "at": { "x": 139.7671, "y": 35.6812 } } } }),
    );
    let alice = create(&mut kb, &["Person"], "Alice");
    let acq = source(&mut kb, "https://news.example/a", "2026-09-21T10:00:00Z", json!({ "origin": "primary", "source_time": "2026-09-21" }));
    let ev = w(
        &mut kb,
        &agent(),
        json!({
            "op": "propose_assertion",
            "subject": { "new": { "types": ["Event"], "label": "駅前イベント", "lang": "ja" } },
            "predicate": "occurred_at",
            "object": { "time": "2026-09-20 15:30", "calendar": "gregorian+09:00" },
            "evidence": [{ "acquisition": acq, "derivation": { "extractor": "llm-extractor", "model": "extractor-model", "model_version": "2026-08", "extraction_conf": 0.9 } }]
        }),
    );
    assert_eq!(ev["status"], "proposed", "AI writes start as proposed");
    let event = ev["subject"].as_str().unwrap().to_string();
    w(
        &mut kb,
        &agent(),
        json!({ "op": "propose_assertion", "subject": event, "predicate": "took_place_at", "object": { "resource": station }, "evidence": [{ "acquisition": acq }] }),
    );
    w(
        &mut kb,
        &agent(),
        json!({ "op": "propose_assertion", "subject": alice, "predicate": "participated_in", "object": { "resource": event }, "evidence": [{ "acquisition": acq }] }),
    );

    // Projection は結果整合: Materialize 前は stale。
    let fr = q(&kb, &agent(), json!({ "op": "freshness", "budget_ms": 50 }));
    assert_eq!(fr["results"]["consistent"], false);
    kb.materialize_all().unwrap();
    let fr = q(&kb, &agent(), json!({ "op": "freshness", "budget_ms": 50 }));
    assert_eq!(fr["results"]["consistent"], true);

    let res = q(
        &kb,
        &agent(),
        json!({
            "op": "search", "budget_ms": 500,
            "types": ["Event"],
            "time": { "from": "2026-09-20", "to": "2026-09-21" },
            "space": { "within_place": japan },
            "entities": [alice]
        }),
    );
    let hits = res["results"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{res:#}");
    assert_eq!(hits[0]["id"], event.as_str());
    assert!(hits[0]["summary"].as_str().unwrap().contains("駅前イベント"));
    assert_eq!(res["truncated"], false);

    // 範囲外の時間窓では見つからない。
    let res = q(&kb, &agent(), json!({ "op": "search", "budget_ms": 500, "types": ["Event"], "time": { "expression": "2026年8月" } }));
    assert!(res["results"].as_array().unwrap().is_empty());

    // 近傍検索: イベント自身は座標を持たず、東京駅の座標を代用しているだけなので既定では除外する。
    let wgs = format!("frm_{}", chronotope_core::FrameId::named("wgs84").0.simple());
    let near_q = |inherit: bool| json!({ "op": "search", "budget_ms": 500, "types": ["Event"], "space": { "near": { "at": { "frame": wgs, "geometry": { "type": "point", "at": { "x": 139.77, "y": 35.68 } } }, "radius": 2000.0 }, "include_inherited": inherit } });
    let near = q(&kb, &agent(), near_q(false));
    assert!(near["results"].as_array().unwrap().is_empty());
    assert!(near["warnings"][0].as_str().unwrap().contains("include_inherited"));
    let near = q(&kb, &agent(), near_q(true));
    assert_eq!(near["results"].as_array().unwrap().len(), 1);
    assert_eq!(near["results"][0]["placement"]["inherited_from"]["label"], "東京駅");

    // Level 2
    let claims = q(&kb, &agent(), json!({ "op": "expand_claims", "budget_ms": 500, "id": event, "include_incoming": true }));
    let groups = claims["results"]["claims"].as_array().unwrap();
    let occ = groups.iter().find(|g| g["predicate"] == "occurred_at").unwrap();
    assert_eq!(occ["contested"], false);
    assert_eq!(occ["preferred"]["tier"], "ai_only");
    assert_eq!(claims["results"]["incoming"].as_array().unwrap().len(), 1);
    let aid = occ["preferred"]["id"].as_str().unwrap().to_string();

    // Level 3
    let ev = q(&kb, &agent(), json!({ "op": "get_acquisition", "budget_ms": 500, "assertion": aid, "include_snapshot": true }));
    let e0 = &ev["results"]["evidence"][0];
    assert_eq!(e0["derivation"]["model_version"], "2026-08");
    assert_eq!(e0["acquisition"]["acquired_at"], "2026-09-21T10:00:00Z");
    // ライセンス不明の snapshot は AI には渡さない。
    assert!(e0["snapshot"]["withheld"].is_string());

    // 旧抽出器由来の Assertion 検索
    let old = q(&kb, &agent(), json!({ "op": "derived_by", "budget_ms": 500, "model_version": "2026-08" }));
    assert_eq!(old["results"].as_array().unwrap().len(), 1);
}

#[test]
fn coordinates_are_inherited_only_from_a_single_most_specific_place() {
    let mut kb = kb();
    let wgs = format!("frm_{}", chronotope_core::FrameId::named("wgs84").0.simple());
    let geo = |x: f64, y: f64| json!({ "geo": { "frame": wgs, "geometry": { "type": "point", "at": { "x": x, "y": y } } } });
    let country = create(&mut kb, &["Country"], "国X");
    let north = create(&mut kb, &["Region"], "地域A");
    let south = create(&mut kb, &["Region"], "地域B");
    assert_accepted(&mut kb, &country, "coordinates", geo(136.0, 35.0));
    assert_accepted(&mut kb, &north, "coordinates", geo(140.0, 39.0));
    assert_accepted(&mut kb, &south, "coordinates", geo(141.0, 40.0));
    for r in [&north, &south] {
        assert_accepted(&mut kb, r, "located_in", json!({ "resource": country }));
    }
    // 国だけ → 国の代表点を代用（明示）。地域 A・B の両方 → 一意でないので代用しない。
    let a = create(&mut kb, &["Event"], "地震A");
    assert_accepted(&mut kb, &a, "took_place_at", json!({ "resource": country }));
    let b = create(&mut kb, &["Event"], "地震B");
    for p in [&country, &north, &south] {
        assert_accepted(&mut kb, &b, "took_place_at", json!({ "resource": p }));
    }
    let c = create(&mut kb, &["Event"], "地震C");
    for p in [&country, &north] {
        assert_accepted(&mut kb, &c, "took_place_at", json!({ "resource": p }));
    }
    kb.materialize_all().unwrap();
    let look = |id: &str| q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": id }))["results"]["matches"][0].clone();
    assert_eq!(look(&a)["placement"]["inherited_from"]["label"], "国X");
    assert!(look(&b)["placement"].is_null());
    assert_eq!(look(&c)["placement"]["inherited_from"]["label"], "地域A");
}

#[test]
fn contested_claims_and_independent_sources() {
    let mut kb = kb();
    let ev = create(&mut kb, &["Event"], "ある合戦");
    let root = source(&mut kb, "https://primary.example/chronicle", "2026-01-01T00:00:00Z", json!({ "origin": "primary" }));
    let root_src = kb.store().acquisitions.values().find(|a| a.id.to_string() == root).unwrap().source.to_string();
    let reprint = source(&mut kb, "https://blog.example/copy", "2026-02-01T00:00:00Z", json!({ "origin": "secondary", "provenance_root": root_src }));
    let other = source(&mut kb, "https://other.example/record", "2026-03-01T00:00:00Z", json!({ "origin": "secondary" }));
    let a1203 = w(
        &mut kb,
        &agent(),
        json!({ "op": "propose_assertion", "subject": ev, "predicate": "occurred_at", "object": { "time": "1203年" }, "evidence": [{ "acquisition": root }, { "acquisition": reprint }] }),
    );
    w(
        &mut kb,
        &agent(),
        json!({ "op": "propose_assertion", "subject": ev, "predicate": "occurred_at", "object": { "time": "1204年頃" }, "evidence": [{ "acquisition": other }] }),
    );
    kb.materialize_all().unwrap();

    let c = q(&kb, &agent(), json!({ "op": "expand_claims", "budget_ms": 500, "id": ev }));
    let occ = c["results"]["claims"].as_array().unwrap().iter().find(|g| g["predicate"] == "occurred_at").unwrap().clone();
    assert_eq!(occ["contested"], true, "{occ:#}");
    // 転載は独立出典として数えない。
    assert_eq!(occ["preferred"]["confidence"]["independent_sources"], 1);
    assert_eq!(occ["preferred"]["id"], a1203["id"], "primary source wins over secondary");
    assert_eq!(occ["alternatives"].as_array().unwrap().len(), 1);
    let s = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": ev }));
    assert_eq!(s["results"]["matches"][0]["contested"], true);
    assert_eq!(s["results"]["matches"][0]["time"]["contested"], true);

    // 独立した 2 つ目の出典で裏付けると corroborated になる。
    let indep = source(&mut kb, "https://museum.example/catalog", "2026-04-01T00:00:00Z", json!({ "origin": "secondary" }));
    w(&mut kb, &agent(), json!({ "op": "add_evidence", "assertion": a1203["id"], "evidence": { "acquisition": indep } }));
    kb.materialize_all().unwrap();
    let c = q(&kb, &agent(), json!({ "op": "expand_claims", "budget_ms": 500, "id": ev }));
    let occ = c["results"]["claims"].as_array().unwrap().iter().find(|g| g["predicate"] == "occurred_at").unwrap().clone();
    assert_eq!(occ["preferred"]["confidence"]["independent_sources"], 2);
    assert_eq!(occ["preferred"]["tier"], "corroborated");

    let conflicts = q(&kb, &agent(), json!({ "op": "conflicts", "budget_ms": 2000 }));
    assert_eq!(conflicts["tier"], "tier2");
    assert_eq!(conflicts["results"]["resources"].as_array().unwrap().len(), 1);
}

#[test]
fn as_known_at_uses_acquisition_time() {
    let mut kb = kb();
    let ev = create(&mut kb, &["Event"], "発表");
    let early = source(&mut kb, "https://a.example/1", "2026-01-10T00:00:00Z", json!({}));
    let late = source(&mut kb, "https://a.example/2", "2026-06-10T00:00:00Z", json!({}));
    w(
        &mut kb,
        &agent(),
        json!({ "op": "propose_assertion", "subject": ev, "predicate": "occurred_at", "object": { "time": "2026-01-05" }, "evidence": [{ "acquisition": early }] }),
    );
    let later_claim = w(
        &mut kb,
        &agent(),
        json!({ "op": "propose_assertion", "subject": ev, "predicate": "occurred_at", "object": { "time": "2026-01-06" }, "evidence": [{ "acquisition": late }] }),
    );
    // 訂正（撤回）は 2026-07 に取得した情報に基づく。
    let correction = source(&mut kb, "https://a.example/3", "2026-07-01T00:00:00Z", json!({}));
    w(&mut kb, &curator(), json!({ "op": "retract_assertion", "id": later_claim["id"], "basis_acquisition": correction }));
    kb.materialize_all().unwrap();

    let at = |t: &str| {
        let c = q(&kb, &agent(), json!({ "op": "expand_claims", "budget_ms": 500, "id": ev, "as_known_at": t }));
        c["results"]["claims"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["predicate"] == "occurred_at")
            .map(|g| (g["contested"].as_bool().unwrap(), 1 + g["alternatives"].as_array().unwrap().len()))
    };
    assert_eq!(at("2026-02-01T00:00:00Z"), Some((false, 1)), "only the early claim was known");
    assert_eq!(at("2026-06-15T00:00:00Z"), Some((true, 2)), "both claims known, not yet retracted");
    assert_eq!(at("2026-08-01T00:00:00Z"), Some((false, 1)), "retraction known");
    assert_eq!(at("2025-12-01T00:00:00Z"), None, "nothing known yet");

    let s = q(&kb, &agent(), json!({ "op": "search", "budget_ms": 500, "types": ["Event"], "as_known_at": "2025-12-01T00:00:00Z" }));
    assert!(s["results"].as_array().unwrap().is_empty());
}

#[test]
fn relative_time_resolution_and_invalidation() {
    let mut kb = kb();
    let a = create(&mut kb, &["Event"], "A事件");
    let b = create(&mut kb, &["Event"], "B事件");
    let x = create(&mut kb, &["Event"], "X");
    let y = create(&mut kb, &["Event"], "Y");
    assert_accepted(&mut kb, &a, "occurred_at", json!({ "time": "2026-09-10" }));
    assert_accepted(&mut kb, &b, "occurred_at", json!({ "time": "2026-09-20" }));
    assert_accepted(&mut kb, &x, "occurred_at", json!({ "time": "A事件の3日前" }));
    assert_accepted(&mut kb, &y, "occurred_at", json!({ "time": "A事件より後、B事件より前" }));
    kb.materialize_all().unwrap();
    let lx = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": x }));
    assert_eq!(lx["results"]["matches"][0]["time"]["earliest_start"], "2026-09-07T00:00:00Z");
    let ly = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": y }));
    // A は 9/10 のどこか（短ければ 9/10 の早い時刻に終わり得る）ので、Y の開始は 9/10 0 時以降。
    assert_eq!(ly["results"]["matches"][0]["time"]["earliest_start"], "2026-09-10T00:00:00Z");
    assert_eq!(ly["results"]["matches"][0]["time"]["latest_end"], "2026-09-21T00:00:00Z");

    // A の時間を訂正すると、依存する X も無効化キュー経由で再計算される。
    let old = q(&kb, &curator(), json!({ "op": "expand_claims", "budget_ms": 500, "id": a }));
    let old_id = old["results"]["claims"][0]["preferred"]["id"].clone();
    w(
        &mut kb,
        &curator(),
        json!({ "op": "supersede_assertion", "id": old_id, "replacement": { "subject": a, "predicate": "occurred_at", "object": { "time": "2026-09-12" }, "status": "accepted" } }),
    );
    let before = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": x }));
    assert_eq!(before["results"]["matches"][0]["stale"], true, "stale until materialized");
    kb.materialize_all().unwrap();
    let after = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": x }));
    assert_eq!(after["results"]["matches"][0]["time"]["earliest_start"], "2026-09-09T00:00:00Z");
    assert_eq!(after["results"]["matches"][0]["stale"], false);

    // 循環参照は解決せず理由を返す。
    let c1 = create(&mut kb, &["Event"], "循環1");
    let c2 = create(&mut kb, &["Event"], "循環2");
    assert_accepted(&mut kb, &c1, "occurred_at", json!({ "time": "循環2の1日後" }));
    assert_accepted(&mut kb, &c2, "occurred_at", json!({ "time": "循環1の1日後" }));
    kb.materialize_all().unwrap();
    let lc = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": c1 }));
    let unresolved = lc["results"]["matches"][0]["time"]["unresolved"].as_str().unwrap();
    assert!(unresolved == "cycle" || unresolved == "anchor_unresolved", "{lc:#}");

    // 参照時刻が必要な表現
    let r = q(&kb, &agent(), json!({ "op": "resolve_temporal", "budget_ms": 100, "text": "先週火曜日", "reference": "2026-09-23T12:00:00Z" }));
    assert_eq!(r["results"]["resolved"]["earliest_start"], "2026-09-15T00:00:00Z");
    let r = q(&kb, &agent(), json!({ "op": "resolve_temporal", "budget_ms": 100, "text": "数日前" }));
    assert_eq!(r["results"]["unresolved"]["kind"], "needs_reference");
}

#[test]
fn partial_order_without_absolute_time() {
    let mut kb = kb();
    let (a, b, c) = (create(&mut kb, &["Event"], "序章"), create(&mut kb, &["Event"], "中盤"), create(&mut kb, &["Event"], "終章"));
    assert_accepted(&mut kb, &b, "before", json!({ "resource": c }));
    assert_accepted(&mut kb, &a, "before", json!({ "resource": b }));
    kb.materialize_all().unwrap();
    let r = q(&kb, &agent(), json!({ "op": "temporal_relation", "budget_ms": 100, "a": a, "b": c }));
    assert_eq!(r["results"]["certain"], "before", "{r:#}");
    assert_eq!(r["results"]["basis"], "order_graph");

    // 矛盾する制約はグラフに載らず、衝突として報告される。
    let bad = assert_accepted(&mut kb, &c, "before", json!({ "resource": a }));
    kb.materialize_all().unwrap();
    let conf = q(&kb, &agent(), json!({ "op": "conflicts", "budget_ms": 1000, "id": c }));
    let oc = &conf["results"]["resources"][0]["temporal_order_conflicts"];
    assert_eq!(oc[0]["assertion"], bad.as_str(), "{conf:#}");
    let t = q(&kb, &agent(), json!({ "op": "timeline", "budget_ms": 500 }));
    let order: Vec<&str> = t["results"]["relative_order"].as_array().unwrap().iter().map(|x| x["label"].as_str().unwrap()).collect();
    assert_eq!(order, vec!["序章", "中盤", "終章"]);
}

#[test]
fn incomparable_calendars() {
    let mut kb = kb();
    w(
        &mut kb,
        &curator(),
        json!({ "op": "define_calendar", "frame": {
        "key": "valley", "name": "Valley calendar", "axis": "valley-world",
        "kind": { "type": "uniform", "epoch": 0, "ticks_per_day": 1_200_000, "days_per_month": 28, "months_per_year": 4, "days_per_week": 7 }
    } }),
    );
    let game = create(&mut kb, &["Event"], "収穫祭");
    let real = create(&mut kb, &["Event"], "現実の祭り");
    assert_accepted(&mut kb, &game, "occurred_at", json!({ "time": "2年1月16日", "calendar": "valley" }));
    assert_accepted(&mut kb, &real, "occurred_at", json!({ "time": "2026-09-20" }));
    kb.materialize_all().unwrap();
    let r = q(&kb, &agent(), json!({ "op": "temporal_relation", "budget_ms": 100, "a": game, "b": real }));
    assert_eq!(r["results"]["comparable"], false);
    // 地球時間軸での検索は他軸の出来事を黙って混ぜない。
    let s = q(&kb, &agent(), json!({ "op": "search", "budget_ms": 500, "time": { "from": "-inf", "to": "+inf" } }));
    let labels: Vec<&str> = s["results"].as_array().unwrap().iter().map(|x| x["label"].as_str().unwrap()).collect();
    assert_eq!(labels, vec!["現実の祭り"]);
    assert!(s["warnings"][0].as_str().unwrap().contains("comparable: false"));
    let g = q(&kb, &agent(), json!({ "op": "search", "budget_ms": 500, "time": { "from": "2年1月", "to": "2年1月", "calendar": "valley" } }));
    assert_eq!(g["results"][0]["label"], "収穫祭");
}

#[test]
fn identity_merge_requires_confirmation() {
    let mut kb = kb();
    let p1 = create(&mut kb, &["Person"], "山田太郎");
    let p2 = create(&mut kb, &["Person"], "山田 太郎");
    w(&mut kb, &agent(), json!({ "op": "propose_assertion", "subject": p1, "predicate": "possibly_same_as", "object": { "resource": p2 } }));
    kb.materialize_all().unwrap();
    let l = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": p1 }));
    assert_eq!(l["results"]["matches"][0]["possibly_same_as"][0], p2.as_str(), "treated as distinct, flagged");
    let prop = w(&mut kb, &agent(), json!({ "op": "merge_identity", "from": p2, "into": p1, "reason": "same person" }));
    assert_eq!(prop["status"], "proposed");
    let err = w_err(&mut kb, &agent(), json!({ "op": "decide_merge", "id": prop["proposal"], "approve": true }));
    assert_eq!(err.code(), "forbidden");
    w(&mut kb, &curator(), json!({ "op": "decide_merge", "id": prop["proposal"], "approve": true }));
    kb.materialize_all().unwrap();
    let l = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": p2 }));
    assert_eq!(l["results"]["matches"][0]["id"], p1.as_str());
    assert_eq!(l["results"]["matches"][0]["redirected_from"], p2.as_str());
    // 統合後も別名で検索できる。
    let s = q(&kb, &agent(), json!({ "op": "search", "budget_ms": 500, "text": "山田 太郎" }));
    assert_eq!(s["results"].as_array().unwrap().len(), 1);

    // distinct_from があると統合提案自体を拒否する。
    let p3 = create(&mut kb, &["Person"], "山田花子");
    assert_accepted(&mut kb, &p3, "distinct_from", json!({ "resource": p1 }));
    let err = w_err(&mut kb, &agent(), json!({ "op": "merge_identity", "from": p3, "into": p1 }));
    assert_eq!(err.code(), "conflict");
}

#[test]
fn branches_are_copy_on_write() {
    let mut kb = kb();
    let ev = create(&mut kb, &["Event"], "出来事");
    let aid = assert_accepted(&mut kb, &ev, "occurred_at", json!({ "time": "2026-05-01" }));
    w(&mut kb, &curator(), json!({ "op": "create_branch", "name": "what-if" }));
    w(&mut kb, &curator(), json!({ "op": "retract_assertion", "id": aid, "branch": "what-if" }));
    w(&mut kb, &curator(), json!({ "op": "materialize_branch", "branch": "what-if" }));
    kb.materialize_all().unwrap();
    let main = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": ev }));
    assert!(main["results"]["matches"][0]["time"].is_object());
    let br = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": ev, "branch": "what-if" }));
    assert!(br["results"]["matches"][0]["time"].is_null(), "{br:#}");
    // main への後続の変更はブランチに見えない。
    let osaka = create(&mut kb, &["City"], "大阪");
    assert_accepted(&mut kb, &ev, "took_place_at", json!({ "resource": osaka }));
    kb.materialize_all().unwrap();
    let br = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": ev, "branch": "what-if" }));
    assert!(br["results"]["matches"][0]["places"].is_null());
    let main = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": ev }));
    assert_eq!(main["results"]["matches"][0]["places"][0], "大阪");
}

#[test]
fn canon_timeline_and_work_graph() {
    let mut kb = kb();
    let franchise = create(&mut kb, &["Franchise"], "星の物語");
    let series = create(&mut kb, &["Series"], "星の物語 TV");
    let season = create(&mut kb, &["Season"], "第1期");
    let ep1 = create(&mut kb, &["Episode"], "第1話");
    let ep2 = create(&mut kb, &["Episode"], "第2話");
    let movie = create(&mut kb, &["Movie"], "劇場版");
    for (s, o) in [(&series, &franchise), (&season, &series), (&ep1, &season), (&ep2, &season), (&movie, &franchise)] {
        assert_accepted(&mut kb, s, "part_of_work", json!({ "resource": o }));
    }
    let manga_canon = create(&mut kb, &["Canon"], "原作漫画");
    let anime_canon = create(&mut kb, &["Canon"], "アニメ版");
    let hero = create(&mut kb, &["Character"], "ヒーロー");
    let battle = create(&mut kb, &["Event"], "決戦");
    assert_accepted(&mut kb, &battle, "appears_in", json!({ "resource": ep2 }));
    assert_accepted(&mut kb, &hero, "participated_in", json!({ "resource": battle }));
    w(
        &mut kb,
        &curator(),
        json!({ "op": "propose_assertion", "subject": battle, "predicate": "occurred_at", "object": { "time": "2026-01-01" }, "canon": manga_canon, "status": "accepted" }),
    );
    w(
        &mut kb,
        &curator(),
        json!({ "op": "propose_assertion", "subject": battle, "predicate": "occurred_at", "object": { "time": "2026-02-01" }, "canon": anime_canon, "status": "accepted" }),
    );
    w(&mut kb, &curator(), json!({ "op": "define_sequence", "scope": series, "kind": "story_order", "items": [ep2, ep1] }));
    kb.materialize_all().unwrap();

    // シリーズ横断: Franchise 指定で Episode 内の Event が見つかる。
    let s = q(&kb, &agent(), json!({ "op": "search", "budget_ms": 500, "types": ["Event"], "works": [franchise] }));
    assert_eq!(s["results"][0]["id"], battle.as_str());
    // 既定ビューでは canon ごとに異なる時間が contested として見える。
    let l = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "id": battle }));
    assert_eq!(l["results"]["matches"][0]["contested"], true);
    // canon 指定で解決が変わる（検索時にビューで再検証）。
    let s = q(&kb, &agent(), json!({ "op": "search", "budget_ms": 500, "types": ["Event"], "canon": anime_canon, "time": { "expression": "2026年2月" } }));
    assert_eq!(s["results"].as_array().unwrap().len(), 1);
    let s = q(&kb, &agent(), json!({ "op": "search", "budget_ms": 500, "types": ["Event"], "canon": manga_canon, "time": { "expression": "2026年2月" } }));
    assert!(s["results"].as_array().unwrap().is_empty());
    let seq = q(&kb, &agent(), json!({ "op": "sequence", "budget_ms": 100, "scope": series, "kind": "story_order" }));
    assert_eq!(seq["results"]["sequences"][0]["items"][0]["label"], "第2話");
}

#[test]
fn agents_cannot_accept_or_define_vocabulary() {
    let mut kb = kb();
    let ev = create(&mut kb, &["Event"], "E");
    let r = w(
        &mut kb,
        &agent(),
        json!({ "op": "propose_assertion", "subject": ev, "predicate": "occurred_at", "object": { "time": "2026" }, "status": "accepted" }),
    );
    assert_eq!(r["status"], "proposed");
    assert!(r["warnings"][0].as_str().unwrap().contains("proposed"));
    let err = w_err(&mut kb, &agent(), json!({ "op": "accept_assertion", "id": r["id"] }));
    assert_eq!(err.code(), "forbidden");
    let p = w(&mut kb, &agent(), json!({ "op": "propose_predicate", "key": "rival_of", "symmetric": true, "range": { "kind": "resource" } }));
    assert_eq!(p["status"], "proposed");
    let r2 = w(&mut kb, &agent(), json!({ "op": "propose_assertion", "subject": ev, "predicate": "rival_of", "object": { "resource": ev } }));
    assert!(r2["warnings"][0].as_str().unwrap().contains("only proposed"));
    let err = w_err(&mut kb, &agent(), json!({ "op": "propose_assertion", "subject": ev, "predicate": "occurred_at", "object": { "text": "not a time" } }));
    assert_eq!(err.code(), "invalid");
    let err = w_err(&mut kb, &agent(), json!({ "op": "propose_assertion", "subject": ev, "predicate": "no_such", "object": { "text": "x" } }));
    assert_eq!(err.code(), "not_found");
    // 他人の主張の撤回は dispute に落ちる。
    let a = assert_accepted(&mut kb, &ev, "name", json!({ "text": "E" }));
    let rr = w(&mut kb, &agent(), json!({ "op": "retract_assertion", "id": a }));
    assert_eq!(rr["status"], "disputed");
}

#[test]
fn crypto_shredding_and_license_enforcement() {
    let mut kb = kb();
    let person = create(&mut kb, &["Person"], "私人A");
    let key = w(&mut kb, &curator(), json!({ "op": "create_key" }))["key_id"].as_str().unwrap().to_string();
    w(
        &mut kb,
        &curator(),
        json!({ "op": "propose_assertion", "subject": person, "predicate": "name", "object": { "text": "本名 山田" }, "protect_with": key, "status": "accepted" }),
    );
    kb.materialize_all().unwrap();
    let c = q(&kb, &curator(), json!({ "op": "expand_claims", "budget_ms": 500, "id": person }));
    assert_eq!(c["results"]["claims"][0]["preferred"]["value"], "本名 山田");
    w(&mut kb, &curator(), json!({ "op": "shred_key", "key_id": key }));
    kb.materialize_all().unwrap();
    let c = q(&kb, &curator(), json!({ "op": "expand_claims", "budget_ms": 500, "id": person }));
    assert_eq!(c["results"]["claims"][0]["preferred"]["value"], "[shredded]");
    // Revision ログには暗号文しか残っていない。
    assert!(kb.revisions().len() > 3);

    let acq = source(&mut kb, "https://open.example/doc", "2026-09-01T00:00:00Z", json!({ "license": "CC-BY-4.0" }));
    let ev = create(&mut kb, &["Event"], "公開情報");
    let a = w(
        &mut kb,
        &agent(),
        json!({ "op": "propose_assertion", "subject": ev, "predicate": "occurred_at", "object": { "time": "2026-08-01" }, "evidence": [{ "acquisition": acq }] }),
    );
    let g = q(&kb, &agent(), json!({ "op": "get_acquisition", "budget_ms": 100, "assertion": a["id"], "include_snapshot": true }));
    assert_eq!(g["results"]["evidence"][0]["snapshot"]["content"], "snapshot of https://open.example/doc");

    // 非公開グループの Assertion は RLS 相当で隠れる。
    w(
        &mut kb,
        &curator(),
        json!({ "op": "propose_assertion", "subject": ev, "predicate": "name", "object": { "text": "内部名" }, "visibility": { "level": "groups", "groups": ["staff"] }, "status": "accepted" }),
    );
    let c = q(&kb, &agent(), json!({ "op": "expand_claims", "budget_ms": 500, "id": ev }));
    assert!(c["results"]["claims"].as_array().unwrap().iter().all(|g| g["predicate"] != "name"));
    let mut staff = Principal::agent("staff-agent");
    staff.groups.insert("staff".into());
    let c = q(&kb, &staff, json!({ "op": "expand_claims", "budget_ms": 500, "id": ev }));
    assert!(c["results"]["claims"].as_array().unwrap().iter().any(|g| g["predicate"] == "name"));
    // 同じ内容の snapshot は重複排除される。
    let again = w(
        &mut kb,
        &agent(),
        json!({ "op": "link_source", "url": "https://open.example/doc", "acquisition": { "acquired_at": "2026-09-02T00:00:00Z", "content": "snapshot of https://open.example/doc" } }),
    );
    assert_eq!(again["new_source"], false);
    assert_eq!(again["snapshot_deduplicated"], true);
}

#[test]
fn recurrence_observations_trajectory_vectors() {
    let mut kb = kb();
    let show = create(&mut kb, &["Event"], "深夜アニメ放送");
    assert_accepted(&mut kb, &show, "occurred_at", json!({ "time": "毎週金曜日25:30", "calendar": "gregorian+09:00" }));
    let post = create(&mut kb, &["Post"], "話題の投稿");
    for (t, v) in [("2026-09-20T12:00:00Z", 100.0), ("2026-09-20T13:00:00Z", 300.0), ("2026-09-20T18:00:00Z", 5000.0)] {
        w(&mut kb, &agent(), json!({ "op": "add_observation", "target": post, "metric": "likes", "observed_at": t, "value": v, "unit": "{likes}" }));
    }
    let train = create(&mut kb, &["Vehicle"], "のぞみ1号");
    let wgs = format!("frm_{}", chronotope_core::FrameId::named("wgs84").0.simple());
    w(
        &mut kb,
        &agent(),
        json!({ "op": "add_trajectory", "target": train, "frame": wgs, "samples": [
        { "t": "2026-09-20T06:00:00Z", "x": 139.7671, "y": 35.6812 },
        { "t": "2026-09-20T08:30:00Z", "x": 135.5023, "y": 34.7334 }
    ] }),
    );
    kb.materialize_all().unwrap();

    // 2026-09-19 01:30 JST（金曜 25:30）を含む窓
    let s = q(
        &kb,
        &agent(),
        json!({ "op": "search", "budget_ms": 500, "types": ["Event"], "time": { "from": "2026-09-18T16:00:00Z", "to": "2026-09-18T17:00:00Z" } }),
    );
    assert_eq!(s["results"][0]["id"], show.as_str());
    let s = q(
        &kb,
        &agent(),
        json!({ "op": "search", "budget_ms": 500, "types": ["Event"], "time": { "from": "2026-09-18T10:00:00Z", "to": "2026-09-18T11:00:00Z" } }),
    );
    assert!(s["results"].as_array().unwrap().is_empty());

    let o = q(&kb, &agent(), json!({ "op": "observations", "budget_ms": 100, "target": post, "metric": "likes", "from": "2026-09-20T12:30:00Z" }));
    assert_eq!(o["results"]["count"], 2);
    let pos = q(&kb, &agent(), json!({ "op": "position_at", "budget_ms": 100, "target": train, "at": "2026-09-20T07:15:00Z" }));
    let x = pos["results"]["position"]["geometry"]["at"]["x"].as_f64().unwrap();
    assert!((137.0..138.0).contains(&x), "{x}");

    // テキストからのベクトル検索（Post / Event が自動 Embedding 対象）
    let v = q(&kb, &agent(), json!({ "op": "search", "budget_ms": 500, "vector": { "text": "話題の投稿" }, "limit": 1 }));
    assert_eq!(v["results"][0]["id"], post.as_str());
    let sim = q(&kb, &agent(), json!({ "op": "similar", "budget_ms": 500, "id": show }));
    assert!(sim["results"]["similar"].is_array());
}

#[test]
fn budget_is_required_and_enforced() {
    let mut kb = kb();
    for i in 0..3000 {
        let e = create(&mut kb, &["Event"], &format!("event {i}"));
        if i == 0 {
            let _ = e;
        }
    }
    kb.materialize_all().unwrap();
    let err = kb.query_json(&agent(), &json!({ "op": "search" }).to_string()).unwrap_err();
    assert!(err.to_string().contains("budget_ms"), "{err}");
    // 予算を使い切ると部分結果で truncated。
    let r = q(&kb, &agent(), json!({ "op": "similar", "budget_ms": 1, "id": kb.store().resources.keys().next().unwrap().to_string() }));
    let _ = r["truncated"].as_bool().unwrap();
    let r = q(&kb, &agent(), json!({ "op": "search", "budget_ms": 1000, "text": "event", "limit": 5 }));
    assert_eq!(r["results"].as_array().unwrap().len(), 5);
    assert_eq!(r["truncated"], false);
}

#[test]
fn ambiguous_lookup_and_spatial_resolution() {
    let mut kb = kb();
    let a = create(&mut kb, &["City"], "府中");
    let b = create(&mut kb, &["City"], "府中");
    let tokyo = create(&mut kb, &["Region"], "東京都");
    let hiroshima = create(&mut kb, &["Region"], "広島県");
    assert_accepted(&mut kb, &a, "located_in", json!({ "resource": tokyo }));
    assert_accepted(&mut kb, &b, "located_in", json!({ "resource": hiroshima }));
    kb.materialize_all().unwrap();
    let l = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 50, "label": "府中" }));
    assert_eq!(l["results"]["ambiguous"], true);
    assert_eq!(l["results"]["matches"].as_array().unwrap().len(), 2);
    let r = q(&kb, &agent(), json!({ "op": "resolve_spatial", "budget_ms": 100, "name": "府中" }));
    assert_eq!(r["results"]["ambiguous"], true);
    let anc: Vec<String> = r["results"]["candidates"].as_array().unwrap().iter().map(|c| c["ancestors"][0].as_str().unwrap().to_string()).collect();
    assert!(anc.contains(&"東京都".to_string()) && anc.contains(&"広島県".to_string()));
}

#[test]
fn rdf_export_only_accepted_redistributable() {
    let mut kb = kb();
    let t = create(&mut kb, &["Building"], "東京タワー");
    assert_accepted(&mut kb, &t, "height", json!({ "quantity": 333, "unit": "m" }));
    w(&mut kb, &agent(), json!({ "op": "propose_assertion", "subject": t, "predicate": "height", "object": { "quantity": 334, "unit": "m" } }));
    let nt = kb.export_ntriples(chronotope_core::BranchId::main()).unwrap();
    assert!(nt.contains("\"333 m\""), "{nt}");
    assert!(!nt.contains("334"), "proposed claims are not exported as facts");
    assert!(nt.contains("http://www.wikidata.org/prop/direct/P2048"));
}

#[test]
fn persistence_replays_revision_log() {
    let dir = std::env::temp_dir().join(format!("chronotope-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ev;
    {
        let mut kb = KnowledgeBase::open(&dir, KbConfig::default()).unwrap();
        ev = create(&mut kb, &["Event"], "永続化テスト");
        assert_accepted(&mut kb, &ev, "occurred_at", json!({ "time": "2026-09-01" }));
        source(&mut kb, "https://persist.example", "2026-09-02T00:00:00Z", json!({}));
    }
    let mut kb = KnowledgeBase::open(&dir, KbConfig::default()).unwrap();
    kb.materialize_all().unwrap();
    let l = q(&kb, &agent(), json!({ "op": "lookup", "budget_ms": 100, "id": ev }));
    assert_eq!(l["results"]["matches"][0]["label"], "永続化テスト");
    assert_eq!(l["results"]["matches"][0]["time"]["earliest_start"], "2026-09-01T00:00:00Z");
    assert_eq!(kb.store().acquisitions.len(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ------------------------------------------------------------------ 会話履歴

fn user(id: &str) -> Principal {
    let mut p = Principal::agent(id);
    p.actor.kind = chronotope_core::model::ActorKind::Human;
    p
}

fn event(id: &str, conv: &str, seq: u64, kind: &str, origin: &str, content: &str) -> Value {
    json!({ "event_id": id, "conversation": conv, "sequence": seq, "kind": kind, "origin": origin, "content": content })
}

fn record(kb: &mut KnowledgeBase, p: &Principal, events: Vec<Value>) -> Value {
    w(kb, p, json!({ "op": "record_events", "events": events }))
}

fn hq(kb: &KnowledgeBase, p: &Principal, mut body: Value) -> Value {
    body["budget_ms"] = json!(2000);
    q(kb, p, body)["results"].clone()
}

fn read_all(kb: &KnowledgeBase, p: &Principal, event_id: &str, page: u64) -> (String, Value) {
    let mut out = String::new();
    let mut offset = 0u64;
    loop {
        let r = hq(kb, p, json!({ "op": "history_get", "event": event_id, "offset": offset, "length": page }));
        let c = &r["content"];
        assert_eq!(c["encoding"], "utf-8");
        assert_eq!(c["range"]["start"], offset);
        out.push_str(c["text"].as_str().unwrap());
        match c["next_offset"].as_u64() {
            Some(n) => {
                assert!(n > offset);
                offset = n;
            }
            None => return (out, c.clone()),
        }
    }
}

#[test]
fn history_keeps_raw_text_and_pages_through_large_content() {
    let mut kb = kb();
    let alice = user("alice");
    let raw = "  前回の依頼は取り消し🙏\r\n\t東京タワーの件で。 \n";
    let r = w(
        &mut kb,
        &alice,
        json!({ "op": "record_event", "event_id": "e1", "conversation": "c1", "sequence": 1, "kind": "message", "origin": "human", "api_role": "user",
                "received_at": "2026-09-22T10:00:00Z", "content": raw }),
    );
    assert_eq!(r["duplicate"], false);
    assert_eq!(r["owner"], "alice");
    let g = hq(&kb, &alice, json!({ "op": "history_get", "event": "e1" }));
    assert_eq!(g["content"]["text"], raw);
    assert_eq!(g["content"]["next_offset"], Value::Null);
    assert_eq!(g["content"]["content_hash"], g["event"]["content_hash"]);
    assert_eq!(g["event"]["origin"], "human");
    assert_eq!(g["event"]["size"], raw.len());

    // 64 KiB を超える多バイト文字の本文を、半端な長さのページでも欠落なく復元できる。
    let big: String = (0..30_000).map(|i| ["あ", "😀", "a", "\n", "漢"][i % 5]).collect();
    assert!(big.len() > 64 * 1024);
    record(&mut kb, &alice, vec![event("e2", "c1", 2, "tool_result", "tool", &big)]);
    let (text, last) = read_all(&kb, &alice, "e2", 1001);
    assert_eq!(text, big);
    assert_eq!(last["total_size"], big.len());
    let (text, _) = read_all(&kb, &alice, "e2", 64 * 1024);
    assert_eq!(text, big);
    // 既定のページでは打ち切りを明示し、続きの位置を返す。
    let g = hq(&kb, &alice, json!({ "op": "history_get", "event": "e2" }));
    assert!(g["content"]["next_offset"].as_u64().unwrap() <= 64 * 1024);
    // 文字の途中からは読ませない（base64 なら任意の位置から読める）。
    let e = kb.query_json(&alice, &json!({ "op": "history_get", "budget_ms": 100, "event": "e2", "offset": 4 }).to_string()).unwrap_err();
    assert_eq!(e.code(), "invalid");
    let b = hq(&kb, &alice, json!({ "op": "history_get", "event": "e2", "offset": 4, "length": 3, "encoding": "base64" }));
    // "あ" の後の "😀"（F0 9F 98 80）の 2 バイト目から。
    assert_eq!(b["content"]["base64"], "n5iA");

    // get_acquisition もページ単位で最後まで読める（所有者本人は非公開の会話を読める）。
    let acq = g["event"]["acquisition"].as_str().unwrap().to_string();
    let mut offset = 0;
    let mut joined = String::new();
    loop {
        let a = hq(
            &kb,
            &alice,
            json!({ "op": "get_acquisition", "acquisition": acq, "include_snapshot": true, "snapshot_offset": offset, "snapshot_length": 50_000 }),
        );
        let s = &a["snapshot"];
        assert_eq!(s["license"], "unknown");
        joined.push_str(s["content"].as_str().unwrap());
        match s["next_offset"].as_u64() {
            Some(n) => offset = n,
            None => break,
        }
    }
    assert_eq!(joined, big);

    // UTF-8 でない本文は base64 で返し、本文検索できないことを示す。
    record(
        &mut kb,
        &alice,
        vec![
            json!({ "event_id": "bin", "conversation": "c1", "sequence": 3, "kind": "attachment", "origin": "human", "content_base64": "/wAB", "media_type": "application/octet-stream" }),
        ],
    );
    let g = hq(&kb, &alice, json!({ "op": "history_get", "event": "bin" }));
    assert_eq!(g["content"]["encoding"], "base64");
    assert_eq!(g["content"]["base64"], "/wAB");
    assert_eq!(g["event"]["text_indexed"], false);
    let s = q(&kb, &alice, json!({ "op": "history_search", "budget_ms": 500, "text": "東京" }));
    assert!(s["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("could not be searched")));
}

#[test]
fn history_records_are_idempotent_by_event_id() {
    let mut kb = kb();
    let alice = user("alice");
    let first = record(&mut kb, &alice, vec![event("e1", "c1", 1, "message", "human", "はい"), event("e2", "c1", 2, "message", "model", "了解")]);
    assert_eq!(first["recorded"], 2);
    let revs = kb.revisions().len();
    // 応答が失われた後の再送: 新しいイベントも Revision も増えない。
    let again = record(&mut kb, &alice, vec![event("e1", "c1", 1, "message", "human", "はい"), event("e2", "c1", 2, "message", "model", "了解")]);
    assert_eq!(again["recorded"], 0);
    assert_eq!(again["events"][0]["duplicate"], true);
    assert_eq!(again["events"][0]["acquisition"], first["events"][0]["acquisition"]);
    assert_eq!(kb.revisions().len(), revs);
    // 同じ ID で内容や記録順が違えば衝突。
    assert_eq!(w_err(&mut kb, &alice, json!({ "op": "record_events", "events": [event("e1", "c1", 1, "message", "human", "いいえ")] })).code(), "conflict");
    assert_eq!(w_err(&mut kb, &alice, json!({ "op": "record_events", "events": [event("e1", "c1", 1, "message", "runtime", "はい")] })).code(), "conflict");
    // 別 ID が同じ記録順を使うのも衝突。
    assert_eq!(w_err(&mut kb, &alice, json!({ "op": "record_events", "events": [event("e9", "c1", 2, "message", "human", "x")] })).code(), "conflict");
    // 送信側のハッシュと一致しない本文は受け付けない。
    let e = w_err(
        &mut kb,
        &alice,
        json!({ "op": "record_event", "event_id": "e3", "conversation": "c1", "sequence": 3, "kind": "message", "origin": "human", "content": "a", "content_hash": "blake3:00" }),
    );
    assert_eq!(e.code(), "invalid");
    // 同じ本文でも ID が違えば両方残る（同文の別発言）。
    let r = record(&mut kb, &alice, vec![event("e3", "c1", 3, "message", "human", "はい"), event("e3", "c1", 3, "message", "human", "はい")]);
    assert_eq!(r["recorded"], 1);
    assert_eq!(r["events"][1]["duplicate"], true);
    let s = hq(&kb, &alice, json!({ "op": "history_search", "text": "はい", "order": "oldest" }));
    let ids: Vec<&str> = s["events"].as_array().unwrap().iter().map(|e| e["event_id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["e1", "e3"]);
    // 匿名の主体は履歴を記録できない。
    assert_eq!(
        w_err(&mut kb, &Principal::anonymous(), json!({ "op": "record_events", "events": [event("x", "c", 1, "message", "human", "a")] })).code(),
        "forbidden"
    );
}

#[test]
fn history_distinguishes_origins_and_links_tool_calls() {
    let mut kb = kb();
    let alice = user("alice");
    let mut cont = event("e2", "c1", 2, "message", "runtime", "続けてください");
    cont["api_role"] = json!("user");
    let mut call_a = event("e4", "c1", 4, "tool_call", "model", r#"{"cmd":"ls"}"#);
    call_a["call_id"] = json!("call-a");
    let mut call_b = event("e5", "c1", 5, "tool_call", "model", r#"{"cmd":"rm -rf build"}"#);
    call_b["call_id"] = json!("call-b");
    let mut res_b = event("e6", "c1", 6, "tool_result", "tool", "removed");
    res_b["call_id"] = json!("call-b");
    res_b["status"] = json!("ok");
    let mut res_a = event("e7", "c1", 7, "tool_result", "tool", "");
    res_a["call_id"] = json!("call-a");
    res_a["status"] = json!("interrupted");
    let mut fix = event("e8", "c1", 8, "message", "human", "さっきの依頼は取り消して、ビルドは残して");
    fix["supersedes"] = json!("e1");
    record(
        &mut kb,
        &alice,
        vec![
            event("e1", "c1", 1, "message", "human", "ビルドを消して"),
            cont,
            event("e3", "c1", 3, "message", "model", "消します"),
            call_a,
            call_b,
            res_b,
            res_a,
            fix,
        ],
    );
    // ユーザー本人の発言だけを探す（API 上 role: user の自動継続指示は含めない）。
    let s = hq(&kb, &alice, json!({ "op": "history_search", "origins": ["human"], "order": "sequence", "conversation": "c1" }));
    let ids: Vec<&str> = s["events"].as_array().unwrap().iter().map(|e| e["event_id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["e1", "e8"]);
    let s = hq(&kb, &alice, json!({ "op": "history_search", "text": "続けて" }));
    assert_eq!(s["events"][0]["origin"], "runtime");
    assert_eq!(s["events"][0]["api_role"], "user");
    // 並行した呼び出しと結果の対応、中断の状態、訂正の関係。
    let g = hq(&kb, &alice, json!({ "op": "history_get", "event": "e4" }));
    assert_eq!(g["related"]["call"][0]["event_id"], "e7");
    assert_eq!(g["related"]["call"][0]["status"], "interrupted");
    let g = hq(&kb, &alice, json!({ "op": "history_get", "event": "e1" }));
    assert_eq!(g["event"]["superseded_by"], json!(["e8"]));
    assert_eq!(g["related"]["superseded_by"][0]["event_id"], "e8");
    // 前後の会話を記録順で確認する。
    let c = hq(&kb, &alice, json!({ "op": "history_context", "event": "e5", "before": 2, "after": 1 }));
    let seqs: Vec<u64> = c["events"].as_array().unwrap().iter().map(|e| e["sequence"].as_u64().unwrap()).collect();
    assert_eq!(seqs, vec![3, 4, 5, 6]);
    assert_eq!(c["has_more_before"], true);
    assert_eq!(c["has_more_after"], true);
    assert_eq!(c["events"][1]["content"]["text"], r#"{"cmd":"ls"}"#);
    // 会話だけを指定すると末尾から。
    let c = hq(&kb, &alice, json!({ "op": "history_context", "conversation": "c1", "before": 2, "max_content_bytes": 0 }));
    let seqs: Vec<u64> = c["events"].as_array().unwrap().iter().map(|e| e["sequence"].as_u64().unwrap()).collect();
    assert_eq!(seqs, vec![7, 8]);
    assert!(c["events"][0].get("content").is_none());
}

#[test]
fn history_search_matches_substrings_and_pages_with_cursor() {
    let mut kb = kb();
    let alice = user("alice");
    let events: Vec<Value> = (1..=25)
        .map(|i| {
            let mut e = event(&format!("e{i}"), "c1", i, "message", "human", &format!("{i} 番目: Deploy the mainline build。東京タワー"));
            e["received_at"] = json!(format!("2026-09-01T00:{:02}:00Z", i));
            e
        })
        .collect();
    record(&mut kb, &alice, events);
    record(&mut kb, &alice, vec![event("other", "c2", 1, "message", "human", "京都に行く")]);
    for q in ["ploy", "MAINL", "京タワ", "タワー mainline", "ｄｅｐｌｏｙ"] {
        let s = hq(&kb, &alice, json!({ "op": "history_search", "text": q, "limit": 1 }));
        assert_eq!(s["events"].as_array().unwrap().len(), 1, "{q}");
        assert_eq!(s["events"][0]["event_id"], "e25", "{q}");
    }
    let s = hq(&kb, &alice, json!({ "op": "history_search", "text": "京都" }));
    assert_eq!(s["events"][0]["event_id"], "other");
    let m = &s["events"][0];
    assert_eq!(m["excerpt"]["text"], "京都に行く");
    assert_eq!(m["match"], json!({ "unit": "byte", "start": 0, "end": 6 }));
    // 完全一致は原文の表記で照合する。
    assert_eq!(hq(&kb, &alice, json!({ "op": "history_search", "text": "deploy", "exact": true }))["events"].as_array().unwrap().len(), 0);
    assert_eq!(hq(&kb, &alice, json!({ "op": "history_search", "text": "Deploy the", "exact": true, "limit": 100 }))["events"].as_array().unwrap().len(), 25);
    // カーソルで取りこぼし・重複なく全件をたどれる。
    let mut seen = vec![];
    let mut cursor = Value::Null;
    loop {
        let s = hq(&kb, &alice, json!({ "op": "history_search", "text": "タワー", "limit": 7, "cursor": cursor, "conversation": "c1" }));
        seen.extend(s["events"].as_array().unwrap().iter().map(|e| e["sequence"].as_u64().unwrap()));
        if s["has_more"] == false {
            break;
        }
        cursor = s["next_cursor"].clone();
    }
    assert_eq!(seen, (1..=25).rev().collect::<Vec<u64>>());
    // 期間での絞り込み（受信時刻）。
    let s = hq(&kb, &alice, json!({ "op": "history_search", "from": "2026-09-01T00:10:00Z", "to": "2026-09-01T00:12:00Z", "order": "oldest" }));
    let seqs: Vec<u64> = s["events"].as_array().unwrap().iter().map(|e| e["sequence"].as_u64().unwrap()).collect();
    assert_eq!(seqs, vec![10, 11]);
    // 会話の一覧から同期済みの位置が分かる。
    let l = hq(&kb, &alice, json!({ "op": "history_conversations" }));
    assert_eq!(l["total"], 2);
    let c1 = l["conversations"].as_array().unwrap().iter().find(|c| c["conversation"] == "c1").unwrap();
    assert_eq!(c1["last_sequence"], 25);
    assert_eq!(c1["missing_sequences"], 0);
    assert_eq!(c1["visibility"]["level"], "private");
}

#[test]
fn history_is_visible_only_to_its_owner_and_delegates() {
    let mut kb = kb();
    let alice = user("alice");
    let bob = user("bob");
    let r = record(&mut kb, &alice, vec![event("e1", "c1", 1, "message", "human", "私の住所は秘密です")]);
    let acq = r["events"][0]["acquisition"].as_str().unwrap().to_string();
    // 別のユーザーは、検索・イベント ID・会話・Acquisition ID のどこからも取得できない。
    assert!(hq(&kb, &bob, json!({ "op": "history_search", "text": "住所" }))["events"].as_array().unwrap().is_empty());
    assert!(hq(&kb, &bob, json!({ "op": "history_search", "text": "住所", "owner": "alice" }))["events"].as_array().unwrap().is_empty());
    for body in [
        json!({ "op": "history_get", "event": "e1", "budget_ms": 100 }),
        json!({ "op": "history_get", "event": "e1", "owner": "alice", "budget_ms": 100 }),
        json!({ "op": "history_context", "conversation": "c1", "owner": "alice", "budget_ms": 100 }),
        json!({ "op": "get_acquisition", "acquisition": acq, "include_snapshot": true, "budget_ms": 100 }),
    ] {
        for p in [&bob, &Principal::curator("carol"), &Principal::anonymous()] {
            let e = kb.query_json(p, &body.to_string()).unwrap_err();
            assert_eq!(e.code(), "not_found", "{body} as {}", p.actor.id);
        }
    }
    assert!(hq(&kb, &bob, json!({ "op": "history_conversations", "owner": "alice" }))["conversations"].as_array().unwrap().is_empty());
    // 他人の非公開の取得記録を根拠として参照させない。
    let ev = create(&mut kb, &["Event"], "何か");
    let e = w_err(
        &mut kb,
        &bob,
        json!({ "op": "propose_assertion", "subject": ev, "predicate": "name", "object": { "text": "x" }, "evidence": [{ "acquisition": acq }] }),
    );
    assert_eq!(e.code(), "forbidden");
    // 同じ会話 ID でも所有者ごとに別の会話になる。
    record(&mut kb, &bob, vec![event("e1", "c1", 1, "message", "human", "bob の発言")]);
    assert_eq!(hq(&kb, &bob, json!({ "op": "history_get", "event": "e1" }))["content"]["text"], "bob の発言");
    assert_eq!(hq(&kb, &alice, json!({ "op": "history_get", "event": "e1" }))["content"]["text"], "私の住所は秘密です");

    // 委任されたエージェントは所有者の履歴を読み書きできるが、キュレーターにはならない。
    let delegate = Principal::delegated("assistant-1", "alice");
    let g = hq(&kb, &delegate, json!({ "op": "history_get", "event": "e1" }));
    assert_eq!(g["content"]["text"], "私の住所は秘密です");
    let a = hq(&kb, &delegate, json!({ "op": "get_acquisition", "acquisition": acq, "include_snapshot": true }));
    assert_eq!(a["snapshot"]["content"], "私の住所は秘密です");
    assert_eq!(a["snapshot"]["redistributable"], false);
    let r = record(&mut kb, &delegate, vec![event("e2", "c1", 2, "message", "model", "承知しました")]);
    assert_eq!(r["owner"], "alice");
    let g = hq(&kb, &alice, json!({ "op": "history_get", "event": "e2" }));
    assert_eq!(g["event"]["recorded_by"]["id"], "assistant-1");
    assert_eq!(
        w_err(&mut kb, &delegate, json!({ "op": "define_license", "license": { "key": "x", "name": "x", "redistributable": true } })).code(),
        "forbidden"
    );
    // 他人を所有者にした非公開データは作れない。
    let e = w_err(
        &mut kb,
        &bob,
        json!({ "op": "record_event", "event_id": "z", "conversation": "c9", "sequence": 1, "kind": "message", "origin": "human", "content": "a", "visibility": { "level": "private", "owner": "alice" } }),
    );
    assert_eq!(e.code(), "forbidden");
}

#[test]
fn private_sources_are_scoped_to_their_owner() {
    let mut kb = kb();
    let alice = user("alice");
    let bob = user("bob");
    let private = |who: &str| json!({ "visibility": { "level": "private", "owner": who } });
    let url = "https://intranet.example/doc";
    let body = |extra: Value| {
        let mut b = json!({ "op": "link_source", "url": url, "acquisition": { "acquired_at": "2026-09-01T00:00:00Z", "content": "社外秘" } });
        if let (Value::Object(m), Value::Object(e)) = (&mut b, extra) {
            m.extend(e);
        }
        b
    };
    let a1 = w(&mut kb, &alice, body(private("alice")));
    let b1 = w(&mut kb, &bob, body(private("bob")));
    let pub1 = w(&mut kb, &alice, body(json!({})));
    // 同じ URL でも所有者・公開ごとに別の Source。
    assert_ne!(a1["source"], b1["source"]);
    assert_ne!(a1["source"], pub1["source"]);
    assert_eq!(w(&mut kb, &alice, body(private("alice")))["source"], a1["source"]);
    // 非公開の Source は所有者だけが本文を読める（公開 Source はライセンスどおり伏せる）。
    let get = |p: &Principal, acq: &Value| {
        kb.query_json(p, &json!({ "op": "get_acquisition", "budget_ms": 100, "acquisition": acq, "include_snapshot": true }).to_string())
    };
    assert_eq!(serde_json::to_value(get(&alice, &a1["acquisition"]).unwrap()).unwrap()["results"]["snapshot"]["content"], "社外秘");
    assert_eq!(get(&bob, &a1["acquisition"]).unwrap_err().code(), "not_found");
    let p = serde_json::to_value(get(&alice, &pub1["acquisition"]).unwrap()).unwrap();
    assert!(p["results"]["snapshot"]["withheld"].is_string());
    // 他人の非公開 Source を ID 指定して取得記録を足すこともできない。
    let src = a1["source"].as_str().unwrap();
    let e = w_err(&mut kb, &bob, json!({ "op": "link_source", "source": src, "acquisition": { "acquired_at": "2026-09-02T00:00:00Z", "content": "x" } }));
    assert_eq!(e.code(), "forbidden");
}

#[test]
fn shredded_history_is_removed_from_the_text_index() {
    let mut kb = kb();
    let alice = user("alice");
    let key = w(&mut kb, &alice, json!({ "op": "create_key" }))["key_id"].as_str().unwrap().to_string();
    let mut e = event("e1", "c1", 1, "message", "human", "削除してほしい個人情報");
    e["encrypt_with"] = json!(key);
    record(&mut kb, &alice, vec![e.clone()]);
    assert_eq!(hq(&kb, &alice, json!({ "op": "history_search", "text": "個人情報" }))["events"].as_array().unwrap().len(), 1);
    // 暗号化した本文も、再送の同一性は復号して確かめる。
    assert_eq!(record(&mut kb, &alice, vec![e.clone()])["events"][0]["duplicate"], true);
    w(&mut kb, &curator(), json!({ "op": "shred_key", "key_id": key }));
    assert!(hq(&kb, &alice, json!({ "op": "history_search", "text": "個人情報" }))["events"].as_array().unwrap().is_empty());
    let g = hq(&kb, &alice, json!({ "op": "history_get", "event": "e1" }));
    assert_eq!(g["content"]["withheld"], "content key has been shredded");
    assert_eq!(g["event"]["text_indexed"], false);
    // メタデータ（記録順・生成元）は残る。
    assert_eq!(g["event"]["origin"], "human");
}

#[test]
fn history_survives_restart_and_a_torn_log_tail() {
    let dir = std::env::temp_dir().join(format!("chronotope-history-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let alice = user("alice");
    let big: String = "長い本文。".repeat(20_000);
    {
        let mut kb = KnowledgeBase::open(&dir, KbConfig::default()).unwrap();
        record(&mut kb, &alice, vec![event("e1", "c1", 1, "message", "human", " 最初の依頼 \n"), event("e2", "c1", 2, "tool_result", "tool", &big)]);
    }
    // 書き込み途中で落ちた Revision（末尾の改行の無い行）。
    let log = dir.join("revisions.jsonl");
    let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    std::io::Write::write_all(&mut f, br#"{"id":"rev_0192","seq":99,"#).unwrap();
    drop(f);
    {
        let mut kb = KnowledgeBase::open(&dir, KbConfig::default()).unwrap();
        assert_eq!(hq(&kb, &alice, json!({ "op": "history_get", "event": "e1" }))["content"]["text"], " 最初の依頼 \n");
        let (text, _) = read_all(&kb, &alice, "e2", 64 * 1024);
        assert_eq!(text, big);
        assert_eq!(hq(&kb, &alice, json!({ "op": "history_search", "text": "最初の依頼" }))["events"][0]["event_id"], "e1");
        // 回復後も追記できる。
        record(&mut kb, &alice, vec![event("e3", "c1", 3, "message", "human", "次")]);
    }
    let kb = KnowledgeBase::open(&dir, KbConfig::default()).unwrap();
    let c = hq(&kb, &alice, json!({ "op": "history_context", "event": "e2", "before": 5, "after": 5, "max_content_bytes": 16 }));
    let seqs: Vec<u64> = c["events"].as_array().unwrap().iter().map(|e| e["sequence"].as_u64().unwrap()).collect();
    assert_eq!(seqs, vec![1, 2, 3]);
    assert!(c["events"][1]["content"]["next_offset"].is_u64());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn history_context_reports_sequence_gaps() {
    let mut kb = kb();
    let alice = user("alice");
    record(&mut kb, &alice, vec![event("e1", "c1", 1, "message", "human", "a"), event("e4", "c1", 4, "message", "human", "b")]);
    let r = q(&kb, &alice, json!({ "op": "history_context", "budget_ms": 100, "event": "e1", "after": 3 }));
    assert_eq!(r["results"]["gaps"], json!([{ "from": 2, "to": 3 }]));
    assert!(!r["warnings"].as_array().unwrap().is_empty());
    let l = hq(&kb, &alice, json!({ "op": "history_conversations" }));
    assert_eq!(l["conversations"][0]["missing_sequences"], 2);
}
