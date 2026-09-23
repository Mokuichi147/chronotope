-- phase: expand
-- Search Projection（検索速度優先・非正規化）。検索時に JOIN を必要としないフラットな 1 Resource = 1 行。
-- Projection は Canonical から Materializer が再計算する派生物で、鮮度情報を持つ。

SET search_path = chronotope, public;

CREATE TABLE IF NOT EXISTS search_projection (
    canonical_id        uuid    NOT NULL,
    branch_id           uuid    NOT NULL,
    types_mask          bigint  NOT NULL DEFAULT 0,
    type_ids            uuid[]  NOT NULL DEFAULT '{}',
    label               text    NOT NULL DEFAULT '',
    labels              text[]  NOT NULL DEFAULT '{}',
    summary             text    NOT NULL DEFAULT '',
    -- 4 点境界（NULL = ±∞）
    earliest_start      bigint,
    latest_start        bigint,
    earliest_end        bigint,
    latest_end          bigint,
    -- 可能区間 [es, le)（異説の包絡を含む）と確実区間 [ls, ee)。GiST で範囲検索する。
    temporal_possible   int8range,
    temporal_certain    int8range,
    time_axis           text,
    granularity         text,
    recurrence          jsonb,
    temporal_contested  boolean NOT NULL DEFAULT false,
    order_start         bigint,               -- Temporal Order Label（アクセラレータ）
    order_end           bigint,
    geo_frame           uuid,
    geo_bbox            box,                  -- 基準 Frame 上の bbox
    space_ids           uuid[]  NOT NULL DEFAULT '{}',
    space_ancestor_ids  uuid[]  NOT NULL DEFAULT '{}',
    entity_ids          uuid[]  NOT NULL DEFAULT '{}',
    work_ids            uuid[]  NOT NULL DEFAULT '{}',
    branch_ids          uuid[]  NOT NULL DEFAULT '{}',
    canon_ids           uuid[]  NOT NULL DEFAULT '{}',
    timeline_ids        uuid[]  NOT NULL DEFAULT '{}',
    rank                float8  NOT NULL DEFAULT 0,
    contested           boolean NOT NULL DEFAULT false,
    known_from          bigint  NOT NULL,
    redistributable     boolean NOT NULL DEFAULT true,
    visibility_level    visibility_level NOT NULL DEFAULT 'public',
    visibility_groups   text[]  NOT NULL DEFAULT '{}',
    embedding_ref       text,
    projection_version  int     NOT NULL,
    source_revision     bigint  NOT NULL,
    materialized_at     bigint  NOT NULL,
    stale               boolean NOT NULL DEFAULT false,
    PRIMARY KEY (canonical_id, branch_id)
);

CREATE INDEX IF NOT EXISTS sp_time_gist ON search_projection USING gist (branch_id, temporal_possible);
CREATE INDEX IF NOT EXISTS sp_geo_gist ON search_projection USING gist (geo_bbox) WHERE geo_bbox IS NOT NULL;
CREATE INDEX IF NOT EXISTS sp_space_anc_gin ON search_projection USING gin (space_ancestor_ids);
CREATE INDEX IF NOT EXISTS sp_entity_gin ON search_projection USING gin (entity_ids);
CREATE INDEX IF NOT EXISTS sp_work_gin ON search_projection USING gin (work_ids);
CREATE INDEX IF NOT EXISTS sp_type_gin ON search_projection USING gin (type_ids);
CREATE INDEX IF NOT EXISTS sp_canon_gin ON search_projection USING gin (canon_ids);
CREATE INDEX IF NOT EXISTS sp_timeline_gin ON search_projection USING gin (timeline_ids);
CREATE INDEX IF NOT EXISTS sp_rank_idx ON search_projection (branch_id, rank DESC);
CREATE INDEX IF NOT EXISTS sp_stale_idx ON search_projection (branch_id) WHERE stale;

-- 結果整合の無効化キュー（変更 → キュー → Resolver 再計算）。
CREATE TABLE IF NOT EXISTS invalidation_queue (
    resource_id  uuid   NOT NULL,
    branch_id    uuid   NOT NULL,
    enqueued_at  bigint NOT NULL,
    reason       text,
    PRIMARY KEY (resource_id, branch_id)
);

-- Tier 0: ID 参照（identity_redirect を最大 16 段たどる）。
CREATE OR REPLACE FUNCTION resolve_id(p_id uuid) RETURNS uuid LANGUAGE plpgsql STABLE AS $$
DECLARE
    cur uuid := p_id;
    nxt uuid;
BEGIN
    FOR i IN 1..16 LOOP
        SELECT to_id INTO nxt FROM chronotope.identity_redirect WHERE from_id = cur;
        EXIT WHEN nxt IS NULL;
        cur := nxt;
    END LOOP;
    RETURN cur;
END $$;

-- Tier 0 の lookup() は可視性ビューに依存するため 0003 で定義する。
