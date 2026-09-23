-- phase: expand
-- Canonical Knowledge Layer（正しさ・表現力優先）。
-- 時刻はすべて int8 の UTA tick（1 tick = 1 ms, epoch = 1970-01-01T00:00:00Z）。
-- ±∞ は NULL（範囲型では非有界端点）で表す。

CREATE SCHEMA IF NOT EXISTS chronotope;
SET search_path = chronotope, public;

CREATE EXTENSION IF NOT EXISTS btree_gist;

DO $$ BEGIN
    CREATE TYPE assertion_status AS ENUM ('accepted', 'proposed', 'disputed', 'retracted', 'superseded');
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
    CREATE TYPE vocab_status AS ENUM ('proposed', 'accepted', 'deprecated');
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
    CREATE TYPE visibility_level AS ENUM ('public', 'groups', 'private');
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

-- ------------------------------------------------------------ Revision / Branch
-- Revision は Immutable（UPDATE / DELETE をトリガで禁止）。
CREATE TABLE IF NOT EXISTS revision (
    id            uuid        NOT NULL,
    seq           bigint      NOT NULL,
    branch_id     uuid        NOT NULL,
    actor_id      text        NOT NULL,
    actor_kind    text        NOT NULL,
    message       text,
    committed_at  bigint      NOT NULL,
    commands      jsonb       NOT NULL,
    PRIMARY KEY (id)
);
CREATE UNIQUE INDEX IF NOT EXISTS revision_seq_idx ON revision (id, seq);
CREATE INDEX IF NOT EXISTS revision_branch_seq_idx ON revision (branch_id, seq);

CREATE OR REPLACE FUNCTION forbid_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% is immutable (% on %)', TG_TABLE_NAME, TG_OP, TG_TABLE_NAME;
END $$;
DROP TRIGGER IF EXISTS revision_immutable ON revision;
CREATE TRIGGER revision_immutable BEFORE UPDATE OR DELETE ON revision
    FOR EACH ROW EXECUTE FUNCTION forbid_mutation();

CREATE TABLE IF NOT EXISTS branch (
    id           uuid   PRIMARY KEY,
    name         text   NOT NULL UNIQUE,
    parent_id    uuid,
    fork_seq     bigint NOT NULL,           -- 親ブランチのこの通番までが見える（copy-on-write）
    kind         text   NOT NULL DEFAULT 'data',
    created_at   bigint NOT NULL,
    description  text
);

-- ------------------------------------------------------------ Vocabulary（参照テーブル）
CREATE TABLE IF NOT EXISTS type_def (
    id        uuid PRIMARY KEY,
    key       text NOT NULL UNIQUE,
    parents   uuid[] NOT NULL DEFAULT '{}',
    labels    jsonb NOT NULL DEFAULT '[]',
    status    vocab_status NOT NULL,
    mask_bit  smallint                      -- search_projection.types_mask のビット位置（最大 64）
);

CREATE TABLE IF NOT EXISTS predicate (
    id          uuid PRIMARY KEY,
    key         text NOT NULL UNIQUE,
    labels      jsonb NOT NULL DEFAULT '[]',
    domain      uuid[] NOT NULL DEFAULT '{}',
    range_spec  jsonb NOT NULL,
    inverse_id  uuid,
    is_transitive boolean NOT NULL DEFAULT false,
    is_symmetric  boolean NOT NULL DEFAULT false,
    is_functional boolean NOT NULL DEFAULT false,
    status      vocab_status NOT NULL,       -- AI は proposed までしか作れない（API 層で強制）
    role        text NOT NULL,
    allen       text
);

-- 外部標準（OWL-Time / PROV-O / CIDOC CRM / Wikidata / UCUM ...）との対応。
CREATE TABLE IF NOT EXISTS vocab_mapping (
    owner_id    uuid NOT NULL,               -- predicate.id または type_def.id
    vocabulary  text NOT NULL,
    iri         text NOT NULL,
    match_kind  text NOT NULL,
    PRIMARY KEY (owner_id, vocabulary, iri)
);

