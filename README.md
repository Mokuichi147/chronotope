# Chronotope — 汎用時空間ナレッジ DB

現実世界・フィクション・ゲーム・仮想空間を区別せず、「何が・いつ・どこで・誰と関係し、どの情報源から、
外部エージェントがいつ知り、どの版・世界線において成立するのか」を元データとの由来を失わずに保持し、
AI エージェントから時空間・関係・意味・出典を組み合わせて高速かつ安全に検索するための Rust 実装です。

```text
Raw Data → Acquisition / Provenance → Canonical Knowledge Model
         → Resolver / Materializer → Search Projection → Indexes → Agent API
```

Canonical 層（正しさ・表現力優先）と Search Projection（検索速度優先・非正規化）を分離しています。
Canonical Graph を検索エンジンとしては使わず、検索は必ず Projection と索引を経由します。

## クレート構成

| クレート | 役割 |
|---|---|
| `crates/chronotope-core` | I/O を持たないドメインモデル。Resource / Assertion / Provenance / Identity / Revision / Work / Observation / Trajectory、時間モデル（パーサ・4 点境界・暦・Allen 代数・部分順序グラフ）、空間モデル（参照系・変換・距離）、ランキング方針、組み込み語彙 |
| `crates/chronotope-engine` | Canonical ストア、Revision ログ（WAL）、Object Storage、crypto-shredding、Resolver / Materializer（無効化キュー）、Search Projection と索引（時間・空間・転置・全文・ベクトル）、Agent Query DSL、意味的書き込み API、RDF / SQL エクスポート |
| `crates/chronotope-extract` | テキストからの抽出パイプライン（抽出の中間形式、規則ベースの日本語抽出器、既存 Resource との照合、書き込み API への変換） |
| `crates/chronotope-server` | CLI（`serve` / `bench` / `demo` / `query` / `ingest-text` / `export-sql` / `export-rdf`）と HTTP Agent API |
| `sql/migrations` | PostgreSQL / Citus スキーマ（Expand → Migrate → Contract、RLS、分散キー設計） |

## 使い方

```bash
cargo run --release --bin chronotope -- demo
```

```bash
cargo run --release --bin chronotope -- serve --data ./data
```

```bash
cargo run --release --bin chronotope -- bench --events 100000
```

開発用に `serve --in-memory --seed-demo` でデモデータ入りのインメモリサーバを起動できます。

### HTTP API

| メソッド | パス | 内容 |
|---|---|---|
| POST | `/v1/query` | JSON Query DSL（`budget_ms` 必須） |
| POST | `/v1/write` | 意味的書き込み API（`op` で操作を指定） |
| GET | `/v1/resources/{id}` | Tier 0 参照 |
| GET | `/v1/freshness?branch=` | Projection の鮮度 |
| GET | `/v1/export/rdf?branch=` | N-Triples（承認済み・再配布可能な主張のみ） |
| POST | `/v1/materialize` | Materializer の即時実行 |
| POST | `/v1/ingest/text` | テキストから主張を抽出して取り込む（`{"document": {...}, "extraction": {...}?, "dry_run": false}`） |

主体はヘッダ `x-chronotope-principal` / `x-chronotope-kind`（`agent` / `human` / `crawler` / `sensor`）/
`x-chronotope-groups` / `x-chronotope-curator` / `x-chronotope-on-behalf-of` から作ります。認証は前段のゲートウェイで行う前提です
（ヘッダが無ければ匿名エージェント。AI エージェントはキュレーターになれません）。
`x-chronotope-on-behalf-of` は、所有者から委任されたエージェントが所有者の非公開データ（会話履歴など）を扱うためのもので、
ゲートウェイが委任を確認したときだけ付けます。主体の種別や権限は変わりません。
リクエスト本文の上限は `serve --max-body-bytes`（既定 64 MiB）です。

```bash
curl -s localhost:7878/v1/query -H 'content-type: application/json' -d '{
  "op": "search", "budget_ms": 200,
  "types": ["Event"],
  "time": { "expression": "2026年9月", "mode": "possibly" },
  "space": { "within_place": "wikidata:Q17" },
  "entities": ["res_..."],
  "as_known_at": "2026-09-22T00:00:00Z"
}'
```

