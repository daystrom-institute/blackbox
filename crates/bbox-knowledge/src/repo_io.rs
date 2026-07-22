//! Authority boundary for repository-owned knowledge I/O.
//!
//! The knowledge store retains logical carrier identities, never checkout
//! roots. A daemon adapter may resolve a carrier through the checkout-access
//! broker and invoke the supplied callback while its validated lease remains
//! alive. Read and write authority are deliberately separate so an operation
//! such as recall telemetry persistence cannot run under a read-only lease.

use std::path::Path;

use anyhow::Result;

/// Logical repository carrier for one durable knowledge project scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KnowledgeRepoCarrier {
    /// Durable project value stamped onto loaded entries.
    pub project: String,
    /// Opaque identity resolved by the authority adapter.
    pub carrier_id: String,
}

impl KnowledgeRepoCarrier {
    pub fn new(project: impl Into<String>, carrier_id: impl Into<String>) -> Result<Self> {
        let project = project.into();
        let carrier_id = carrier_id.into();
        if project.trim().is_empty() {
            anyhow::bail!("knowledge repository carrier project is required");
        }
        if carrier_id.trim().is_empty() {
            anyhow::bail!("knowledge repository carrier id is required");
        }
        Ok(Self {
            project,
            carrier_id,
        })
    }
}

/// Resolves a logical carrier for one read operation.
pub trait KnowledgeRepoRead: Send + Sync {
    /// Invoke `operation` while read authority for `carrier` remains alive.
    /// The root must not escape the callback.
    fn with_read(
        &self,
        carrier: &KnowledgeRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()>;
}

/// Resolves a logical carrier for one repository mutation.
pub trait KnowledgeRepoWrite: Send + Sync {
    /// Invoke `operation` while write authority for `carrier` remains alive.
    /// Reads needed to implement an atomic mutation are covered by this write
    /// authority. The root must not escape the callback.
    fn with_write(
        &self,
        carrier: &KnowledgeRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()>;
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::RwLock;

    use anyhow::{Context, Result};

    use super::{KnowledgeRepoCarrier, KnowledgeRepoRead, KnowledgeRepoWrite};

    /// Test-only direct filesystem adapter. Production code must provide a
    /// broker-backed adapter from the daemon boundary.
    #[derive(Default)]
    pub(crate) struct TestKnowledgeRepoIo {
        roots: RwLock<BTreeMap<String, PathBuf>>,
    }

    impl TestKnowledgeRepoIo {
        pub(crate) fn replace(&self, carriers: &[(KnowledgeRepoCarrier, PathBuf)]) {
            let mut roots = self.roots.write().expect("test knowledge repo roots");
            roots.clear();
            roots.extend(
                carriers
                    .iter()
                    .map(|(carrier, root)| (carrier.carrier_id.clone(), root.clone())),
            );
        }

        fn with_root(
            &self,
            carrier: &KnowledgeRepoCarrier,
            operation: &mut dyn FnMut(&Path) -> Result<()>,
        ) -> Result<()> {
            let mapped = self
                .roots
                .read()
                .expect("test knowledge repo roots")
                .get(&carrier.carrier_id)
                .cloned();
            let root = mapped
                .or_else(|| {
                    let path = PathBuf::from(&carrier.carrier_id);
                    path.is_dir().then_some(path)
                })
                .with_context(|| {
                    format!(
                        "unknown test knowledge repository carrier {}",
                        carrier.carrier_id
                    )
                })?;
            operation(&root)
        }
    }

    impl KnowledgeRepoRead for TestKnowledgeRepoIo {
        fn with_read(
            &self,
            carrier: &KnowledgeRepoCarrier,
            operation: &mut dyn FnMut(&Path) -> Result<()>,
        ) -> Result<()> {
            self.with_root(carrier, operation)
        }
    }

    impl KnowledgeRepoWrite for TestKnowledgeRepoIo {
        fn with_write(
            &self,
            carrier: &KnowledgeRepoCarrier,
            operation: &mut dyn FnMut(&Path) -> Result<()>,
        ) -> Result<()> {
            self.with_root(carrier, operation)
        }
    }
}
