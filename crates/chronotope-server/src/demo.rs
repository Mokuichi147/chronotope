//! デモデータ（現実世界・架空世界・ゲーム内暦・作品階層・異説）と代表的なクエリ。

use chronotope_core::FrameId;
use chronotope_core::model::Principal;
use chronotope_core::time::Tick;
use chronotope_engine::{KbConfig, KnowledgeBase, WriteRequest};
use serde_json::{Value, json};

type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn w(kb: &mut KnowledgeBase, p: &Principal, body: Value) -> R<Value> {
    let req: WriteRequest = serde_json::from_value(body)?;
    Ok(kb.write(p, req)?)
}

fn create(kb: &mut KnowledgeBase, types: &[&str], label: &str, extra: Value) -> R<String> {
    let mut r = json!({ "types": types, "label": label, "lang": "ja" });
    if let (Value::Object(m), Value::Object(e)) = (&mut r, extra) {
        m.extend(e);
    }
    Ok(w(kb, &Principal::curator("demo-curator"), json!({ "op": "create_resource", "resource": r }))?["id"].as_str().unwrap_or_default().to_string())
}

fn accept(kb: &mut KnowledgeBase, s: &str, p: &str, o: Value) -> R<Value> {
    w(kb, &Principal::curator("demo-curator"), json!({ "op": "propose_assertion", "subject": s, "predicate": p, "object": o, "status": "accepted" }))
}

/// デモデータを投入する。
pub fn seed(kb: &mut KnowledgeBase) -> R<()> {
    let agent = Principal::agent("demo-agent");
    let wgs = FrameId::named("wgs84").to_string();
    let japan = create(kb, &["Country"], "日本", json!({ "external_ids": ["wikidata:Q17"], "aliases": ["Japan"] }))?;
    let tokyo = create(kb, &["City"], "東京", json!({ "external_ids": ["wikidata:Q1490"], "aliases": ["Tokyo"] }))?;
    let station =
        create(kb, &["Station", "Building", "TransportFacility"], "東京駅", json!({ "external_ids": ["wikidata:Q801124"], "aliases": ["Tokyo Station"] }))?;
    let tower = create(kb, &["Building"], "東京タワー", json!({ "aliases": ["Tokyo Tower"], "description": "東京都港区の電波塔" }))?;
    accept(kb, &tokyo, "located_in", json!({ "resource": japan }))?;
    accept(kb, &station, "located_in", json!({ "resource": tokyo }))?;
    accept(kb, &tower, "located_in", json!({ "resource": tokyo }))?;
    accept(kb, &station, "coordinates", json!({ "geo": { "frame": wgs, "geometry": { "type": "point", "at": { "x": 139.7671, "y": 35.6812 } } } }))?;
    accept(kb, &tower, "coordinates", json!({ "geo": { "frame": wgs, "geometry": { "type": "point", "at": { "x": 139.7454, "y": 35.6586 } } } }))?;
    accept(kb, &tower, "height", json!({ "quantity": 333, "unit": "m" }))?;

    // Web 記事から AI が抽出したイベント（proposed）
    let src = w(
        kb,
        &agent,
        json!({
            "op": "link_source", "url": "https://example.com/news/2026-09-21", "kind": "web_page", "title": "駅前イベント開催",
            "source_time": "2026-09-21T09:00+09:00", "origin": "primary", "license": "CC-BY-4.0",
            "acquisition": { "acquired_at": "2026-09-21T01:23:00Z", "content": "9月20日15時30分、東京駅前でイベントが開かれ、Aliceが参加した。" }
        }),
    )?;
    let acq = src["acquisition"].as_str().unwrap_or_default().to_string();
    let alice = create(kb, &["Person"], "Alice", json!({}))?;
    let ev = w(
        kb,
        &agent,
        json!({
            "op": "propose_assertion",
            "subject": { "new": { "types": ["Event"], "label": "駅前イベント", "lang": "ja" } },
            "predicate": "occurred_at", "object": { "time": "9月20日15時30分", "calendar": "gregorian+09:00" },
            "evidence": [{ "acquisition": acq, "derivation": { "extractor": "news-extractor", "model": "extractor-llm", "model_version": "2026-08", "schema_version": "v1", "source_span": { "type": "text_span", "start": 0, "end": 30 }, "extraction_conf": 0.92 } }]
        }),
    )?;
    let event = ev["subject"].as_str().unwrap_or_default().to_string();
    w(
        kb,
        &agent,
        json!({ "op": "propose_assertion", "subject": event, "predicate": "took_place_at", "object": { "resource": station }, "evidence": [{ "acquisition": acq }] }),
    )?;
    w(
        kb,
        &agent,
        json!({ "op": "propose_assertion", "subject": alice, "predicate": "participated_in", "object": { "resource": event }, "evidence": [{ "acquisition": acq }] }),
    )?;
    w(
        kb,
        &agent,
        json!({ "op": "add_observation", "target": event, "metric": "attendees", "observed_at": "2026-09-20T07:00:00Z", "value": 1200.0, "unit": "{person}", "acquisition": acq }),
    )?;

    // 異説（年代の食い違い）
    let battle = create(kb, &["Event"], "ある合戦", json!({}))?;
    let s1 = w(
        kb,
        &agent,
        json!({ "op": "link_source", "url": "https://example.org/chronicle", "kind": "book", "origin": "primary", "acquisition": { "acquired_at": "2026-01-01T00:00:00Z" } }),
    )?;
    let s2 = w(
        kb,
        &agent,
        json!({ "op": "link_source", "url": "https://example.org/encyclopedia", "kind": "web_page", "origin": "secondary", "acquisition": { "acquired_at": "2026-02-01T00:00:00Z" } }),
    )?;
    w(
        kb,
        &agent,
        json!({ "op": "propose_assertion", "subject": battle, "predicate": "occurred_at", "object": { "time": "1203年" }, "evidence": [{ "acquisition": s1["acquisition"] }] }),
    )?;
    w(
        kb,
        &agent,
        json!({ "op": "propose_assertion", "subject": battle, "predicate": "occurred_at", "object": { "time": "1204年頃" }, "evidence": [{ "acquisition": s2["acquisition"] }] }),
    )?;

    // 架空世界（ゲーム内暦・独自座標系・作品階層・部分順序のみの出来事）
    let curator = Principal::curator("demo-curator");
    w(
        kb,
        &curator,
        json!({ "op": "define_calendar", "frame": { "key": "valley", "name": "Valley calendar", "axis": "valley-world", "kind": { "type": "uniform", "epoch": 0, "ticks_per_day": 1_200_000, "days_per_month": 28, "months_per_year": 4, "days_per_week": 7 } } }),
    )?;
    let map_frame = FrameId::named("valley-map").to_string();
    w(
        kb,
        &curator,
        json!({ "op": "define_frame", "frame": { "id": map_frame, "name": "Valley map", "coordinate_system": { "type": "grid", "cell": 1.0 }, "dimensionality": 2, "unit": "[tile]" } }),
    )?;
    let game = create(kb, &["Game"], "Valley Story", json!({}))?;
    let valley = create(kb, &["World"], "谷の世界", json!({}))?;
    let town = create(kb, &["Area"], "谷の町", json!({}))?;
    let dungeon = create(kb, &["Dungeon"], "古代の坑道", json!({}))?;
    accept(kb, &town, "located_in", json!({ "resource": valley }))?;
    accept(kb, &dungeon, "located_in", json!({ "resource": valley }))?;
    accept(kb, &town, "portal_to", json!({ "resource": dungeon }))?;
    accept(kb, &town, "coordinates", json!({ "geo": { "frame": map_frame, "geometry": { "type": "point", "at": { "x": 40.0, "y": 12.0 } } } }))?;
    let fest = create(kb, &["Event"], "収穫祭", json!({}))?;
    accept(kb, &fest, "occurred_at", json!({ "time": "1年3月16日", "calendar": "valley" }))?;
    accept(kb, &fest, "took_place_at", json!({ "resource": town }))?;
    accept(kb, &fest, "appears_in", json!({ "resource": game }))?;
    let (p1, p2, p3) =
        (create(kb, &["Event"], "封印の解放", json!({}))?, create(kb, &["Event"], "坑道の崩落", json!({}))?, create(kb, &["Event"], "町の再建", json!({}))?);
    for (a, b) in [(&p1, &p2), (&p2, &p3)] {
        accept(kb, a, "before", json!({ "resource": b }))?;
    }
    for e in [&p1, &p2, &p3] {
        accept(kb, e, "appears_in", json!({ "resource": game }))?;
    }
    Ok(())
}