CREATE TABLE IF NOT EXISTS calendar_frame (
    key   text PRIMARY KEY,
    name  text NOT NULL,
    axis  text NOT NULL,                     -- 同じ軸の暦同士だけが比較可能
    kind  jsonb NOT NULL
);

CREATE TABLE IF NOT EXISTS spatial_frame (
    id                 uuid PRIMARY KEY,
    name               text NOT NULL,
    parent_id          uuid,
    coordinate_system  jsonb NOT NULL,
    dimensionality     smallint NOT NULL,
    unit               text,
    transform          float8[]              -- 3x4 アフィン行列（行優先）。NULL なら親と座標比較不能
);

CREATE TABLE IF NOT EXISTS license (
    key                   text PRIMARY KEY,
    name                  text NOT NULL,
    url                   text,
    redistributable       boolean NOT NULL,
    attribution_required  boolean NOT NULL DEFAULT false
);

-- ------------------------------------------------------------ Resource / Identity
CREATE TABLE IF NOT EXISTS resource (
    id                 uuid   PRIMARY KEY,
    labels             jsonb  NOT NULL DEFAULT '[]',
    descriptions       jsonb  NOT NULL DEFAULT '[]',
    visibility_level   visibility_level NOT NULL DEFAULT 'public',
    visibility_groups  text[] NOT NULL DEFAULT '{}',
    visibility_owner   text,
    license            text,
    created_at         bigint NOT NULL,
    created_by         text   NOT NULL,
    created_revision   uuid   NOT NULL
);

-- Resource と Type の関係（1 Resource が複数 Type を持てる）。
CREATE TABLE IF NOT EXISTS resource_type (
    resource_id  uuid NOT NULL,
    type_id      uuid NOT NULL,
    PRIMARY KEY (resource_id, type_id)
);

-- ラベル索引（正規化文字列で分散し、名前参照・曖昧性判定に使う）。
CREATE TABLE IF NOT EXISTS label_index (
    norm         text NOT NULL,
    resource_id  uuid NOT NULL,
    text         text NOT NULL,
    lang         text NOT NULL DEFAULT '',
    kind         text NOT NULL,
    PRIMARY KEY (norm, resource_id, text, lang)
);

CREATE TABLE IF NOT EXISTS external_id (
    scheme       text NOT NULL,
    value        text NOT NULL,
    resource_id  uuid NOT NULL,
    PRIMARY KEY (value, scheme)
);

-- 統合後も旧 ID を有効にするためのリダイレクト（必須）。
CREATE TABLE IF NOT EXISTS identity_redirect (
    from_id      uuid PRIMARY KEY,
    to_id        uuid NOT NULL,
    revision     uuid NOT NULL,
    approved_by  text NOT NULL,
    at           bigint NOT NULL,
    reason       text
);

CREATE TABLE IF NOT EXISTS merge_proposal (
    id           uuid PRIMARY KEY,
    from_id      uuid NOT NULL,
    into_id      uuid NOT NULL,
    proposed_by  text NOT NULL,
    proposed_at  bigint NOT NULL,
    reason       text,
    evidence     uuid[] NOT NULL DEFAULT '{}',
    status       text NOT NULL,              -- proposed / approved / rejected（自動マージはしない）
    decided_by   text,
    decided_at   bigint
);

-- ------------------------------------------------------------ Acquisition / Provenance
CREATE TABLE IF NOT EXISTS source (
    id                 uuid PRIMARY KEY,
    kind               text NOT NULL,
    locator            jsonb NOT NULL,
    locator_key        text NOT NULL,
    title              text,
    resource_id        uuid,
    publisher_id       uuid,
    source_time        jsonb,                -- TemporalExpression（source_time）
    origin             text NOT NULL DEFAULT 'unknown',
    reliability        real,
    provenance_root    uuid,                 -- 転載元の一次情報（独立性判定）
    license            text,
    visibility_level   visibility_level NOT NULL DEFAULT 'public',
    visibility_groups  text[] NOT NULL DEFAULT '{}',
    registered_at      bigint NOT NULL
);
CREATE INDEX IF NOT EXISTS source_locator_idx ON source (locator_key);

