//! bbox-indexing — extracted from the origin crate by `extract_rust_crate`.
//! Modules move verbatim; the origin re-exports them under their original
//! `crate::<module>` paths.

pub mod accepted_publication_runtime;
pub mod accepted_publication_store;
pub mod catalog_records;
pub mod checkout_access;
pub mod checkout_access_v1;
pub mod checkout_access_v2;
pub mod checkout_registry;
pub mod index;
pub mod project_catalog_admin;
pub mod project_catalog_inventory;
pub(crate) mod project_catalog_inventory_adapters;
pub mod project_catalog_migration;
pub mod project_catalog_migration_lock;
pub mod project_catalog_store;
pub mod project_resolver;
pub mod projects;
pub mod publisher;
