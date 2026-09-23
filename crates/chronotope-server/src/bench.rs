//! 合成データによるレイテンシ測定（Tier 0: ID 参照、Tier 1: 構造・時空間・テキスト・ベクトル検索）。

use chronotope_core::model::*;
use chronotope_core::time::Tick;
use chronotope_core::time::expr::TemporalExpression;
use chronotope_core::vocab::{predicate_id, type_id};
use chronotope_core::*;
use chronotope_engine::command::{Command, Revision};
use chronotope_engine::{KbConfig, KnowledgeBase};
use serde_json::json;
use std::collections::BTreeSet;
use std::time::Instant;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const NAMES: &[&str] = &["佐藤", "鈴木", "高橋", "田中", "伊藤", "渡辺", "山本", "中村", "小林", "加藤", "Smith", "Johnson", "Garcia", "Müller", "Rossi"];
const WORDS: &[&str] =
    &["祭り", "会議", "地震", "選挙", "発表会", "試合", "展示", "公演", "事故", "記念式典", "summit", "launch", "concert", "festival", "storm"];

struct Loader<'a> {
    kb: &'a mut KnowledgeBase,
    cmds: Vec<Command>,
    rev: RevisionId,
    now: Tick,
}

impl Loader<'_> {
    fn flush(&mut self) {
        if self.cmds.is_empty() {
            return;
        }
        let rev = Revision {
            id: self.rev,
            seq: 0,
            branch: BranchId::main(),
            actor: ActorRef::system(),
            message: Some("bench load".into()),
            committed_at: self.now,
            commands: std::mem::take(&mut self.cmds),
        };
        self.kb.commit_prepared(rev).expect("commit");
        self.rev = RevisionId::new();
    }

    fn push(&mut self, c: Command) {
        self.cmds.push(c);
        if self.cmds.len() >= 5_000 {
            self.flush();
        }
    }

    fn resource(&mut self, types: &[&str], label: String) -> ResourceId {
        let id = ResourceId::new();
        let types: BTreeSet<ResourceId> = types.iter().map(|t| type_id(t)).collect();
        self.push(Command::CreateResource {
            resource: Resource {
                id,
                types,
                labels: vec![Label::preferred(&label, Some("ja"))],
                descriptions: vec![],
                external_ids: vec![],
                visibility: Visibility::Public,
                license: None,
                created_at: self.now,
                created_by: ActorRef::system(),
                created_revision: self.rev,
            },
        });
        id
    }

    fn assertion(&mut self, s: ResourceId, pred: &str, object: Value) {
        let a = Assertion {
            id: AssertionId::new(),
            subject: s,
            predicate: predicate_id(pred),
            object,
            polarity: Polarity::Affirmed,
            valid_time: None,
            spatial_scope: None,
            branch: BranchId::main(),
            timeline: None,
            canon: None,
            status: AssertionStatus::Accepted,
            confidence: ConfidenceComponents::default(),
            rank: None,
            evidence: vec![],
            supersedes: None,
            superseded_by: None,
            overrides: None,
            asserted_by: ActorRef::system(),
            created_revision: self.rev,
            created_at: self.now,
            first_known_at: self.now,
            status_history: vec![],
            visibility: Visibility::Public,
            license: None,
            note: None,
        };
        self.push(Command::AddAssertion { assertion: a });
    }
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i]
}

fn report(name: &str, mut v: Vec<f64>, truncated: usize, results: usize) {
    let n = v.len();
    let mean = v.iter().sum::<f64>() / n.max(1) as f64;
    println!(
        "{name:<34} n={n:<6} mean={mean:>7.3}ms p50={:>7.3}ms p95={:>7.3}ms p99={:>7.3}ms truncated={truncated} avg_results={:.1}",
        pct(&mut v, 0.50),
        pct(&mut v, 0.95),
        pct(&mut v, 0.99),
        results as f64 / n.max(1) as f64
    );
}

