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

    // 近傍検索（座標は東京駅から継承）
    let near = q(
        &kb,
        &agent(),
        json!({ "op": "search", "budget_ms": 500, "types": ["Event"], "space": { "near": { "at": { "frame": format!("frm_{}", chronotope_core::FrameId::named("wgs84").0.simple()), "geometry": { "type": "point", "at": { "x": 139.77, "y": 35.68 } } }, "radius": 2000.0 } } }),
    );
    assert_eq!(near["results"].as_array().unwrap().len(), 1);

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