-- 取得行為。同じ URL を何度取得しても別行。snapshot 本体は Object Storage（content-addressed）。
CREATE TABLE IF NOT EXISTS acquisition (
    source_id       uuid   NOT NULL,
    id              uuid   NOT NULL,
    acquired_at     bigint NOT NULL,          -- 外部エージェントが知った時刻（DB 登録時刻ではない）
    acquired_by     text   NOT NULL,
    acquired_by_kind text  NOT NULL,
    method          text   NOT NULL,
    locator         jsonb  NOT NULL,
    content_hash    text,
    snapshot_hash   text,
    snapshot_size   bigint,
    media_type      text,
    encrypted_with  uuid,
    recorded_at     bigint NOT NULL,
    PRIMARY KEY (source_id, id)
);
CREATE INDEX IF NOT EXISTS acquisition_id_idx ON acquisition (id);
CREATE INDEX IF NOT EXISTS acquisition_time_idx ON acquisition (acquired_at);

CREATE TABLE IF NOT EXISTS derivation (
    source_id        uuid NOT NULL,
    id               uuid NOT NULL,
    acquisition_id   uuid NOT NULL,
    extractor        text NOT NULL,
    model            text,
    model_version    text,
    schema_version   text,
    source_span      jsonb,
    extracted_at     bigint NOT NULL,
    extraction_conf  real,
    PRIMARY KEY (source_id, id)
);
CREATE INDEX IF NOT EXISTS derivation_model_idx ON derivation (extractor, model, model_version);

-- ------------------------------------------------------------ Assertion
CREATE TABLE IF NOT EXISTS assertion (
    subject_id          uuid   NOT NULL,
    id                  uuid   NOT NULL,
    predicate_id        uuid   NOT NULL,
    object_resource_id  uuid,                 -- Value::Resource の場合
    object_value        jsonb  NOT NULL,      -- 値（時間は TemporalExpression の原表現を不変保存）
    polarity            smallint NOT NULL DEFAULT 1,   -- 1 = 肯定, -1 = 否定（confidence とは別軸）
    valid_time          jsonb,
    spatial_scope       uuid,
    branch_id           uuid   NOT NULL,
    timeline_id         uuid,
    canon_id            uuid,
    status              assertion_status NOT NULL,
    source_reliability  real,
    extraction_conf     real,
    specificity         real,
    human_verified      boolean NOT NULL DEFAULT false,
    supersedes          uuid,
    superseded_by       uuid,
    overrides           uuid,                 -- copy-on-write 元
    asserted_by         text   NOT NULL,
    asserted_by_kind    text   NOT NULL,
    created_revision    uuid   NOT NULL,
    created_seq         bigint NOT NULL,
    created_at          bigint NOT NULL,
    first_known_at      bigint NOT NULL,      -- 根拠の最小 acquired_at（過去時点検索）
    visibility_level    visibility_level NOT NULL DEFAULT 'public',
    visibility_groups   text[] NOT NULL DEFAULT '{}',
    visibility_owner    text,
    license             text,
    note                text,
    PRIMARY KEY (subject_id, id)
);
CREATE INDEX IF NOT EXISTS assertion_pred_idx ON assertion (subject_id, predicate_id);
CREATE INDEX IF NOT EXISTS assertion_object_idx ON assertion (object_resource_id, predicate_id) WHERE object_resource_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS assertion_known_idx ON assertion (first_known_at);

-- 削除ではなく状態変化として残すため DELETE は禁止。
DROP TRIGGER IF EXISTS assertion_no_delete ON assertion;
CREATE TRIGGER assertion_no_delete BEFORE DELETE ON assertion
    FOR EACH ROW EXECUTE FUNCTION forbid_mutation();