pub fn run(events: usize, queries: usize, seed: u64, export_sql: Option<std::path::PathBuf>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut rng = Rng(seed.max(1));
    let mut kb = KnowledgeBase::in_memory(KbConfig::default());
    let now = kb.now();
    let t0 = Instant::now();
    let n_regions = 50usize;
    let n_cities = 1_000usize;
    let n_people = (events / 10).max(100);
    let (regions, cities, people, evs) = {
        let mut l = Loader { kb: &mut kb, cmds: vec![], rev: RevisionId::new(), now };
        let world = l.resource(&["World"], "地球".into());
        let regions: Vec<ResourceId> = (0..n_regions).map(|i| l.resource(&["Region"], format!("地域{i}"))).collect();
        for r in &regions {
            l.assertion(*r, "located_in", Value::Resource(world));
        }
        let wgs = FrameId::named("wgs84");
        let mut cities = vec![];
        for i in 0..n_cities {
            let c = l.resource(&["City"], format!("都市{i}"));
            l.assertion(c, "located_in", Value::Resource(regions[i % n_regions]));
            let (x, y) = (122.0 + (rng.below(2400) as f64) / 100.0, 24.0 + (rng.below(2100) as f64) / 100.0);
            l.assertion(
                c,
                "coordinates",
                Value::Geo(chronotope_core::space::Placement {
                    frame: wgs,
                    geometry: chronotope_core::space::Geometry::Point { at: chronotope_core::space::Point::xy(x, y) },
                }),
            );
            cities.push(c);
        }
        let people: Vec<ResourceId> = (0..n_people).map(|i| l.resource(&["Person"], format!("{}{}", NAMES[i % NAMES.len()], i))).collect();
        let mut evs = vec![];
        for i in 0..events {
            let e = l.resource(&["Event"], format!("{}{} {}", WORDS[rng.below(WORDS.len())], i, WORDS[rng.below(WORDS.len())]));
            let (y, m, d) = (1900 + rng.below(127), 1 + rng.below(12), 1 + rng.below(28));
            l.assertion(e, "occurred_at", Value::Time(TemporalExpression::parse(&format!("{y}-{m:02}-{d:02}"), "gregorian")));
            l.assertion(e, "took_place_at", Value::Resource(cities[rng.below(n_cities)]));
            for _ in 0..(1 + rng.below(3)) {
                let p = people[rng.below(people.len())];
                l.assertion(p, "participated_in", Value::Resource(e));
            }
            evs.push(e);
        }
        l.flush();
        (regions, cities, people, evs)
    };
    let load = t0.elapsed();
    let t1 = Instant::now();
    let rows = kb.materialize_all()?;
    let mat = t1.elapsed();
    println!("== chronotope bench (in-memory engine) ==");
    println!("resources: {} (events {events}, people {}, cities {n_cities}, regions {n_regions})", kb.store().resources.len(), people.len());
    println!("assertions: {}", kb.store().assertions.len());
    println!("canonical load: {:.2}s ({:.0} assertions/s)", load.as_secs_f64(), kb.store().assertions.len() as f64 / load.as_secs_f64());
    println!("materialize: {rows} rows in {:.2}s ({:.0} rows/s)", mat.as_secs_f64(), rows as f64 / mat.as_secs_f64());
    let _ = cities;
    let p = Principal::anonymous();

    let mut run = |name: &str, mk: &mut dyn FnMut(&mut Rng) -> String| {
        let mut lat = vec![];
        let mut trunc = 0;
        let mut results = 0;
        for _ in 0..queries {
            let body = mk(&mut rng);
            let t = Instant::now();
            let r = kb.query_json(&p, &body).expect("query");
            lat.push(t.elapsed().as_secs_f64() * 1000.0);
            trunc += r.truncated as usize;
            results += match &r.results {
                serde_json::Value::Array(a) => a.len(),
                serde_json::Value::Object(o) => o.get("matches").and_then(|m| m.as_array()).map(|a| a.len()).unwrap_or(1),
                _ => 0,
            };
        }
        report(name, lat, trunc, results);
    };
    run("tier0 lookup(id)", &mut |r| json!({ "op": "lookup", "budget_ms": 50, "id": evs[r.below(evs.len())] }).to_string());
    run("tier1 search(type+time+region)", &mut |r| {
        let (y, m) = (1900 + r.below(127), 1 + r.below(12));
        json!({ "op": "search", "budget_ms": 800, "types": ["Event"], "time": { "expression": format!("{y}年{m}月") }, "space": { "within_place": regions[r.below(regions.len())] }, "limit": 20 }).to_string()
    });
    run("tier1 search(time decade, rank)", &mut |r| {
        let y = 1900 + r.below(12) * 10;
        json!({ "op": "search", "budget_ms": 800, "types": ["Event"], "time": { "from": format!("{y}"), "to": format!("{}", y + 9) }, "order_by": "rank", "limit": 20 }).to_string()
    });
    run("tier1 search(entity)", &mut |r| json!({ "op": "search", "budget_ms": 800, "entities": [people[r.below(people.len())]], "limit": 20 }).to_string());
    run("tier1 search(text)", &mut |r| {
        json!({ "op": "search", "budget_ms": 800, "text": WORDS[r.below(WORDS.len())], "types": ["Event"], "limit": 20 }).to_string()
    });
    run("tier1 search(vector text)", &mut |r| {
        json!({ "op": "search", "budget_ms": 800, "vector": { "text": format!("{} {}", WORDS[r.below(WORDS.len())], WORDS[r.below(WORDS.len())]) }, "limit": 10 }).to_string()
    });
    run("tier1 expand_claims", &mut |r| json!({ "op": "expand_claims", "budget_ms": 800, "id": evs[r.below(evs.len())] }).to_string());
    if let Some(path) = export_sql {
        let t = Instant::now();
        let mut out = std::io::BufWriter::new(std::fs::File::create(&path)?);
        kb.export_sql(&mut out, BranchId::main())?;
        println!("exported SQL to {} in {:.2}s", path.display(), t.elapsed().as_secs_f64());
    }
    Ok(())
}
