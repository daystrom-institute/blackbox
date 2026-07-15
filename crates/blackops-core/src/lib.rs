//! Durable operational authority for agents, mailboxes, definitions, schedules,
//! and operation-to-attempt reconciliation.

mod authority;
mod error;
mod model;
mod persistence;
mod ports;

pub use authority::BlackopsAuthority;
pub use bro_core::AgentId;
pub use error::{BlackopsError, BlackopsResult};
pub use model::*;
pub use persistence::{FileRepository, MemoryRepository};
pub use ports::{IdentityGenerator, OperationalRepository, UuidIdentityGenerator};
