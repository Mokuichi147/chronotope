//! 型付き ID。内部表現は UUID（新規は v7、語彙など既知のものは v5 で決定的に生成）。
//! 文字列表現は `<prefix>_<uuid simple>` で、どの種類の ID か一目で分かるようにする。

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// 既知 ID（語彙・既定ブランチなど）を v5 で導出するための名前空間。
pub const CHRONOTOPE_NAMESPACE: Uuid = Uuid::from_u128(0x6368_726f_6e6f_746f_7065_0000_0000_0001);

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub Uuid);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            /// 時刻順に並ぶ新しい ID（UUID v7）。
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// 名前から決定的に導出した ID（UUID v5）。語彙や既定オブジェクト用。
            pub fn named(name: &str) -> Self {
                let key = format!("{}:{}", $prefix, name);
                Self(Uuid::new_v5(&CHRONOTOPE_NAMESPACE, key.as_bytes()))
            }

            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}_{}", $prefix, self.0.simple())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }

        impl FromStr for $name {
            type Err = crate::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let body = match s.split_once('_') {
                    Some((p, rest)) if p == $prefix => rest,
                    Some((p, _)) if p.chars().all(|c| c.is_ascii_lowercase()) => {
                        return Err(crate::Error::invalid(format!(
                            "id `{s}` has prefix `{p}`, expected `{}`",
                            $prefix
                        )))
                    }
                    _ => s,
                };
                Uuid::parse_str(body)
                    .map(Self)
                    .map_err(|e| crate::Error::invalid(format!("bad id `{s}`: {e}")))
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

define_id!(
    /// Resource（Event / Entity / Place / Work / Document / Predicate / Type ...）の ID。
    ResourceId, "res"
);
define_id!(AssertionId, "asr");
define_id!(SourceId, "src");
define_id!(AcquisitionId, "acq");
define_id!(DerivationId, "drv");
define_id!(RevisionId, "rev");
define_id!(BranchId, "br");
define_id!(ObservationId, "obs");
define_id!(TrajectoryId, "trj");
define_id!(SequenceId, "seq");
define_id!(MergeProposalId, "mrg");
define_id!(FrameId, "frm");
define_id!(TableId, "tbl");
define_id!(
    /// crypto-shredding 用の暗号鍵 ID。鍵を破棄すると、その鍵で暗号化された値は復元不能になる。
    KeyId, "key"
);

impl BranchId {
    pub fn main() -> Self {
        BranchId::named("main")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let id = ResourceId::new();
        let s = id.to_string();
        assert!(s.starts_with("res_"));
        assert_eq!(s.parse::<ResourceId>().unwrap(), id);
        assert_eq!(id.0.to_string().parse::<ResourceId>().unwrap(), id);
        assert!(s.replace("res_", "asr_").parse::<ResourceId>().is_err());
        assert_eq!(ResourceId::named("x"), ResourceId::named("x"));
        assert_ne!(ResourceId::named("x"), AssertionId::named("x").0.into_res());
    }

    trait IntoRes {
        fn into_res(self) -> ResourceId;
    }
    impl IntoRes for Uuid {
        fn into_res(self) -> ResourceId {
            ResourceId(self)
        }
    }
}
