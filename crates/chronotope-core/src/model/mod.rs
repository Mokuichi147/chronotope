//! Canonical Knowledge Model のデータ型。

pub mod assertion;
pub mod identity;
pub mod observation;
pub mod predicate;
pub mod provenance;
pub mod resource;
pub mod revision;
pub mod security;
pub mod table;
pub mod work;

pub use assertion::*;
pub use identity::*;
pub use observation::*;
pub use predicate::*;
pub use provenance::*;
pub use resource::*;
pub use revision::*;
pub use security::*;
pub use table::*;
pub use work::*;
