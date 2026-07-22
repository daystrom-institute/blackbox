//! Authority boundary for repository-owned gap I/O.
//!
//! The store keeps only logical carrier identities. A daemon adapter resolves
//! them to validated checkout leases and invokes the operation while the lease
//! remains alive. Read and mutation authority are separate by construction.

use std::path::Path;

use anyhow::Result;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GapRepoCarrier {
    /// Durable project value stamped onto loaded gap notes.
    pub project: String,
    /// Opaque carrier identity resolved by the daemon authority adapter.
    pub carrier_id: String,
}

impl GapRepoCarrier {
    pub fn new(project: impl Into<String>, carrier_id: impl Into<String>) -> Result<Self> {
        let project = project.into();
        let carrier_id = carrier_id.into();
        if project.trim().is_empty() {
            anyhow::bail!("gap repository carrier project is required");
        }
        if carrier_id.trim().is_empty() {
            anyhow::bail!("gap repository carrier id is required");
        }
        Ok(Self {
            project,
            carrier_id,
        })
    }
}

pub trait GapRepoRead: Send + Sync {
    /// Invoke `operation` exactly once while read authority for `carrier`
    /// remains alive. A denial must not invoke the operation.
    fn with_read(
        &self,
        carrier: &GapRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()>;
}

pub trait GapRepoWrite: Send + Sync {
    /// Invoke `operation` exactly once while repository mutation authority for
    /// `carrier` remains alive. A denial must not invoke the operation.
    fn with_write(
        &self,
        carrier: &GapRepoCarrier,
        operation: &mut dyn FnMut(&Path) -> Result<()>,
    ) -> Result<()>;
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::RwLock;

    use anyhow::{Context, Result};

    use super::{GapRepoCarrier, GapRepoRead, GapRepoWrite};

    #[derive(Default)]
    pub(crate) struct TestGapRepoIo {
        roots: RwLock<BTreeMap<String, PathBuf>>,
    }

    impl TestGapRepoIo {
        pub(crate) fn replace(&self, carriers: &[(GapRepoCarrier, PathBuf)]) {
            let mut roots = self.roots.write().expect("test gap repo roots");
            roots.clear();
            roots.extend(
                carriers
                    .iter()
                    .map(|(carrier, root)| (carrier.carrier_id.clone(), root.clone())),
            );
        }

        fn with_root(
            &self,
            carrier: &GapRepoCarrier,
            operation: &mut dyn FnMut(&Path) -> Result<()>,
        ) -> Result<()> {
            let mapped = self
                .roots
                .read()
                .expect("test gap repo roots")
                .get(&carrier.carrier_id)
                .cloned();
            let root = mapped
                .or_else(|| {
                    let path = PathBuf::from(&carrier.carrier_id);
                    path.is_dir().then_some(path)
                })
                .with_context(|| {
                    format!("unknown test gap repository carrier {}", carrier.carrier_id)
                })?;
            operation(&root)
        }
    }

    impl GapRepoRead for TestGapRepoIo {
        fn with_read(
            &self,
            carrier: &GapRepoCarrier,
            operation: &mut dyn FnMut(&Path) -> Result<()>,
        ) -> Result<()> {
            self.with_root(carrier, operation)
        }
    }

    impl GapRepoWrite for TestGapRepoIo {
        fn with_write(
            &self,
            carrier: &GapRepoCarrier,
            operation: &mut dyn FnMut(&Path) -> Result<()>,
        ) -> Result<()> {
            self.with_root(carrier, operation)
        }
    }
}
