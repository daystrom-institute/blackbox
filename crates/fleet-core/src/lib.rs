//! Dependency-clean live execution authority.
//!
//! The crate deliberately stops below sockets, HTTP, provider execution, and
//! durable operational policy. Hosts supply persistence, identity, and
//! capability-routing ports while this crate enforces the state machines.

mod authority;
mod error;
mod migration;
mod model;
mod persistence;
mod ports;
mod roster;

pub use authority::{
    FleetAuthority, HandshakeAccepted, LeasePolicy, ProvisionedWorker, ProvisionedWorkerCommand,
};
pub use error::{FleetError, FleetResult};
pub use migration::{
    LegacyBroReport, LegacyBrofileContext, LegacyMigrationOptions, LegacyRuntimeLease,
    LegacyRuntimeLeaseStore, LegacyTaskRecord, LegacyTaskStatus, LegacyTranscriptLocation,
    LegacyWorkerAuthority, MigrationDiagnostic, MigrationReport, migrate_legacy_files,
    read_legacy_runtime_leases, read_legacy_tasks, read_legacy_worker_authority,
};
pub use model::{
    AdmissionRecord, AttemptRecord, CURRENT_SNAPSHOT_FORMAT, CapabilityRoute, FleetSnapshot,
    ProviderAllocationRecord, ProviderAllocationState, ProviderCredentialStatus,
    ProviderDefaultsPolicy, ProviderLaneRecord, ProviderQuotaStatus, RuntimeLeaseRecord,
    SessionEventDescriptor, SessionRecord, TaskEventObservation, TaskEventProjection, TaskRecord,
    WorkerAuthorityRecord, WorkerAuthorityState, WorktreeOwnershipRecord, WorktreeOwnershipState,
};
pub use persistence::{FileFleetRepository, MemoryFleetRepository};
pub use ports::{
    CapabilityRouter, FleetRepository, IdentityGenerator, ProviderEnvironmentResolver,
    ProviderSpawnEnvironment, UuidIdentityGenerator,
};
pub use roster::{project_task, roster_delta};
