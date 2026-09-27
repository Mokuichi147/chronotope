//! KnowledgeBase — Canonical 層・派生状態（ブランチごとの Projection）・外部ストアをまとめる。

use crate::canonical::{CanonicalStore, Touch};
use crate::command::{Command, Revision, RevisionHeader};
use crate::crypto::KeyVault;
use crate::embed::{Embedder, HashingEmbedder};
use crate::history::{HistoryStore, Unindexed};
use crate::materialize::{BranchState, MaterializeStats};
use crate::store::columnar::{ColumnarStore, ObservationStore};
use crate::store::log::{JsonlLog, MemoryLog, RevisionLog};
use crate::store::object::{FsObjectStore, MemoryObjectStore, ObjectStore};
use crate::vector::{EmbeddingSpace, VectorStore};
use chronotope_core::model::*;
use chronotope_core::time::Tick;
use chronotope_core::vocab::{builtin_predicates, builtin_types, type_id};
use chronotope_core::*;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct KbConfig {
    /// 要約などの既定言語。
    pub default_lang: Option<String>,
    /// 自動 Embedding の対象型（仕様: 主に Document / Post / Work / Event）。
    pub embed_types: Vec<String>,
    pub auto_embed: bool,
    /// 1 回の Materializer 実行で処理する最大件数（ブランチごと）。
    pub materialize_batch: usize,
    /// Revision ログとスナップショットを書き込みごとに fsync する（電源断でもコミット済みの変更を失わない）。
    pub fsync: bool,
}

impl Default for KbConfig {
    fn default() -> Self {
        KbConfig {
            default_lang: Some("ja".into()),
            embed_types: ["Document", "Post", "Work", "Event"].iter().map(|s| s.to_string()).collect(),
            auto_embed: true,
            materialize_batch: 10_000,
            fsync: true,
        }
    }
}

pub struct KnowledgeBase {
    pub(crate) store: CanonicalStore,
    pub(crate) states: HashMap<BranchId, BranchState>,
    pub(crate) vectors: VectorStore,
    pub(crate) vault: KeyVault,
    pub(crate) objects: Arc<dyn ObjectStore>,
    log: Box<dyn RevisionLog>,
    pub(crate) columnar: ColumnarStore,
    pub(crate) history: HistoryStore,
    pub(crate) embedder: Box<dyn Embedder>,
    pub(crate) config: KbConfig,
    clock: Box<dyn Fn() -> Tick + Send + Sync>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Freshness {
    pub branch: BranchId,
    /// Canonical の最新 Revision 通番。
    pub source_revision: u64,
    /// キューが空になった時点の Revision 通番。
    pub materialized_revision: u64,
    pub queue: usize,
    pub stale_rows: u64,
    pub rows: usize,
    pub last_materialized_at: Tick,
    pub consistent: bool,
}

pub const DEFAULT_EMBEDDING_SPACE: &str = "hashing@1";

impl KnowledgeBase {
    fn with_parts(log: Box<dyn RevisionLog>, objects: Arc<dyn ObjectStore>, vault: KeyVault, config: KbConfig) -> Self {
        let mut states = HashMap::new();
        states.insert(BranchId::main(), BranchState::new(BranchId::main()));
        KnowledgeBase {
            store: CanonicalStore::new(),
            states,
            vectors: VectorStore::default(),
            vault,
            objects,
            log,
            columnar: ColumnarStore::default(),
            history: HistoryStore::default(),
            embedder: Box::new(HashingEmbedder::default()),
            config,
            clock: Box::new(Tick::now),
        }
    }

    /// 永続化しないインメモリ KB（テスト・ベンチマーク用）。
    pub fn in_memory(config: KbConfig) -> Self {
        let mut kb = Self::with_parts(Box::new(MemoryLog::default()), Arc::new(MemoryObjectStore::default()), KeyVault::in_memory(), config);
        kb.bootstrap().expect("bootstrap in memory");
        kb
    }

