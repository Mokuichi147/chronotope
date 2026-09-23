-- phase: expand
-- Assertion 単位の Visibility（RLS）。API 層はトランザクションごとに
--   SET LOCAL chronotope.principal = '<id>'; SET LOCAL chronotope.groups = 'g1,g2';
-- を設定してから問い合わせる。AI エージェント向けのロールは SELECT のみ（書き込みは意味的 API 経由）。

SET search_path = chronotope, public;

CREATE OR REPLACE FUNCTION can_see(level visibility_level, groups text[], owner text) RETURNS boolean
LANGUAGE sql STABLE AS $$
    SELECT CASE level
        WHEN 'public' THEN true
        WHEN 'groups' THEN groups && string_to_array(coalesce(current_setting('chronotope.groups', true), ''), ',')
        WHEN 'private' THEN owner = current_setting('chronotope.principal', true)
    END
$$;

ALTER TABLE assertion ENABLE ROW LEVEL SECURITY;
ALTER TABLE source ENABLE ROW LEVEL SECURITY;
ALTER TABLE resource ENABLE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS assertion_visibility ON assertion;
CREATE POLICY assertion_visibility ON assertion FOR SELECT
    USING (can_see(visibility_level, visibility_groups, visibility_owner));
-- Search Projection には行単位 RLS を掛けない。
--  * Projection は公開 Assertion だけから Materialize される（非公開 Assertion は Level 2 以降で RLS 越しに見る）。
--  * RLS の security barrier は非 leakproof な範囲・配列演算子（&&, @>）より先に評価されるため、
--    GiST / GIN 索引が使えず Tier 1 のレイテンシ目標を満たせない。
-- 代わりに Resource 単位の可視性を条件に持つビュー経由で公開し、元テーブルは API ロールだけが読む。
ALTER TABLE search_projection DISABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS projection_visibility ON search_projection;
CREATE OR REPLACE VIEW search_projection_visible AS
    SELECT * FROM search_projection
    WHERE visibility_level = 'public'
       OR (visibility_level = 'groups'
           AND visibility_groups && string_to_array(coalesce(current_setting('chronotope.groups', true), ''), ','));
DROP POLICY IF EXISTS source_visibility ON source;
CREATE POLICY source_visibility ON source FOR SELECT
    USING (can_see(visibility_level, visibility_groups, NULL));
DROP POLICY IF EXISTS resource_visibility ON resource;
CREATE POLICY resource_visibility ON resource FOR SELECT
    USING (can_see(visibility_level, visibility_groups, visibility_owner));

-- Tier 0: ID 参照（統合済み ID はリダイレクトをたどる）。
DROP FUNCTION IF EXISTS lookup(uuid, uuid);
CREATE FUNCTION lookup(p_id uuid, p_branch uuid)
RETURNS SETOF search_projection_visible LANGUAGE sql STABLE AS $$
    SELECT * FROM chronotope.search_projection_visible
    WHERE canonical_id = chronotope.resolve_id(p_id) AND branch_id = p_branch
$$;

DO $$ BEGIN
    CREATE ROLE chronotope_agent NOLOGIN;
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
    CREATE ROLE chronotope_api NOLOGIN;
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

GRANT USAGE ON SCHEMA chronotope TO chronotope_agent, chronotope_api;
GRANT SELECT ON ALL TABLES IN SCHEMA chronotope TO chronotope_agent;
-- 鍵と Projection 元テーブルは読み取りロールから隠す（Projection はビュー経由）。
REVOKE SELECT ON crypto_key, search_projection FROM chronotope_agent;
GRANT SELECT ON search_projection_visible TO chronotope_agent;
GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA chronotope TO chronotope_api;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA chronotope TO chronotope_agent, chronotope_api;
