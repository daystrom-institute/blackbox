use std::sync::Arc;

use crate::{BlackopsResult, OperationalSnapshot};

/// Durable snapshot repository with a cross-process compare-and-swap fence.
pub trait OperationalRepository: Send + Sync {
    fn load(&self) -> BlackopsResult<Option<OperationalSnapshot>>;

    fn compare_and_swap(
        &self,
        expected_generation: u64,
        next: &OperationalSnapshot,
    ) -> BlackopsResult<()>;
}

impl<T: OperationalRepository + ?Sized> OperationalRepository for Arc<T> {
    fn load(&self) -> BlackopsResult<Option<OperationalSnapshot>> {
        (**self).load()
    }

    fn compare_and_swap(
        &self,
        expected_generation: u64,
        next: &OperationalSnapshot,
    ) -> BlackopsResult<()> {
        (**self).compare_and_swap(expected_generation, next)
    }
}

pub trait IdentityGenerator: Send + Sync {
    fn next_id(&self, prefix: &str) -> String;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UuidIdentityGenerator;

impl IdentityGenerator for UuidIdentityGenerator {
    fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }
}