pub fn run() -> R<()> {
    let mut kb = KnowledgeBase::in_memory(KbConfig::default());
    kb.set_clock(|| Tick::from_civil(2026, 9, 23, 0, 0, 0, 0));
    seed(&mut kb)?;
    kb.materialize_all()?;
    let p = Principal::agent("reader");
    let show = |title: &str, q: Value| -> R<Value> {
        let r = kb.query_json(&p, &q.to_string())?;
        println!("\n### {title}\n>>> {q}\n{}", serde_json::to_string_pretty(&r)?);
        Ok(serde_json::to_value(r)?)
    };
    let s = show(
        "時空間検索: 2026年9月・日本国内の出来事",
        json!({ "op": "search", "budget_ms": 200, "types": ["Event"], "time": { "expression": "2026年9月" }, "space": { "within_place": "wikidata:Q17" } }),
    )?;
    let ev = s["results"][0]["id"].clone();
    show("Level 2: 主張と異説", json!({ "op": "expand_claims", "budget_ms": 200, "id": ev }))?;
    let lk = show("異説のある出来事（contested）", json!({ "op": "lookup", "budget_ms": 50, "label": "ある合戦" }))?;
    let battle = lk["results"]["matches"][0]["id"].clone();
    let c = kb.query_json(&p, &json!({ "op": "expand_claims", "budget_ms": 200, "id": battle }).to_string())?;
    let aid = serde_json::to_value(&c)?["results"]["claims"][0]["preferred"]["id"].clone();
    show("Level 3: 出典・取得・抽出", json!({ "op": "get_acquisition", "budget_ms": 200, "assertion": aid }))?;
    show(
        "比較不能な暦",
        json!({ "op": "temporal_relation", "budget_ms": 100, "a": lk["results"]["matches"][0]["id"], "b": kb.query_json(&p, &json!({ "op": "lookup", "budget_ms": 50, "label": "収穫祭" }).to_string()).map(|r| r.results["matches"][0]["id"].clone())? }),
    )?;
    show("部分順序だけの年表", json!({ "op": "timeline", "budget_ms": 200 }))?;
    show(
        "過去時点検索（2026-01-15 時点で知り得た情報）",
        json!({ "op": "expand_claims", "budget_ms": 200, "id": battle, "as_known_at": "2026-01-15T00:00:00Z" }),
    )?;
    Ok(())
}
