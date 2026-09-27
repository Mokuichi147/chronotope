-- phase: expand
-- 会話履歴と、所有者に基づく出典の閲覧制御。
--
--  * source に所有者（visibility_owner）を持たせ、非公開の出典を所有者だけに見せる。
--  * acquisition / derivation は親の source が見える場合だけ見せる（ID 直指定でも他人の取得記録を読ませない）。
--  * 委任: API 層は所有者の代わりに動くエージェントの問い合わせで
--      SET LOCAL chronotope.on_behalf_of = '<owner>';
--    を設定する。主体（chronotope.principal）はエージェント自身のまま。
--  * conversation_event は原文イベントのメタデータ。本文は acquisition のスナップショット
--    （Object Storage）にあり、DB には格納しない。

SET search_path = chronotope, public;

ALTER TABLE source ADD COLUMN IF NOT EXISTS visibility_owner text;

CREATE OR REPLACE FUNCTION can_see(level visibility_level, groups text[], owner text) RETURNS boolean
LANGUAGE sql STABLE AS $$
    SELECT CASE level
        WHEN 'public' THEN true
        WHEN 'groups' THEN groups && string_to_array(coalesce(current_setting('chronotope.groups', true), ''), ',')
        WHEN 'private' THEN owner IS NOT NULL AND (
            owner = current_setting('chronotope.principal', true)
            OR owner = nullif(current_setting('chronotope.on_behalf_of', true), ''))
    END
$$;

DROP POLICY IF EXISTS source_visibility ON source;
CREATE POLICY source_visibility ON source FOR SELECT
    USING (can_see(visibility_level, visibility_groups, visibility_owner));

ALTER TABLE acquisition ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS acquisition_visibility ON acquisition;
CREATE POLICY acquisition_visibility ON acquisition FOR SELECT
    USING (EXISTS (SELECT 1 FROM source s WHERE s.id = acquisition.source_id));

ALTER TABLE derivation ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS derivation_visibility ON derivation;
CREATE POLICY derivation_visibility ON derivation FOR SELECT
    USING (EXISTS (SELECT 1 FROM source s WHERE s.id = derivation.source_id));

DO $$ BEGIN
    CREATE TYPE event_kind AS ENUM ('message', 'tool_call', 'tool_result', 'summary', 'attachment', 'other');
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
    -- API 上の role とは独立した実際の生成元（human = ユーザー本人の入力）。
    CREATE TYPE event_origin AS ENUM ('human', 'model', 'tool', 'runtime', 'system', 'agent', 'unknown');
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
    CREATE TYPE event_status AS ENUM ('ok', 'error', 'interrupted', 'unknown');
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

-- (owner_id, event_id) の一意性は書き込み API が保証する（再送は同じ行を返し、内容が違えば conflict）。
-- Citus では source_id で分散するため、一意制約は分散キーを含む主キーだけに置く。
CREATE TABLE IF NOT EXISTS conversation_event (
    source_id          uuid   NOT NULL,          -- 会話を表す Source
    owner_id           text   NOT NULL,
    event_id           text   NOT NULL,          -- クライアントが保存前に採番する ID
    conversation_id    text   NOT NULL,
    turn_id            text,
    sequence           bigint NOT NULL,          -- 会話内の記録順（時刻ではなくこれで並べる）
    kind               event_kind   NOT NULL,
    origin             event_origin NOT NULL,
    api_role           text,
    speaker            text,
    received_at        bigint,
    recorded_at        bigint NOT NULL,
    recorded_by        text   NOT NULL,
    response_id        text,
    call_id            text,
    parent_event_id    text,
    status             event_status,
    supersedes         text,
    derived_from       text[] NOT NULL DEFAULT '{}',
    metadata           jsonb,
    acquisition_id     uuid   NOT NULL,          -- 原文（スナップショット）
    size               bigint NOT NULL,
    visibility_level   visibility_level NOT NULL,
    visibility_groups  text[] NOT NULL DEFAULT '{}',
    visibility_owner   text,
    PRIMARY KEY (source_id, event_id)
);
CREATE INDEX IF NOT EXISTS conversation_event_seq_idx ON conversation_event (owner_id, conversation_id, sequence);
CREATE INDEX IF NOT EXISTS conversation_event_id_idx ON conversation_event (owner_id, event_id);
CREATE INDEX IF NOT EXISTS conversation_event_call_idx ON conversation_event (owner_id, conversation_id, call_id) WHERE call_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS conversation_event_time_idx ON conversation_event (owner_id, (coalesce(received_at, recorded_at)));

ALTER TABLE conversation_event ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS conversation_event_visibility ON conversation_event;
CREATE POLICY conversation_event_visibility ON conversation_event FOR SELECT
    USING (can_see(visibility_level, visibility_groups, visibility_owner));

GRANT SELECT ON conversation_event TO chronotope_agent;
GRANT SELECT, INSERT, UPDATE ON conversation_event TO chronotope_api;

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'citus') THEN
        PERFORM create_distributed_table('chronotope.conversation_event', 'source_id', colocate_with => 'chronotope.source');
    END IF;
END $$;