    /// ディレクトリ上の KB を開く（Revision ログを再生して状態を復元する）。
    pub fn open(dir: &Path, config: KbConfig) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| Error::Storage(e.to_string()))?;
        let log = JsonlLog::open(&dir.join("revisions.jsonl"), config.fsync)?;
        let revisions = log.read_all()?;
        let objects = Arc::new(FsObjectStore::with_fsync(dir.join("objects"), config.fsync)?);
        let vault = KeyVault::open(dir.join("keys.json"))?;
        let mut kb = Self::with_parts(Box::new(log), objects, vault, config);
        if revisions.is_empty() {
            kb.bootstrap()?;
        } else {
            for rev in &revisions {
                kb.apply_revision(rev);
            }
            tracing::info!(revisions = revisions.len(), "replayed revision log");
        }
        Ok(kb)
    }

    pub fn set_clock(&mut self, f: impl Fn() -> Tick + Send + Sync + 'static) {
        self.clock = Box::new(f);
    }

    pub fn set_embedder(&mut self, e: Box<dyn Embedder>) {
        self.embedder = e;
    }

    pub fn now(&self) -> Tick {
        (self.clock)()
    }

    pub fn store(&self) -> &CanonicalStore {
        &self.store
    }

    pub fn config(&self) -> &KbConfig {
        &self.config
    }

    pub fn objects(&self) -> &Arc<dyn ObjectStore> {
        &self.objects
    }

    pub fn observations(&self) -> &dyn ObservationStore {
        self.columnar.observations.as_ref()
    }

    pub fn history(&self) -> &HistoryStore {
        &self.history
    }

    /// スナップショットを（指定があれば暗号化して）保存する。
    /// 返す ContentHash は平文のハッシュで、暗号化した場合は照合用に公開しない。
    pub(crate) fn put_snapshot(&self, bytes: &[u8], encrypt_with: Option<KeyId>, media_type: Option<String>) -> Result<(ObjectRef, ContentHash, bool)> {
        let plain_hash = crate::store::object::content_hash(bytes);
        let (stored, encrypted_with) = match encrypt_with {
            Some(k) => {
                let (nonce, ct) = self.vault.encrypt_bytes(k, bytes)?;
                let mut v = hex::decode(&nonce).map_err(|e| Error::Storage(e.to_string()))?;
                v.extend(ct);
                (v, Some(k))
            }
            None => (bytes.to_vec(), None),
        };
        let (h, fresh) = self.objects.put(&stored)?;
        let r = ObjectRef { hash: h, size: stored.len() as u64, media_type, encrypted_with };
        let shown = if encrypted_with.is_some() { ContentHash("withheld:encrypted".into()) } else { plain_hash };
        Ok((r, shown, !fresh))
    }

    /// スナップショットの平文。
    pub(crate) fn read_snapshot(&self, r: &ObjectRef) -> Result<std::result::Result<Vec<u8>, Unindexed>> {
        let Some(bytes) = self.objects.get(&r.hash)? else { return Ok(Err(Unindexed::Missing)) };
        Ok(match r.encrypted_with {
            Some(k) if bytes.len() > 12 => self.vault.decrypt_bytes(&k, &hex::encode(&bytes[..12]), &bytes[12..]).ok_or(Unindexed::Shredded),
            Some(_) => Err(Unindexed::Shredded),
            None => Ok(bytes),
        })
    }

    fn bootstrap(&mut self) -> Result<()> {
        let mut cmds: Vec<Command> = vec![];
        cmds.extend(builtin_types().into_iter().map(|def| Command::DefineType { def }));
        cmds.extend(builtin_predicates().into_iter().map(|def| Command::DefinePredicate { def }));
        let e = &self.embedder;
        cmds.push(Command::DefineEmbeddingSpace { space: EmbeddingSpace::new(e.model_id(), e.version(), e.dimension()) });
        for (key, name, redistributable) in [
            ("CC0-1.0", "Creative Commons Zero", true),
            ("CC-BY-4.0", "Creative Commons Attribution 4.0", true),
            ("CC-BY-SA-4.0", "Creative Commons Attribution-ShareAlike 4.0", true),
            ("proprietary", "Proprietary / all rights reserved", false),
            ("fair-use-excerpt", "Excerpt under fair use / quotation", false),
            ("unknown", "Unknown license (treated as non-redistributable)", false),
        ] {
            cmds.push(Command::DefineLicense {
                license: License { key: key.into(), name: name.into(), url: None, redistributable, attribution_required: key.starts_with("CC-BY") },
            });
        }
        self.commit(&ActorRef::system(), BranchId::main(), Some("bootstrap vocabulary".into()), cmds)?;
        Ok(())
    }

    /// Revision を作ってログへ書き（WAL）、Canonical へ適用し、影響範囲を無効化する。
    pub fn commit(&mut self, actor: &ActorRef, branch: BranchId, message: Option<String>, commands: Vec<Command>) -> Result<RevisionHeader> {
        if commands.is_empty() {
            return Err(Error::invalid("empty revision"));
        }
        let rev = Revision { id: RevisionId::new(), seq: 0, branch, actor: actor.clone(), message, committed_at: self.now(), commands };
        self.commit_prepared(rev)
    }

    /// ID を事前に決めた Revision をコミットする（Command 内の created_revision と一致させるため）。
    pub fn commit_prepared(&mut self, mut rev: Revision) -> Result<RevisionHeader> {
        if rev.commands.is_empty() {
            return Err(Error::invalid("empty revision"));
        }
        rev.seq = self.store.head_seq + 1;
        self.log.append(&rev)?;
        self.apply_revision(&rev);
        Ok(rev.header())
    }

    fn apply_revision(&mut self, rev: &Revision) {
        self.store.register_revision(rev);
        for cmd in &rev.commands {
            self.apply_side_stores(cmd);
            let touch = self.store.apply(rev, cmd);
            self.invalidate(touch, cmd);
        }
    }

    fn apply_side_stores(&mut self, cmd: &Command) {
        match cmd {
            Command::CreateKey { key_id } => {
                if let Err(e) = self.vault.ensure_key(*key_id) {
                    tracing::error!(%key_id, error = %e, "failed to create key");
                }
            }
            Command::ShredKey { key_id } => {
                if let Err(e) = self.vault.shred(*key_id) {
                    tracing::error!(%key_id, error = %e, "failed to shred key");
                }
                self.history.shred(*key_id);
            }
            Command::RecordEvent { event } => {
                let snapshot = self.store.acquisitions.get(&event.acquisition).and_then(|a| a.snapshot_ref.clone());
                let content = match &snapshot {
                    Some(r) => match self.read_snapshot(r) {
                        Ok(Ok(b)) => String::from_utf8(b).map_err(|_| Unindexed::NotUtf8),
                        Ok(Err(why)) => Err(why),
                        Err(e) => {
                            tracing::error!(event = %event.event_id, error = %e, "failed to read event snapshot");
                            Err(Unindexed::Missing)
                        }
                    },
                    None => Err(Unindexed::Missing),
                };
                let key = snapshot.and_then(|r| r.encrypted_with);
                self.history.insert(event.clone(), content.as_deref().map_err(|e| *e), key);
            }
            Command::AddObservation { observation } => self.columnar.observations.insert(observation.clone()),
            Command::AddTrajectory { trajectory } => {
                let mut t = trajectory.clone();
                t.samples.sort_by_key(|s| s.t);
                self.columnar.trajectories.entry(t.target).or_default().push(t);
            }
            Command::AddTableRows { table, rows } => self.columnar.table_rows.entry(*table).or_default().extend(rows.iter().cloned()),
            Command::DefineEmbeddingSpace { space } => self.vectors.define(space.clone()),
            Command::SetEmbedding { resource, space, vector, generated_at } => {
                if let Some(idx) = self.vectors.get_mut(space) {
                    if let Err(e) = idx.upsert(*resource, vector, *generated_at) {
                        tracing::warn!(error = %e, "embedding rejected");
                    }
                }
            }
            _ => {}
        }
    }

    fn invalidate(&mut self, touch: Touch, cmd: &Command) {
        if touch.global {
            let store = &self.store;
            for st in self.states.values_mut() {
                st.invalidate_all(store);
            }
            return;
        }
        let targets: Vec<BranchId> =
            if touch.all_branches { self.states.keys().copied().collect() } else { touch.branch.filter(|b| self.states.contains_key(b)).into_iter().collect() };
        for b in targets {
            let st = self.states.get_mut(&b).expect("target branch state exists");
            for r in &touch.resources {
                st.invalidate(*r);
                let canonical = self.store.resolve_id(*r);
                if canonical != *r {
                    st.invalidate(canonical);
                }
                let expand: Vec<ResourceId> = match touch.role {
                    Some(PredicateRole::SpatialContainment | PredicateRole::Coordinates | PredicateRole::Location) => {
                        let p = &st.projection;
                        let mut bm = p.bitmap_of(&p.idx_space_ancestor, r);
                        bm |= p.bitmap_of(&p.idx_space, r);
                        p.ids_of(&bm)
                    }
                    Some(PredicateRole::WorkMembership) => {
                        let p = &st.projection;
                        p.ids_of(&p.bitmap_of(&p.idx_work, r))
                    }
                    _ => vec![],
                };
                for id in expand {
                    st.invalidate(id);
                }
            }
            if touch.role == Some(PredicateRole::TemporalRelation) {
                match cmd {
                    Command::AddAssertion { assertion } => st.pending_relations.push(assertion.id),
                    _ => st.order_dirty = true,
                }
            }
            for l in &touch.labels {
                st.invalidate_name(l);
            }
        }
    }

    // ------------------------------------------------------------ materialization

    /// 各ブランチの無効化キューを最大 `max_per_branch` 件ずつ処理する。
    pub fn materialize(&mut self, max_per_branch: usize) -> Result<Vec<(BranchId, MaterializeStats)>> {
        let now = self.now();
        let KnowledgeBase { store, states, vectors, embedder, config, .. } = self;
        let embed_types: Vec<ResourceId> = config.embed_types.iter().map(|k| store.type_id(k).unwrap_or_else(|| type_id(k))).collect();
        let space_key = format!("{}@{}", embedder.model_id(), embedder.version());
        let mut out = vec![];
        let mut ids: Vec<BranchId> = states.keys().copied().collect();
        ids.sort_by_key(|b| (*b != BranchId::main(), *b));
        for b in ids {
            let st = states.get_mut(&b).expect("state exists");
            let is_main = b == BranchId::main();
            let mut on_row = |row: &crate::projection::ProjectionRow| {
                if !config.auto_embed || !is_main || !row.type_ids.iter().any(|t| embed_types.contains(t)) {
                    return;
                }
                let text = embedding_text(row);
                let v = embedder.embed(&text);
                if let Some(idx) = vectors.get_mut(&space_key) {
                    if idx.generated_at(&row.canonical_id).is_none_or(|t| t < row.materialized_at) {
                        let _ = idx.upsert(row.canonical_id, &v, row.materialized_at);
                    }
                }
            };
            let stats = st.process(store, now, max_per_branch, &mut on_row)?;
            out.push((b, stats));
        }
        Ok(out)
    }

    /// キューが空になるまで処理する（テスト・CLI・バッチ用）。
    pub fn materialize_all(&mut self) -> Result<usize> {
        let mut total = 0;
        loop {
            let stats = self.materialize(self.config.materialize_batch)?;
            let processed: usize = stats.iter().map(|(_, s)| s.processed).sum();
            total += processed;
            if stats.iter().all(|(_, s)| s.remaining == 0) {
                return Ok(total);
            }
        }
    }

    /// ブランチの Projection を作成（未作成なら全件を無効化して構築）する。
    pub fn materialize_branch(&mut self, branch: BranchId) -> Result<usize> {
        if !self.store.branches.contains_key(&branch) {
            return Err(Error::not_found(format!("branch {branch}")));
        }
        if !self.states.contains_key(&branch) {
            let mut st = BranchState::new(branch);
            st.invalidate_all(&self.store);
            self.states.insert(branch, st);
        }
        self.materialize_all()
    }

    pub fn state(&self, branch: BranchId) -> Result<&BranchState> {
        self.states.get(&branch).ok_or_else(|| Error::Invalid(format!("branch {branch} is not materialized; call materialize_branch first")))
    }

    pub fn freshness(&self, branch: BranchId) -> Result<Freshness> {
        let st = self.state(branch)?;
        Ok(Freshness {
            branch,
            source_revision: self.store.head_seq,
            materialized_revision: st.materialized_seq,
            queue: st.queue_len(),
            stale_rows: st.projection.stale.len(),
            rows: st.projection.len(),
            last_materialized_at: st.last_materialized_at,
            consistent: st.queue_len() == 0 && st.materialized_seq == self.store.head_seq,
        })
    }

    /// 保護値の復号（鍵が破棄されていれば `[shredded]`）。
    pub fn reveal(&self, v: &Value) -> Value {
        match v {
            Value::Protected(p) => self.vault.reveal(p).unwrap_or(Value::Text { text: "[shredded]".into(), lang: None }),
            other => other.clone(),
        }
    }

    pub fn revisions(&self) -> &[RevisionHeader] {
        &self.store.revisions
    }
}

fn embedding_text(row: &crate::projection::ProjectionRow) -> String {
    let mut s = String::new();
    for l in &row.labels {
        s.push_str(&l.text);
        s.push(' ');
    }
    if let Some(d) = &row.description {
        s.push_str(d);
        s.push(' ');
    }
    if let Some(t) = &row.temporal_raw {
        s.push_str(t);
    }
    s
}
