-- phase: expand
-- Citus 分散配置。distribution key は全テーブル canonical_id 固定にはせず、
-- 主要アクセスパターンと co-location でテーブルごとに決める。
--
--   resource / resource_type / assertion / evidence / assertion_status_change /
--   search_projection / observation / trajectory / row_link / embedding
--       → Resource ID（assertion は subject_id）で co-locate。
--         「ある Resource の主張・根拠・Projection 行」を 1 ノードで JOIN できる。
--   source / acquisition / derivation
--       → source_id で co-locate（同じ情報源の再取得・抽出を 1 ノードに集める）。
--   label_index → 正規化ラベル（名前参照はラベルで引くため）。
--   external_id → value。
--   revision → id（追記専用）。
--   語彙・暦・座標系・ライセンス・ブランチ・Embedding 空間 → reference table（全ノード複製）。
--
-- Citus が無い単一 PostgreSQL では何もしない。

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'citus') THEN
        RAISE NOTICE 'citus extension not installed; skipping distribution';
        RETURN;
    END IF;

    PERFORM create_reference_table('chronotope.type_def');
    PERFORM create_reference_table('chronotope.predicate');
    PERFORM create_reference_table('chronotope.vocab_mapping');
    PERFORM create_reference_table('chronotope.calendar_frame');
    PERFORM create_reference_table('chronotope.spatial_frame');
    PERFORM create_reference_table('chronotope.license');
    PERFORM create_reference_table('chronotope.branch');
    PERFORM create_reference_table('chronotope.embedding_space');
    PERFORM create_reference_table('chronotope.identity_redirect');

    PERFORM create_distributed_table('chronotope.resource', 'id');
    PERFORM create_distributed_table('chronotope.resource_type', 'resource_id', colocate_with => 'chronotope.resource');
    PERFORM create_distributed_table('chronotope.assertion', 'subject_id', colocate_with => 'chronotope.resource');
    PERFORM create_distributed_table('chronotope.evidence', 'subject_id', colocate_with => 'chronotope.resource');
    PERFORM create_distributed_table('chronotope.assertion_status_change', 'subject_id', colocate_with => 'chronotope.resource');
    PERFORM create_distributed_table('chronotope.search_projection', 'canonical_id', colocate_with => 'chronotope.resource');
    PERFORM create_distributed_table('chronotope.invalidation_queue', 'resource_id', colocate_with => 'chronotope.resource');
    PERFORM create_distributed_table('chronotope.observation', 'target_id', colocate_with => 'chronotope.resource');
    PERFORM create_distributed_table('chronotope.trajectory', 'target_id', colocate_with => 'chronotope.resource');
    PERFORM create_distributed_table('chronotope.row_link', 'resource_id', colocate_with => 'chronotope.resource');
    PERFORM create_distributed_table('chronotope.embedding', 'resource_id', colocate_with => 'chronotope.resource');

    PERFORM create_distributed_table('chronotope.source', 'id');
    PERFORM create_distributed_table('chronotope.acquisition', 'source_id', colocate_with => 'chronotope.source');
    PERFORM create_distributed_table('chronotope.derivation', 'source_id', colocate_with => 'chronotope.source');

    PERFORM create_distributed_table('chronotope.label_index', 'norm');
    PERFORM create_distributed_table('chronotope.external_id', 'value');
    PERFORM create_distributed_table('chronotope.revision', 'id');
    PERFORM create_distributed_table('chronotope.merge_proposal', 'id');
    PERFORM create_distributed_table('chronotope.work_sequence', 'id');
    PERFORM create_distributed_table('chronotope.dataset_table', 'id');
END $$;
