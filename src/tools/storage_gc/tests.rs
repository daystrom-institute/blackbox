use super::*;
use serde_json::json;

#[test]
fn gc_defaults_keep_policy_and_return_summary() {
    let params: StorageGcParams = serde_json::from_value(json!({})).unwrap();
    params.validate().unwrap();
    assert_eq!(params.detail, StorageGcDetail::Summary);
    assert!(params.dry_run);
    assert!(params.prune_backups);
    assert!(params.prune_temps);
    assert!(!params.prune_orphans);
    assert!(!params.prune_inactive_snapshots);
    assert!(!params.prune_duplicate_packets);
    assert!(!params.prune_explicitly_unregistered);
    assert_eq!(params.keep_newest_backup_per_source, 1);
    assert_eq!(params.max_snapshots_per_workspace, Some(32));
    assert_eq!(
        params.max_snapshot_total_bytes_per_workspace,
        Some(8 * 1024 * 1024 * 1024)
    );
}

#[test]
fn gc_rejects_mutating_or_ambiguous_pagination_before_work() {
    for input in [
        json!({"dry_run":false, "cursor":"cursor", "detail":"full"}),
        json!({"receipt_id":"receipt", "dry_run":false}),
        json!({"receipt_id":"receipt", "prune_duplicate_packets":true}),
        json!({"receipt_id":"receipt", "project":"different"}),
        json!({"receipt_id":"receipt", "keep_newest_backup_per_source":0}),
        json!({"receipt_id":"receipt", "max_backup_total_bytes":null}),
        json!({"receipt_id":"receipt", "cursor":"cursor"}),
        json!({"limit":10}),
    ] {
        let params: StorageGcParams = serde_json::from_value(input.clone()).unwrap();
        assert!(params.validate().is_err(), "{input}");
    }
    for input in [
        json!({"offset":10, "dry_run":false}),
        json!({"detail":"unbounded"}),
    ] {
        assert!(serde_json::from_value::<StorageGcParams>(input).is_err());
    }
    for input in [
        json!({"dry_run":false, "detail":"errors", "limit":4096}),
        json!({"receipt_id":"receipt", "detail":"full", "cursor":"cursor"}),
        json!({"receipt_id":"receipt"}),
    ] {
        serde_json::from_value::<StorageGcParams>(input)
            .unwrap()
            .validate()
            .unwrap();
    }
}
