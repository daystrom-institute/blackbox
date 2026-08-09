//! bbox-indexing — extracted from the origin crate by `extract_rust_crate`.
//! Modules move verbatim; the origin re-exports them under their original
//! `crate::<module>` paths.

pub mod accepted_publication_runtime;
pub mod accepted_publication_store;
pub mod blame_locality_cutover;
pub mod blame_locality_observations;
// Test-only accepted-publication installation. `#[cfg(test)]` does not cross
// crate boundaries, so downstream tests enable `test-support` through a
// dev-dependency feature. Off by default: no production build carries it.
#[cfg(any(test, feature = "test-support"))]
pub mod accepted_publication_test_support;
pub mod catalog_records;
pub mod checkout_access;
pub mod checkout_access_v1;
pub mod checkout_access_v2;
pub mod checkout_registry;
pub mod git_transport_cutover;
pub mod index;
pub mod knowledge_transport_cutover;
pub mod knowledge_transport_observations;
pub mod project_catalog_admin;
pub mod project_catalog_backfill;
pub mod project_catalog_inventory;
pub(crate) mod project_catalog_inventory_adapters;
pub mod project_catalog_migration;
pub mod project_catalog_migration_lock;
pub mod project_catalog_rebuild;
pub mod project_catalog_rebuild_planning;
pub mod project_catalog_store;
pub mod project_resolver;
pub mod projects;
pub mod publisher;
pub mod render_locality_cutover;
pub mod render_locality_observations;
