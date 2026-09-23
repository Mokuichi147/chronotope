//! Visibility（Assertion 単位の RLS）、ライセンス、主体、crypto-shredding 用の保護値。

use crate::KeyId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Agent,
    Crawler,
    Sensor,
    System,
}

/// 書き込み・取得を行った主体。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActorRef {
    pub id: String,
    pub kind: ActorKind,
}

impl ActorRef {
    pub fn system() -> Self {
        ActorRef { id: "system".into(), kind: ActorKind::System }
    }
    pub fn is_ai(&self) -> bool {
        matches!(self.kind, ActorKind::Agent)
    }
}

/// API 呼び出し元。`curator` は Assertion の承認・Identity のマージ承認・語彙の正式登録ができる。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub actor: ActorRef,
    #[serde(default)]
    pub groups: BTreeSet<String>,
    #[serde(default)]
    pub curator: bool,
}

impl Principal {
    pub fn system() -> Self {
        Principal { actor: ActorRef::system(), groups: BTreeSet::new(), curator: true }
    }
    pub fn agent(id: &str) -> Self {
        Principal { actor: ActorRef { id: id.into(), kind: ActorKind::Agent }, groups: BTreeSet::new(), curator: false }
    }
    pub fn curator(id: &str) -> Self {
        Principal { actor: ActorRef { id: id.into(), kind: ActorKind::Human }, groups: BTreeSet::new(), curator: true }
    }
    pub fn anonymous() -> Self {
        Principal { actor: ActorRef { id: "anonymous".into(), kind: ActorKind::Agent }, groups: BTreeSet::new(), curator: false }
    }

    pub fn can_see(&self, v: &Visibility) -> bool {
        match v {
            Visibility::Public => true,
            Visibility::Groups { groups } => self.actor.kind == ActorKind::System || groups.iter().any(|g| self.groups.contains(g)),
            Visibility::Private { owner } => self.actor.kind == ActorKind::System || &self.actor.id == owner,
        }
    }
}

/// 可視性。Canonical では Assertion ごとに持ち、PostgreSQL では RLS ポリシーとして強制する。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Public,
    Groups {
        groups: BTreeSet<String>,
    },
    Private {
        owner: String,
    },
}

impl Visibility {
    pub fn is_public(&self) -> bool {
        matches!(self, Visibility::Public)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct License {
    /// SPDX 等のキー（`CC-BY-4.0`, `proprietary`, `fair-use-quote` など）。
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    /// 再配布（エクスポート・スナップショット本文の提供）を許可するか。
    pub redistributable: bool,
    #[serde(default)]
    pub attribution_required: bool,
}

/// crypto-shredding 対象の値。鍵（KeyVault）を破棄すると復号不能になり、
/// Immutable な Revision を書き換えずに削除権へ対応できる。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedValue {
    pub key_id: KeyId,
    /// hex エンコードした nonce。
    pub nonce: String,
    /// hex エンコードした暗号文（中身は JSON シリアライズした Value）。
    pub ciphertext: String,
}