```bash
curl -s localhost:7878/v1/write -H 'content-type: application/json' -H 'x-chronotope-principal: extractor-1' -d '{
  "op": "propose_assertion",
  "subject": { "new": { "types": ["Event"], "label": "駅前イベント", "lang": "ja" } },
  "predicate": "occurred_at",
  "object": { "time": "9月20日15時30分", "calendar": "gregorian+09:00" },
  "evidence": [{ "acquisition": "acq_...", "derivation": { "extractor": "news", "model": "llm-x", "model_version": "2026-08", "extraction_conf": 0.9 } }]
}'
```

### 読み取り操作（`op`）

| op | Tier | 内容 |
|---|---|---|
| `lookup` | 0 | ID・外部 ID・ラベル完全一致。複数一致は `ambiguous: true` で確定しない。旧 ID は `redirected_from` 付きで統合先を返す |
| `search` | 1 | 型・関係・作品・場所（包含階層 / bbox / 半径）・時間窓（possibly / certainly / within）・全文・ベクトル・canon / timeline / 過去時点。Level 1 要約（約 80 トークン） |
| `similar` | 1 | 意味・時間・空間・実体重複・作品・グラフ距離・ランクの多特徴類似 |
| `expand_claims` | 1 | Level 2: 述語ごとの優先値・異説・否定・履歴・被参照・同一性候補 |
| `get_acquisition` | 1 | Level 3: 出典・取得（acquired_at）・抽出（モデル・版・スパン）・スナップショット（ライセンスと出典の可視性で制御。`snapshot_offset` / `snapshot_length` で全文をページ取得） |
| `history_search` / `history_get` / `history_context` / `history_conversations` | 1 | 会話履歴の検索 / 原文と関連イベント / 前後の記録 / 会話の一覧（[会話履歴](#会話履歴)） |
| `temporal_relation` | 1 | 2 つの出来事のあり得る Allen 関係。時間軸が違えば `comparable: false` |
| `timeline` | 1 | 時間軸ごとの年表と、絶対時刻の無い出来事の部分順序 |
| `neighbors` | 1 | 関係の近傍探索（深さ 3 まで） |
| `observations` / `position_at` / `sequence` | 1 | 観測値履歴 / 軌跡の補間位置 / 作品の順序 |
| `resolve_temporal` / `resolve_spatial` | 1 | 時間表現の解析・解決 / 場所名・座標から候補（自動選択しない） |
| `conflicts` / `merge_candidates` / `derived_by` | 2 | 異説・時間順序の矛盾 / 統合候補 / 抽出器・モデル版ごとの主張 |
| `freshness` / `vocabulary` / `revisions` / `table_rows` | 0–1 | 鮮度 / 語彙 / 変更履歴 / 表データ |

予算を超過した場合は途中結果を `"truncated": true` で返します。応答には常に Projection の鮮度
（`source_revision` / `materialized_revision` / `stale_rows` / `consistent`）が付きます。

### 書き込み操作（`op`）

`propose_assertion` / `retract_assertion` / `supersede_assertion` / `dispute_assertion` / `merge_identity` /
`link_source` / `record_event` / `record_events` / `add_evidence` / `add_observation` / `add_trajectory` / `propose_predicate` / `propose_type` /
`create_resource` / `update_resource` / `create_branch` / `define_sequence` / `define_table` / `add_table_rows` /
`link_row` / `set_embedding` / `create_key`、キュレーター専用の `accept_assertion` / `verify_assertion` /
`decide_merge` / `accept_predicate` / `define_calendar` / `define_frame` / `define_license` / `set_rank_policy` /
`shred_key`。

`link_source` は `"visibility": {"level": "private", "owner": "<自分>"}` などで非公開の出典を登録できます。
非公開の出典は同じ URL でも所有者・グループごとに別の Source になり、取得記録・スナップショット・根拠としての参照は
閲覧できる主体に限られます（Acquisition ID を直接指定しても、見えなければ `not_found`）。

## 会話履歴

AI エージェントとのやり取り（入力・応答・ツールの呼び出しと結果）を原文のまま保存し、後から正確に参照するための機能です。
要約や類似検索は候補探しに使い、回答・引用は取得した原文に基づけることを前提にしています。

- 1 イベント = 1 回の発言・ツール呼び出し・結果。会話は所有者だけが見える Source、イベントはその Acquisition になり、
  原文はバイト列のままスナップショットとして保存します（整形・正規化しない）。要約・抽出した知識はこの Acquisition を根拠に参照できます。
- 所有者は認証済みの主体（委任されていれば委任元）で、リクエストでは指定できません。匿名の主体は記録できません。
- `event_id` はクライアントが保存前に採番します。同じ ID・同じ内容の再送は既存の記録を返し（Revision も増えない）、
  同じ ID で内容が違う場合や、会話内の記録順（`sequence`）が別のイベントと重なる場合は `conflict` です。本文が同じでも ID が違えば両方残します。
- `origin`（`human` / `model` / `tool` / `runtime` / `system` / `agent` / `unknown`）は必須で、API 上の role（`api_role`）とは別に持ちます。
  自動継続の指示は `api_role: "user"` でも `origin: "runtime"` とし、ユーザー本人の発言と区別できます。移行などで分からなければ `unknown` を明示します。
- ツールの呼び出しと結果は `call_id` で結び、`status`（`ok` / `error` / `interrupted` / `unknown`）で中断や結果不明を成功と区別します。
  訂正は `supersedes`、要約の元は `derived_from`、因果関係は `parent_event` で指し、過去のイベントは上書きしません。

```bash
curl -s localhost:7878/v1/write -H 'content-type: application/json' -H 'x-chronotope-principal: alice' -H 'x-chronotope-kind: human' -d '{
  "op": "record_events",
  "events": [
    { "event_id": "e-001", "conversation": "c-1", "sequence": 1, "kind": "message", "origin": "human", "api_role": "user",
      "received_at": "2026-09-27T10:00:00+09:00", "content": "  ビルドを消して\n" },
    { "event_id": "e-002", "conversation": "c-1", "sequence": 2, "kind": "message", "origin": "runtime", "api_role": "user",
      "content": "続けてください" }
  ]
}'
```

| op | 内容 |
|---|---|
| `history_search` | 本文（正規化後の部分一致。`exact: true` で原文どおりの連続一致）・会話・`kinds`・`origins`・`call_id`・期間で探す。一致位置（原文のバイト範囲）と抜粋、`next_cursor` を返す |
| `history_get` | イベント ID から原文を `offset` / `length` バイトずつ返す（UTF-8 の文字境界で区切り、`next_offset` が `null` になるまで読めば欠落なく復元できる。原文全体の `content_hash` 付き）。呼び出しと結果・訂正・要約元などの関連イベントも返す |
| `history_context` | イベント（または会話と記録順）の前後を記録順で返す。`conversation` だけなら末尾。記録順の欠け（未同期の可能性）を `gaps` で示す |
| `history_conversations` | 会話ごとのイベント数・最後の記録順・欠けている記録順の数（同期の再開位置の確認用） |

本文の索引はコミットと同時に更新するため、記録直後の検索にも索引の遅れはありません。UTF-8 でない本文や鍵を破棄した本文は
本文検索の対象外で、その件数を `not_text_searchable` と警告で返します。検索で見つからなかったことは、その発言が無かったことを意味しません。
CJK・英数字とも文字 bigram で候補を絞ってから原文で照合するので、語の途中から始まる検索語も取りこぼしません（1 文字だけの語は全件を照合）。

## テキストからの取り込み

記事などの非構造化テキストは、抽出の中間形式（`extract-v1`）を経由して取り込みます。エンジン自体は LLM を呼びません。

```text
文書（本文 + URL・公開日時・取得日時・ライセンス）
  → 抽出（実体・主張・観測値。時間表現は原文のまま）
  → 照合（既存 Resource へのリンク。一意に決まらなければ新規作成 + possibly_same_as）
  → 書き込み（link_source → create_resource → propose_assertion / add_observation）
```

```bash
cargo run --release --bin chronotope -- ingest-text --data ./data --url https://example.jp/news/1 --dry-run article.txt
```

- 抽出器は、組み込みの規則ベース抽出器（外部サービスを呼ばない）か、`--extraction` / `"extraction"` で渡した
  任意の抽出器（人手・ローカル LLM など）の出力を使います。
- 規則ベース抽出器は、KB 内の場所・組織・人物のラベルを辞書として使い、次のような書き方を拾います。文型に合わない書き方は取りこぼします。
  - 出来事: `〇〇が開かれ` `〇〇が発生`、事典的な主題文 `〇〇（読み）は、…で行われた戦い` `〇〇とは、…に発生した地震である`。
    報道（見出しの日付か公開日時がある文書）では、本文の最初の文に文型の出来事が無ければ見出しを主な出来事とし、
    その文の日付のうち記事の日付に最も近いものを採ります（離れていれば続く文の日付。本文に月まで書かれた日付が無ければ
    告知の見出しの `【4/12開催】` などの日付、過去形の文で日付が無ければ `記事の日付以前`）。時間も場所も得られない見出しは出来事にしません
  - 百科事典の冒頭文の書式（人物・作品・組織・施設）: `〇〇（読み、1950年1月1日 - ）は、…出身の俳優` → birth_date / death_date /
    birth_place、`『〇〇』は、2001年に公開された映画` → publication_date、`〇〇株式会社は、〇〇県〇〇市に本社を置く…` →
    located_in / inception、`〇〇駅（読み）は、〇〇県〇〇市にある…` → located_in。この書式に沿わない文章では働きません
  - 人物・組織: `〇〇氏` `〇〇大臣`、`主催した〇〇`
  - 場所: 辞書の地名、`〇〇駅` `〇〇県〇〇市` `〇〇沖` `〇〇半島` など。続けて書かれた地名は KB 上の配下関係があるときだけ細かい方へ進み、
    `〇〇で` `〇〇において` と場所を示す地名や `現在の〇〇` と書き添えられた地名を優先します。
    同名の地名は同じ文書に出てくる地名の配下にある候補に絞り、`〇〇近傍` `〇〇で` のような市町村名・旧国名の省略形も辞書と照合します。
    辞書に無い場所は、括弧で添えられた既存の地名（`〇〇空港（〇〇県〇〇市）`）の配下として記録します
  - 時間: 日付の途中の注記 `（昭和50年）` `（月曜日）` を読み飛ばし、`告示` より `執行` `発生` などが続く日付を優先します。
    年の無い日付は見出しの年で補い、前近代の元号の月日（旧暦）は採らずに併記された西暦を使います
  - 数値: `約1200人が参加`
- 時間表現は原文のまま渡し、公開日時（無ければ本文冒頭の【日付】）を基準にエンジンが解決します。
  年の無い日付（`12月31日` `2/20`）は基準時刻に最も近い年、日だけの日付は最も近い月のものとします。
  `来月10日` を `10日` と取り違えるような、修飾を落とした部分一致は採りません。
- すべての主張に、取得（本文のスナップショット）と抽出の記録（抽出器・モデル・スキーマ版・本文中の文字位置・確信度）が付き、
  取り込み主体の権限どおり proposed から始まります。語彙に無い述語は提案（proposed）されます。

中間形式の例:

```json
{
  "extractor": { "name": "my-extractor", "model": "some-local-model", "model_version": "q4" },
  "source_time": "2026-09-21",
  "entities": [
    { "ref": "E1", "types": ["Event"], "label": "東京駅の防災イベント" },
    { "ref": "P1", "types": ["Station"], "label": "東京駅", "mention": "東京駅丸の内口の広場" }
  ],
  "claims": [
    { "subject": "E1", "predicate": "occurred_at", "object": { "time": "20日午後3時半ごろ" }, "span": [12, 22], "confidence": 0.9 },
    { "subject": "E1", "predicate": "took_place_at", "object": { "ref": "P1" }, "span": [23, 33] }
  ],
  "observations": [
    { "target": "E1", "metric": "attendees", "value": 1200, "unit": "{person}", "approximate": true }
  ]
}
```

`span` は本文の文字（Unicode スカラー値）単位の `[start, end)` です。

## 仕様との対応

| 仕様 | 実装 |
|---|---|
| 2 Canonical / Projection 分離 | `canonical.rs`（Command で変更・Touch で影響範囲）→ `materialize.rs` → `projection.rs`（1 Resource = 1 フラット行 + 索引）。検索は Projection のみ |
| 3 Resource と複数 type | `Resource.types`（ResourceType 関係）＋ `instance_of` Assertion。Projection では `types_mask`（u64）と型索引 |
| 4 Assertion | subject / predicate / object / valid_time / spatial_scope / branch / timeline / canon / status / rank（`model/assertion.rs`） |
| 5 列と Assertion の境界 | ラベル・外部 ID・acquired_at・content_hash・内部 ID は列、意味情報は Assertion |
| 6 Predicate と外部語彙 | `PredicateDef`（domain / range / inverse / transitive / symmetric / functional / status / role）と OWL-Time・PROV-O・CIDOC CRM・Wikidata・UCUM への Mapping。AI は `proposed` のみ |
| 7 時間 3 層 | A: `TemporalExpression`（raw_text / AST / calendar_frame、解析失敗でも原文保存）、B: 4 点境界 `FuzzyRange`（i64 ms の UTA tick）、C: `TemporalOrderGraph`（順序ラベル、派生物） |
| 7 対応表現 | `2026-09-20 15:30`、`2026年9月頃`、`9月1日〜9月10日`、`数日前`、`先週火曜日`、`月曜日の夕方`、`毎週金曜日25:30`（day_offset 付きで 01:30 へ解決）、`A事件の3日前`、`Aより後、Bより前`、`紀元前300年`、`1980年代`、`19世紀`、`9月上旬`、`circa 1204`、`3 days ago`、`来月10日から12日まで`、`昭和50年5月5日`・`平成元年`（近代の元号。明治 5 年以前は旧暦として幅を持たせる）、`ユリウス暦1200年6月1日`（グレゴリオ暦へ換算）、`江戸時代前期`（時代区分）、`3時半ごろ`（時刻の「ごろ」は ±30 分）など |
| 8 時間関係 | Allen 13 関係のビットマスク・合成表・4 点境界からのあり得る関係（差分制約で厳密判定）。Pearce–Kelly の増分トポロジカル順序・循環検出。相対参照の深さ上限 8。変更 → 無効化キュー → 再計算 |
| 9 時間の意味の分離 | event_time（occurred_at 等）/ valid_time / source_time / observed_at / acquired_at。`as_known_at` は acquired_at と状態変化の根拠時刻で判定 |
| 10–11 Acquisition / Derivation | Source と Acquisition を分離（同 URL の再取得は別 Acquisition）。スナップショットは BLAKE3 の content-addressed Object Storage で自動重複排除。Derivation は抽出器・モデル・版・スキーマ版・スパン・確信度 |
| 12 信頼度 | source_reliability / extraction_conf / corroboration / specificity / human_verified に分解。provenance_root で転載を同一出典として数える。`computed_rank` に方針 ID・版・計算時刻 |
| 13 状態 | accepted / proposed / disputed / retracted / superseded と、別軸の肯定・否定。履歴を保持し削除しない |
| 14–15 空間 | World → Region → Area → Place → Subplace の包含階層（located_in / contains / inside）、隣接・接続・近傍・方位・間・ポータル。SpatialReferenceFrame（WGS84 / 直交 / 格子 / 独自 / 座標なし、親へのアフィン変換）。比較不能な Frame 同士は距離を返さない |
| 16 Event 間関係 | before / after / meets / overlaps / during / starts / finishes / simultaneous / part_of / causes / caused_by / alternative_of / same_as |
| 17 Identity | aliases / external_ids / same_as / possibly_same_as / distinct_from。possibly_same_as は別 Resource のまま扱い、`merge_identity`（提案）→ キュレーター `decide_merge` → identity_redirect |
| 18 Revision / Branch / Canon / Timeline | Revision は追記専用ログ（WAL、再生で復元）。Branch は copy-on-write + 親参照 + fork 通番。Canon・Timeline は Resource（timeline は diverges_from で系譜） |
| 19 Work / Series | part_of_work の作品階層（Franchise → Series → Season → Episode 等）、appears_in、adaptation_of、独立した Sequence（release / work / story / recommended order） |
| 20 表データ | Dataset → Table（Schema・主キー・外部キー・格納先 inline / parquet / csv / sql）→ Row と Resource の RowLink、セル単位の Locator |
| 21 非構造化データ | SourceKind（Web / SNS / PDF / JSON / API / ログ / 画像 / 動画 / 音声 / 字幕 …）と Locator（テキスト範囲・ページ・メディア区間・JSON Pointer・セル） |
| 22–23 Observation / Trajectory | UCUM 単位の観測値履歴、時刻 → 位置の軌跡（線形・階段補間、外部格納） |
| 24 Search Projection | resolved range / order label / space_ids / space_ancestor_ids / entity_ids / work_ids / branch_ids / canon_ids / timeline_ids / types_mask / rank / embedding_ref / projection_version / source_revision / materialized_at / stale |
| 25–26 類似・ベクトル検索 | Structured Filter + ANN → 候補 → 再順位付け。Embedding 空間ごとに model / version / dimension / generated_at を保持し、Binary → f16 → f32 の 3 段階 |
| 27–29 読み取り API | JSON DSL・`budget_ms` 必須・`truncated`・3 段階ドリルダウン・`contested` / `ambiguous` / `comparable: false` |
| 30 書き込み API | 意味的 API のみ。AI の追加は proposed から |
| 31 レイテンシ Tier | 応答の `tier`（tier0 / tier1 / tier2）。Tier 2（矛盾検出等）はオンライン検索経路と別操作 |
| 32–33 物理構成 | `sql/migrations`（PostgreSQL / Citus、テーブルごとの分散キー）、Object Storage、列指向ストアの境界 |
| 34 セキュリティ・法的要件 | Assertion・Source 単位の可視性（エンジン内判定 + PostgreSQL RLS。Acquisition・会話イベントは Source の可視性に従う）、所有者からの委任（`on_behalf_of`）、license / redistributable を API で強制（スナップショット本文・RDF 出力）、crypto-shredding（ChaCha20-Poly1305、鍵はログに書かない。破棄した本文は会話の索引からも外す） |
| 35 Schema Migration | `sql/README.md` に Expand → Migrate → Contract の手順 |

## 計測（Apple Silicon、`--release`）

インメモリエンジン（`chronotope bench`）:

| 規模 | Tier 0 lookup p95 | Tier 1 型+時間+地域 p95 | Tier 1 全文 p95 | Tier 1 ベクトル p95 | Materialize |
|---|---|---|---|---|---|
| Resource 11 万 / Assertion 40 万 | 0.008 ms | 0.019 ms | 7.8 ms | 2.5 ms | 8.2 万行/s |
| Resource 110 万 / Assertion 400 万 | 0.68 ms | 1.5 ms | 89 ms | 60 ms | 2.6 万行/s（RSS 6.7 GB） |

単一 PostgreSQL 17（Citus なし、`export-sql` で 10 万件投入、読み取りロール・4 並列の pgbench）:

| クエリ | p95 |
|---|---|
| Tier 0 `lookup()`（リダイレクト解決込み） | 0.12 ms |
| Tier 1 時間（GiST）+ 地域（GIN）+ 型 + rank 順 | 0.26 ms |
| Tier 1 関係する実体（GIN） | 0.22 ms |

Search Projection には行単位 RLS を掛けていません。RLS の security barrier が非 leakproof な範囲・配列演算子
より先に評価されて GiST / GIN が使えなくなり、Tier 1 が 20 ms 超になったためです。Projection は公開 Assertion
だけから作り、Resource 単位の可視性を条件に持つビュー経由で公開しています（RLS は Canonical 側に適用）。

## 現時点の制約と今後

- Revision ログとスナップショットは既定で書き込みごとに fsync します（`serve --no-fsync` で省略）。スナップショットを
  保存してからログを書くため、ログが存在しないオブジェクトを指すことはありません。クラッシュでログ末尾に残った書きかけの
  Revision は起動時に切り捨てます（その Revision は呼び出し元には失敗として返っていたものです）。
- 会話履歴の本文索引はメモリ上にあり、起動時に Revision ログとスナップショットから作り直します。1 リクエストで送れるイベントは 1000 件までで、
  1 イベントの本文はリクエスト本文の上限に収まる必要があります（読み出しはページ単位）。
- 稼働中のエンジンは Canonical をメモリに持ち、Revision ログ（JSONL）で永続化します。PostgreSQL / Citus は
  スキーマと COPY 形式の一括エクスポートまでで、エンジンが直接読み書きするバックエンドはまだありません。
  Phase 0 の「Citus で 1000 万件」は未計測です（インメモリ 100 万件・単一 PostgreSQL 10 万件で計測）。
- DuckDB / Parquet は境界（`TableStorage`、`ObservationStore`、`TrajectoryStorage::External`）のみで、読み出しは未実装です。
- 組み込みの規則ベース抽出器は日本語の典型的な文型のみ対応です。精度が必要な場合は、同じ中間形式を出力する抽出器に置き換えてください。
- 組み込みの Embedding は字句ベースの特徴ハッシュです。意味的な類似検索には外部モデルのベクトルを
  `set_embedding` で登録してください（空間ごとにモデル・版・次元を分離して保持します）。
- ブランチの Projection は初回の `materialize_branch` で全件を計算します。時間順序グラフは関係の撤回時に全体を作り直します。
- 繰り返し表現はグレゴリオ暦のみ対応です。RDF 出力は承認済み・肯定・再配布可能な主張の N-Triples のみです。
- 書き込みはキューを挟まず直接適用しています（一括投入で 23〜34 万 Assertion/s）。仕様どおり、実測で必要になった時点で Queue 層を検討します。
