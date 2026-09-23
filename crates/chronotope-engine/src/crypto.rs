//! crypto-shredding 用の鍵保管庫。鍵素材は Revision ログに書かず、ここにだけ保存する。
//! 鍵を破棄すると、その鍵で暗号化された値はログに残っていても復号できなくなる。

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use chronotope_core::model::{ProtectedValue, Value};
use chronotope_core::{Error, KeyId, Result};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Default)]
pub struct KeyVault {
    keys: HashMap<KeyId, [u8; 32]>,
    shredded: std::collections::HashSet<KeyId>,
    path: Option<PathBuf>,
}

impl KeyVault {
    pub fn in_memory() -> Self {
        KeyVault::default()
    }

    pub fn open(path: PathBuf) -> Result<Self> {
        let mut v = KeyVault { path: Some(path.clone()), ..Default::default() };
        if let Ok(bytes) = std::fs::read(&path) {
            let m: HashMap<String, String> = serde_json::from_slice(&bytes).map_err(|e| Error::Storage(format!("key vault: {e}")))?;
            for (k, hexkey) in m {
                let id: KeyId = k.parse()?;
                let raw = hex::decode(hexkey).map_err(|e| Error::Storage(e.to_string()))?;
                let arr: [u8; 32] = raw.try_into().map_err(|_| Error::Storage("bad key length".into()))?;
                v.keys.insert(id, arr);
            }
        }
        Ok(v)
    }

    fn persist(&self) -> Result<()> {
        let Some(p) = &self.path else { return Ok(()) };
        let m: HashMap<String, String> = self.keys.iter().map(|(k, v)| (k.to_string(), hex::encode(v))).collect();
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(&m).unwrap_or_default()).map_err(|e| Error::Storage(e.to_string()))?;
        std::fs::rename(&tmp, p).map_err(|e| Error::Storage(e.to_string()))
    }

    /// 鍵を生成する（ログ再生時は既存鍵を保持し、破棄済みなら作り直さない）。
    pub fn ensure_key(&mut self, id: KeyId) -> Result<()> {
        if self.keys.contains_key(&id) || self.shredded.contains(&id) {
            return Ok(());
        }
        let key = ChaCha20Poly1305::generate_key(&mut OsRng);
        self.keys.insert(id, key.into());
        self.persist()
    }

    pub fn shred(&mut self, id: KeyId) -> Result<()> {
        self.keys.remove(&id);
        self.shredded.insert(id);
        self.persist()
    }

    pub fn has_key(&self, id: &KeyId) -> bool {
        self.keys.contains_key(id)
    }

    pub fn encrypt_bytes(&self, id: KeyId, plain: &[u8]) -> Result<(String, Vec<u8>)> {
        let key = self.keys.get(&id).ok_or_else(|| Error::not_found(format!("key {id} (shredded or never created)")))?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ct = cipher.encrypt(&nonce, plain).map_err(|e| Error::Storage(format!("encrypt: {e}")))?;
        Ok((hex::encode(nonce), ct))
    }

    pub fn decrypt_bytes(&self, id: &KeyId, nonce_hex: &str, ct: &[u8]) -> Option<Vec<u8>> {
        let key = self.keys.get(id)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
        let nonce = hex::decode(nonce_hex).ok()?;
        if nonce.len() != 12 {
            return None;
        }
        cipher.decrypt(Nonce::from_slice(&nonce), ct).ok()
    }

    pub fn protect(&self, id: KeyId, value: &Value) -> Result<ProtectedValue> {
        let plain = serde_json::to_vec(value).map_err(|e| Error::Storage(e.to_string()))?;
        let (nonce, ct) = self.encrypt_bytes(id, &plain)?;
        Ok(ProtectedValue { key_id: id, nonce, ciphertext: hex::encode(ct) })
    }

    /// 復号。鍵が破棄されていれば None。
    pub fn reveal(&self, p: &ProtectedValue) -> Option<Value> {
        let ct = hex::decode(&p.ciphertext).ok()?;
        let plain = self.decrypt_bytes(&p.key_id, &p.nonce, &ct)?;
        serde_json::from_slice(&plain).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shred_makes_value_unrecoverable() {
        let mut v = KeyVault::in_memory();
        let id = KeyId::new();
        v.ensure_key(id).unwrap();
        let p = v.protect(id, &Value::Text { text: "secret".into(), lang: None }).unwrap();
        assert!(matches!(v.reveal(&p), Some(Value::Text { .. })));
        v.shred(id).unwrap();
        assert!(v.reveal(&p).is_none());
        v.ensure_key(id).unwrap();
        assert!(!v.has_key(&id), "shredded keys are never recreated");
    }
}
