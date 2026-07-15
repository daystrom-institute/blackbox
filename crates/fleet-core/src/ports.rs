use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bro_core::{SessionId, WorkerId};
use bro_protocol::{CapabilityRequest, CapabilityResponse};

use crate::{CapabilityRoute, FleetResult, FleetSnapshot, ProviderAllocationRecord};

/// Durable snapshot repository. Implementations must provide compare-and-swap
/// semantics across processes, not only within one authority instance.
pub trait FleetRepository: Send + Sync {
    fn load(&self) -> FleetResult<Option<FleetSnapshot>>;

    fn compare_and_swap(&self, expected_generation: u64, next: &FleetSnapshot) -> FleetResult<()>;
}

impl<T: FleetRepository + ?Sized> FleetRepository for Arc<T> {
    fn load(&self) -> FleetResult<Option<FleetSnapshot>> {
        (**self).load()
    }

    fn compare_and_swap(&self, expected_generation: u64, next: &FleetSnapshot) -> FleetResult<()> {
        (**self).compare_and_swap(expected_generation, next)
    }
}

/// Generates opaque authority identities. Implementations should provide
/// collision-resistant values but need not expose any persistence concern.
pub trait IdentityGenerator: Send + Sync {
    fn next_id(&self, prefix: &str) -> String;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UuidIdentityGenerator;

impl IdentityGenerator for UuidIdentityGenerator {
    fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }
}

/// The host supplies transport clients for the two authority directions.
/// `fleet-core` decides which direction is allowed and never learns endpoint,
/// retry, HTTP, or socket details.
pub trait CapabilityRouter: Send + Sync {
    fn route(
        &self,
        route: CapabilityRoute,
        worker_id: &WorkerId,
        session_id: &SessionId,
        request: CapabilityRequest,
    ) -> FleetResult<CapabilityResponse>;
}

/// Spawn-only provider environment returned by the host implementation. It is
/// intentionally non-serializable and redacts its contents from `Debug`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProviderSpawnEnvironment {
    values: BTreeMap<String, String>,
    protected_paths: Vec<PathBuf>,
}

impl ProviderSpawnEnvironment {
    pub fn new(values: BTreeMap<String, String>) -> Self {
        Self {
            values,
            protected_paths: Vec::new(),
        }
    }

    pub fn with_protected_paths(
        values: BTreeMap<String, String>,
        protected_paths: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        let mut protected_paths = protected_paths.into_iter().collect::<Vec<_>>();
        protected_paths.sort();
        protected_paths.dedup();
        Self {
            values,
            protected_paths,
        }
    }

    pub fn expose(&self) -> &BTreeMap<String, String> {
        &self.values
    }

    /// Host-only credential sources that the worker sandbox must make
    /// unreadable and immutable. Paths never enter durable fleet state or the
    /// worker environment.
    pub fn protected_paths(&self) -> impl Iterator<Item = &Path> {
        self.protected_paths.iter().map(PathBuf::as_path)
    }
}

impl std::fmt::Debug for ProviderSpawnEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderSpawnEnvironment")
            .field("keys", &self.values.keys().collect::<Vec<_>>())
            .field("values", &"[REDACTED]")
            .field("protected_path_count", &self.protected_paths.len())
            .finish()
    }
}

/// Service-host port for resolving credentials and provider-specific process
/// configuration after a secret-free capacity allocation is durable.
pub trait ProviderEnvironmentResolver: Send + Sync {
    fn resolve(
        &self,
        allocation: &ProviderAllocationRecord,
    ) -> FleetResult<ProviderSpawnEnvironment>;
}
