//! content-addressed Object Storage。ハッシュ（BLAKE3）をキーにするため自動で重複排除される。

use chronotope_core::model::ContentHash;
use chronotope_core::{Error, Result};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;

pub fn content_hash(bytes: &[u8]) -> ContentHash {
    ContentHash(format!("blake3:{}", blake3::hash(bytes).to_hex()))
}

pub trait ObjectStore: Send + Sync {
    /// 保存してハッシュを返す。既に存在すれば書き込まない。
    fn put(&self, bytes: &[u8]) -> Result<(ContentHash, bool)>;
    fn get(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>>;
    fn contains(&self, hash: &ContentHash) -> bool;
}

#[derive(Default)]
pub struct MemoryObjectStore {
    objects: RwLock<HashMap<ContentHash, Vec<u8>>>,
}

impl ObjectStore for MemoryObjectStore {
    fn put(&self, bytes: &[u8]) -> Result<(ContentHash, bool)> {
        let h = content_hash(bytes);
        let mut m = self.objects.write();
        if m.contains_key(&h) {
            return Ok((h, false));
        }
        m.insert(h.clone(), bytes.to_vec());
        Ok((h, true))
    }
    fn get(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        Ok(self.objects.read().get(hash).cloned())
    }
    fn contains(&self, hash: &ContentHash) -> bool {
        self.objects.read().contains_key(hash)
    }
}

/// ローカルファイルシステム上の Object Storage（`<root>/ab/cdef...`）。
/// S3 互換ストレージへ置き換える場合もこのトレイトを実装すればよい。
pub struct FsObjectStore {
    root: PathBuf,
}

impl FsObjectStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root).map_err(|e| Error::Storage(e.to_string()))?;
        Ok(FsObjectStore { root })
    }

    fn path(&self, h: &ContentHash) -> Result<PathBuf> {
        let hex = h.0.strip_prefix("blake3:").ok_or_else(|| Error::invalid("unsupported hash"))?;
        if hex.len() < 4 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::invalid("bad hash"));
        }
        Ok(self.root.join(&hex[..2]).join(&hex[2..]))
    }
}

impl ObjectStore for FsObjectStore {
    fn put(&self, bytes: &[u8]) -> Result<(ContentHash, bool)> {
        let h = content_hash(bytes);
        let p = self.path(&h)?;
        if p.exists() {
            return Ok((h, false));
        }
        let dir = p.parent().expect("object path has a parent");
        std::fs::create_dir_all(dir).map_err(|e| Error::Storage(e.to_string()))?;
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, bytes).map_err(|e| Error::Storage(e.to_string()))?;
        std::fs::rename(&tmp, &p).map_err(|e| Error::Storage(e.to_string()))?;
        Ok((h, true))
    }
    fn get(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(hash)?) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Storage(e.to_string())),
        }
    }
    fn contains(&self, hash: &ContentHash) -> bool {
        self.path(hash).map(|p| p.exists()).unwrap_or(false)
    }
}
