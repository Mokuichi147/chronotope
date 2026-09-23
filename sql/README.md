# PostgreSQL / Citus スキーマ

`migrations/` のファイルは番号順に適用する。各ファイル先頭の `-- phase:` は
Expand → Migrate → Contract のどの段階かを示す。

| ファイル | 段階 | 内容 |
|---|---|---|
| 0001_expand_canonical.sql | expand | Canonical 層（Resource / Assertion / Provenance / Identity / Revision ...） |
| 0002_expand_projection.sql | expand | Search Projection（GiST / GIN 索引）・無効化キュー・Tier 0 関数 |
| 0003_expand_rls.sql | expand | Assertion 単位の RLS・ロール |
| 0004_expand_citus.sql | expand | Citus の分散配置（Citus が無ければ何もしない） |

```bash
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f sql/migrations/0001_expand_canonical.sql   # 以降同様
```

## 大規模 DB での段階的なスキーマ変更

1. **Expand**: 旧スキーマと両立する追加だけを行う（列・テーブル・索引の追加。`NOT NULL` は付けない、
   索引は `CREATE INDEX CONCURRENTLY`）。アプリは旧・新の両方に書く。
2. **Migrate**: 既存データをバッチで移す（Citus では分散実行、無効化キュー経由で Projection も再計算）。
   Projection は `projection_version` を上げて再 Materialize し、読み取りを新バージョンへ切り替える。
3. **Contract**: 旧列・旧テーブルへの参照が無いことを確認してから削除・制約追加を行う。

ファイル名は `NNNN_<phase>_<name>.sql` とし、Contract は Expand / Migrate の適用・デプロイ完了後に
別リリースで流す。

## 時刻・範囲の表現

- 時刻は int8 の UTA tick（ミリ秒、Unix epoch 起点）。±∞ は NULL。
- `search_projection.temporal_possible` は可能区間 `[earliest_start, latest_end)`（異説の包絡を含む）、
  `temporal_certain` は確実区間 `[latest_start, earliest_end)`。
- 異なる `time_axis` 同士は比較しない（API は `comparable: false` を返す）。