CREATE TABLE IF NOT EXISTS assertion_status_change (
    subject_id         uuid   NOT NULL,
    assertion_id       uuid   NOT NULL,
    revision           uuid   NOT NULL,
    from_status        assertion_status NOT NULL,
    to_status          assertion_status NOT NULL,
    changed_by         text   NOT NULL,
    recorded_at        bigint NOT NULL,
    basis_acquired_at  bigint,               -- 状態変化の根拠を知った時刻
    reason             text,
    PRIMARY KEY (subject_id, assertion_id, revision)
);

CREATE TABLE IF NOT EXISTS evidence (
    subject_id      uuid NOT NULL,
    assertion_id    uuid NOT NULL,
    acquisition_id  uuid NOT NULL,
    derivation_id   uuid,
    PRIMARY KEY (subject_id, assertion_id, acquisition_id)
);

-- ------------------------------------------------------------ Work / Table / Observation / Trajectory
CREATE TABLE IF NOT EXISTS work_sequence (
    id         uuid PRIMARY KEY,
    scope_id   uuid NOT NULL,
    kind       text NOT NULL,               -- release_order / work_order / story_order / recommended_order / custom
    items      uuid[] NOT NULL,
    branch_id  uuid NOT NULL,
    canon_id   uuid,
    label      text
);

CREATE TABLE IF NOT EXISTS dataset_table (
    id            uuid PRIMARY KEY,
    dataset_id    uuid NOT NULL,
    name          text NOT NULL,
    columns       jsonb NOT NULL,
    primary_key   text[] NOT NULL DEFAULT '{}',
    foreign_keys  jsonb NOT NULL DEFAULT '[]',
    storage       jsonb NOT NULL             -- inline / parquet / csv / sql（本体は DuckDB / Parquet）
);

CREATE TABLE IF NOT EXISTS row_link (
    resource_id  uuid NOT NULL,
    table_id     uuid NOT NULL,
    row_key      text NOT NULL,
    PRIMARY KEY (resource_id, table_id, row_key)
);

-- 大量履歴は DuckDB / Parquet へ分離できる（ここは直近・小規模分）。
CREATE TABLE IF NOT EXISTS observation (
    target_id       uuid   NOT NULL,
    metric          text   NOT NULL,
    observed_at     bigint NOT NULL,
    id              uuid   NOT NULL,
    value           float8 NOT NULL,
    unit            text,                    -- UCUM
    acquisition_id  uuid,
    branch_id       uuid   NOT NULL,
    PRIMARY KEY (target_id, metric, observed_at, id)
);

CREATE TABLE IF NOT EXISTS trajectory (
    target_id      uuid NOT NULL,
    id             uuid NOT NULL,
    frame_id       uuid NOT NULL,
    interpolation  text NOT NULL,
    span           int8range,
    samples        jsonb,                    -- 小規模は inline、大規模は storage_uri（Parquet）
    storage_uri    text,
    acquisition_id uuid,
    PRIMARY KEY (target_id, id)
);

-- ------------------------------------------------------------ Embedding / Keys
CREATE TABLE IF NOT EXISTS embedding_space (
    key                 text PRIMARY KEY,    -- <embedding_model_id>@<embedding_version>
    embedding_model_id  text NOT NULL,
    embedding_version   text NOT NULL,
    dimension           int  NOT NULL
);

-- ベクトル本体は ANN 用の外部索引（例: pgvector / 専用サービス）が持つ前提で、
-- ここでは再構築用に float32 を bytea で保持する（量子化方式は固定しない）。
CREATE TABLE IF NOT EXISTS embedding (
    resource_id   uuid   NOT NULL,
    space_key     text   NOT NULL,
    generated_at  bigint NOT NULL,
    vector_f32    bytea  NOT NULL,
    binary_code   bytea,
    PRIMARY KEY (resource_id, space_key)
);

-- crypto-shredding 用の鍵メタデータ。鍵素材は KMS に置くのが望ましい。
-- 同一 DB に置く場合も key_material を NULL にすることで削除権に対応する（Revision は書き換えない）。
CREATE TABLE IF NOT EXISTS crypto_key (
    id            uuid PRIMARY KEY,
    created_at    bigint NOT NULL,
    key_material  bytea,
    shredded_at   bigint
);
