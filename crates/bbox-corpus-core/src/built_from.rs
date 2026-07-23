//! Response-local provenance for mixed published and checkout-backed views.
//!
//! These stamps describe the immutable inputs pinned while one response is
//! assembled. They are deliberately separate from store load provenance and
//! from durable entity identity. Rows refer to a compact id allocated by the
//! containing response, and each distinct stamp appears in the table once.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::identity::PublishedScope;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BuiltFromStamp {
    Published {
        published_scope: PublishedScope,
        published_ref: String,
        publisher_commit: String,
    },
    CheckoutOverlay {
        published_scope: PublishedScope,
        checkout_id: String,
        publisher_commit: String,
        checkout_head: String,
        merge_base: String,
        working_fingerprint: String,
    },
}

/// Deduplicated provenance table for one assembled response.
///
/// Ids have meaning only inside the response that contains this table. They
/// are deterministic for insertion order so rows assembled from one pinned
/// view keep stable references throughout rendering.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct BuiltFromTable(BTreeMap<String, BuiltFromStamp>);

impl BuiltFromTable {
    pub fn intern(&mut self, stamp: BuiltFromStamp) -> String {
        if let Some((id, _)) = self.0.iter().find(|(_, existing)| *existing == &stamp) {
            return id.clone();
        }
        let id = (0usize..)
            .map(|index| format!("built_from_{index}"))
            .find(|candidate| !self.0.contains_key(candidate))
            .expect("response-local built_from id space exhausted");
        self.0.insert(id.clone(), stamp);
        id
    }

    pub fn get(&self, id: &str) -> Option<&BuiltFromStamp> {
        self.0.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &BuiltFromStamp)> {
        self.0.iter().map(|(id, stamp)| (id.as_str(), stamp))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn retain_ids<'a>(&mut self, ids: impl IntoIterator<Item = &'a str>) {
        let ids = ids.into_iter().collect::<BTreeSet<_>>();
        self.0.retain(|id, _| ids.contains(id.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo", ".").unwrap()
    }

    #[test]
    fn response_table_deduplicates_equal_stamps() {
        let stamp = BuiltFromStamp::Published {
            published_scope: scope(),
            published_ref: "refs/heads/main".into(),
            publisher_commit: "abc123".into(),
        };
        let mut table = BuiltFromTable::default();

        let first = table.intern(stamp.clone());
        let second = table.intern(stamp);

        assert_eq!(first, "built_from_0");
        assert_eq!(second, first);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn response_table_keeps_distinct_overlay_bytes_distinct() {
        let overlay = |fingerprint: &str| BuiltFromStamp::CheckoutOverlay {
            published_scope: scope(),
            checkout_id: "checkout".into(),
            publisher_commit: "abc123".into(),
            checkout_head: "def456".into(),
            merge_base: "abc123".into(),
            working_fingerprint: fingerprint.into(),
        };
        let mut table = BuiltFromTable::default();

        let clean = table.intern(overlay("clean"));
        let dirty = table.intern(overlay("dirty"));

        assert_ne!(clean, dirty);
        assert_eq!(table.len(), 2);
    }
}
