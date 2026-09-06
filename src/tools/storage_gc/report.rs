use super::{StorageGcDetail, StorageGcParams};
use crate::server::state::SharedState;
use crate::storage_health::GcResult;
use anyhow::{Result, bail};
use bbox_packets::PacketGcReport;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

const RECEIPT_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_RECEIPTS: usize = 16;
const CACHE_BYTE_TARGET: usize = 64 * 1024 * 1024;

struct Receipt {
    id: String,
    owner: Weak<SharedState>,
    created: Instant,
    summary: Value,
    details: Value,
    bytes: usize,
}

#[derive(Default)]
struct Receipts {
    entries: VecDeque<Arc<Receipt>>,
}

impl Receipts {
    fn expire(&mut self, now: Instant) {
        self.entries.retain(|entry| {
            now.saturating_duration_since(entry.created) < RECEIPT_TTL
                && entry.owner.strong_count() > 0
        });
    }

    fn insert(&mut self, receipt: Arc<Receipt>) {
        self.expire(receipt.created);
        let mut bytes = self.entries.iter().map(|entry| entry.bytes).sum::<usize>();
        while !self.entries.is_empty()
            && (self.entries.len() >= MAX_RECEIPTS
                || bytes.saturating_add(receipt.bytes) > CACHE_BYTE_TARGET)
        {
            bytes = bytes.saturating_sub(self.entries.pop_front().unwrap().bytes);
        }
        self.entries.push_back(receipt);
    }

    fn get(&mut self, owner: &Arc<SharedState>, id: &str, now: Instant) -> Result<Arc<Receipt>> {
        self.expire(now);
        self.entries
            .iter()
            .find(|entry| entry.id == id && entry.owner.ptr_eq(&Arc::downgrade(owner)))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!(
                "error.not_found: GC receipt unknown, expired, evicted, or from another daemon. \
                 No GC was run. Do not repeat apply to recover details."
            ))
    }
}

fn receipts() -> &'static Mutex<Receipts> {
    static RECEIPTS: OnceLock<Mutex<Receipts>> = OnceLock::new();
    RECEIPTS.get_or_init(|| Mutex::new(Receipts::default()))
}

pub(super) fn read(
    owner: &Arc<SharedState>,
    receipt_id: &str,
    params: &StorageGcParams,
) -> Result<String> {
    let receipt = receipts().lock().get(owner, receipt_id, Instant::now())?;
    render(&receipt, params)
}

pub(super) fn publish(
    owner: &Arc<SharedState>,
    params: &StorageGcParams,
    result: GcResult,
    exclusions: Value,
    excluded_count: usize,
    packets: Option<Result<PacketGcReport>>,
) -> Result<String> {
    let (summary, details) = project(params.dry_run, result, exclusions, excluded_count, packets);
    let receipt = Arc::new(Receipt {
        id: uuid::Uuid::new_v4().to_string(),
        owner: Arc::downgrade(owner),
        created: Instant::now(),
        bytes: serde_json::to_vec(&details)?.len(),
        summary,
        details,
    });
    receipts().lock().insert(Arc::clone(&receipt));
    render(&receipt, params)
}

fn project(
    dry_run: bool,
    result: GcResult,
    exclusions: Value,
    excluded_count: usize,
    packets: Option<Result<PacketGcReport>>,
) -> (Value, Value) {
    let deleted: HashSet<&str> = result
        .deleted
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let deleted_bytes = result
        .candidates
        .iter()
        .filter(|candidate| deleted.contains(candidate.path.as_str()))
        .fold(0u64, |total, candidate| {
            total.saturating_add(candidate.bytes)
        });
    let delete_error_count = result.delete_errors.as_ref().map_or(0, Vec::len);
    let (packet_summary, packet_details, packet_error_count) = match packets {
        None => (json!({"status":"not_requested"}), Value::Null, 0),
        Some(Err(error)) => (
            json!({"status":"failed", "error_count":1}),
            json!({"report":null, "errors":[format!("{error:#}")]}),
            1,
        ),
        Some(Ok(report)) => {
            let errors = report.errors.len();
            let summary = json!({
                "status": outcome(dry_run, errors > 0),
                "scanned": report.scanned,
                "duplicate_candidates": report.duplicate_candidates,
                "deleted_count": if dry_run { 0 } else { report.deleted },
                "would_delete_count": if dry_run { report.deleted } else { 0 },
                "protected_by_refs": report.protected_by_refs,
                "orphan_lock_candidates": report.orphan_lock_candidates,
                "orphan_locks_removed": report.orphan_locks_removed,
                "domain_count": report.per_domain.len(),
                "error_count": errors,
                "scope": "all",
            });
            let mut detail = json!(report);
            detail.as_object_mut().unwrap().remove("applied");
            detail["status"] = json!(outcome(dry_run, errors > 0));
            (summary, json!({"report":detail}), errors)
        }
    };
    let incomplete = delete_error_count > 0 || packet_error_count > 0;
    let summary = json!({
        "status": outcome(dry_run, incomplete),
        "apply_requested": !dry_run,
        "result": {
            "candidate_count": result.candidates.len(),
            "deletable_count": result.deletable_count,
            "deletable_bytes": result.deletable_bytes,
            "retained_count": result.candidates.len().saturating_sub(result.deletable_count),
            "operator_review_count": result.candidates.iter()
                .filter(|candidate| candidate.rule.starts_with("observed_over_cap_operator_review"))
                .count(),
            "deleted_count": deleted.len(),
            "deleted_bytes_estimate": deleted_bytes,
            "delete_error_count": delete_error_count,
            "unconfirmed_count": if dry_run { 0 } else {
                result.deletable_count.saturating_sub(deleted.len())
            },
        },
        "packet_gc": packet_summary,
        "catalog_gc_exclusions": {
            "kind": exclusions["kind"],
            "protected_root_count": exclusions["roots"].as_array().map_or(0, Vec::len),
            "named_immutable_assets": exclusions["named_immutable_assets"].as_u64().unwrap_or(0),
            "filtered_candidate_count": excluded_count,
        },
    });
    let mut edge_details = json!(result);
    edge_details.as_object_mut().unwrap().remove("applied");
    edge_details["status"] = json!(outcome(dry_run, delete_error_count > 0));
    edge_details["apply_requested"] = json!(!dry_run);
    let details = json!({
        "result": edge_details,
        "catalog_gc_exclusions": exclusions,
        "packet_gc": packet_details,
    });
    (summary, details)
}

fn outcome(dry_run: bool, incomplete: bool) -> &'static str {
    match (dry_run, incomplete) {
        (true, false) => "dry_run",
        (true, true) => "incomplete",
        (false, false) => "applied",
        (false, true) => "partial",
    }
}

fn render(receipt: &Receipt, params: &StorageGcParams) -> Result<String> {
    let mut response = if params.detail == StorageGcDetail::Summary || params.receipt_id.is_none() {
        receipt.summary.clone()
    } else {
        json!({
            "status":receipt.summary["status"],
            "apply_requested":receipt.summary["apply_requested"],
        })
    };
    response["receipt_id"] = json!(receipt.id);
    response["expires_in_seconds"] = json!(
        RECEIPT_TTL
            .saturating_sub(receipt.created.elapsed())
            .as_secs()
    );
    if params.detail != StorageGcDetail::Summary {
        let selected = match params.detail {
            StorageGcDetail::Candidates => Cow::Borrowed(&receipt.details["result"]["candidates"]),
            StorageGcDetail::Deleted => Cow::Borrowed(&receipt.details["result"]["deleted"]),
            StorageGcDetail::Errors => Cow::Owned(json!({
                "edge":receipt.details["result"]["delete_errors"],
                "packets":receipt.details["packet_gc"]["report"]["errors"],
                "packet_stage":receipt.details["packet_gc"]["errors"],
            })),
            StorageGcDetail::Exclusions => Cow::Borrowed(&receipt.details["catalog_gc_exclusions"]),
            StorageGcDetail::Packets => Cow::Borrowed(&receipt.details["packet_gc"]),
            StorageGcDetail::Full => Cow::Borrowed(&receipt.details),
            StorageGcDetail::Summary => unreachable!(),
        };
        let scope = format!("{}:{:?}", receipt.id, params.detail);
        response["detail"] = json!(params.detail);
        response["body"] = crate::tools::body_page::json_body_page(
            &scope,
            selected.as_ref(),
            params.cursor.as_deref(),
            params.limit,
        )?;
    } else if params.cursor.is_some() {
        bail!("error.bad_input: summary has no body cursor");
    }
    Ok(serde_json::to_string(&response)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_health::{FileKind, GcCandidate};
    use std::collections::BTreeMap;

    fn candidate(index: usize, deletable: bool) -> GcCandidate {
        GcCandidate {
            path: format!("/daemon/edges/{index}-{}", "\u{0001}🦀\n".repeat(20)),
            root_relative_path: None,
            planned_device: None,
            planned_inode: None,
            planned_mtime_secs: None,
            kind: FileKind::Backup,
            bytes: 100,
            project_id: Some("fixture".into()),
            rule: if deletable {
                "old_backup"
            } else {
                "keep_newest"
            }
            .into(),
            deletable,
        }
    }

    fn edge_result(dry_run: bool) -> GcResult {
        let candidates = vec![candidate(0, true), candidate(1, true), candidate(2, false)];
        GcResult {
            applied: !dry_run,
            deleted: (!dry_run)
                .then(|| vec![candidates[0].path.clone(), candidates[1].path.clone()]),
            candidates,
            deletable_count: 2,
            deletable_bytes: 200,
            delete_errors: None,
        }
    }

    fn packet_report() -> PacketGcReport {
        PacketGcReport {
            apply_requested: true,
            applied: true,
            scanned: 100,
            deleted: 50,
            protected_by_refs: 3,
            orphan_locks_removed: 2,
            orphan_lock_candidates: 2,
            duplicate_candidates: 50,
            errors: Vec::new(),
            per_domain: BTreeMap::from([("fixture".into(), 50)]),
        }
    }

    fn receipt(summary: Value, details: Value) -> Receipt {
        Receipt {
            id: uuid::Uuid::new_v4().to_string(),
            owner: Weak::new(),
            created: Instant::now(),
            bytes: serde_json::to_vec(&details).unwrap().len(),
            summary,
            details,
        }
    }

    fn params(value: Value) -> StorageGcParams {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn summary_bounds_all_inventories_but_preserves_counts_and_outcome() {
        let mut result = edge_result(false);
        result.candidates = (0..2000).map(|index| candidate(index, true)).collect();
        result.deletable_count = 2000;
        result.deletable_bytes = 200_000;
        result.deleted = Some(
            result.candidates[..1000]
                .iter()
                .map(|row| row.path.clone())
                .collect(),
        );
        result.delete_errors = Some(vec!["large edge error".repeat(500); 1000]);
        result.applied = false;
        let roots: Vec<_> = (0..2000)
            .map(|index| json!({"path":format!("/protected/{index}")}))
            .collect();
        let exclusions = json!({
            "kind":"marker_driven",
            "named_immutable_assets":2000,
            "roots":roots,
        });
        let mut packets = packet_report();
        packets.per_domain = (0..2000)
            .map(|index| (format!("domain-{index}"), 1))
            .collect();
        let (summary, details) = project(false, result, exclusions, 5, Some(Ok(packets)));
        let receipt = receipt(summary, details);
        let encoded = render(&receipt, &params(json!({}))).unwrap();
        assert!(encoded.len() < 2048, "{}", encoded.len());
        assert!(!encoded.contains("/daemon/"));
        assert!(!encoded.contains("/protected/"));
        assert!(!encoded.contains("large edge error"));
        assert!(!encoded.contains("domain-1999"));
        let response: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(response["status"], "partial");
        assert!(response.get("applied").is_none());
        assert!(response.get("outcome_note").is_none());
        assert!(response.get("receipt").is_none());
        assert!(response.get("detail_hint").is_none());
        assert!(response["expires_in_seconds"].as_u64().unwrap() <= RECEIPT_TTL.as_secs());
        assert_eq!(response["result"]["candidate_count"], 2000);
        assert_eq!(response["result"]["deleted_count"], 1000);
        assert_eq!(response["result"]["deleted_bytes_estimate"], 100_000);
        assert_eq!(response["result"]["delete_error_count"], 1000);
        assert_eq!(
            response["catalog_gc_exclusions"]["protected_root_count"],
            2000
        );
        assert_eq!(
            response["catalog_gc_exclusions"]["filtered_candidate_count"],
            5
        );
        assert_eq!(response["packet_gc"]["domain_count"], 2000);
        assert!(response.get("body").is_none());
    }

    #[test]
    fn packet_failure_does_not_erase_successful_edge_effects() {
        let (summary, details) = project(
            false,
            edge_result(false),
            json!({"kind":"exempt_fresh_origin"}),
            0,
            Some(Err(anyhow::anyhow!("cannot read packets"))),
        );
        assert_eq!(summary["status"], "partial");
        assert!(summary.get("applied").is_none());
        assert!(details["result"].get("applied").is_none());
        assert_eq!(summary["apply_requested"], true);
        assert_eq!(summary["result"]["deleted_count"], 2);
        assert_eq!(summary["result"]["deleted_bytes_estimate"], 200);
        assert_eq!(summary["packet_gc"]["status"], "failed");
        assert_eq!(details["result"]["deleted"].as_array().unwrap().len(), 2);
        assert_eq!(details["packet_gc"]["errors"][0], "cannot read packets");
    }

    #[test]
    fn preview_and_partial_packet_results_are_not_claimed_as_applied() {
        let (preview, _) = project(true, edge_result(true), Value::Null, 0, None);
        assert_eq!(preview["status"], "dry_run");
        assert_eq!(preview["apply_requested"], false);
        assert_eq!(preview["result"]["deleted_count"], 0);
        let (incomplete, _) = project(
            true,
            edge_result(true),
            Value::Null,
            0,
            Some(Err(anyhow::anyhow!("preview failed"))),
        );
        assert_eq!(incomplete["status"], "incomplete");
        assert_eq!(incomplete["apply_requested"], false);
        let mut packets = packet_report();
        packets.applied = false;
        packets.errors.push("second deletion failed".into());
        let (partial, details) =
            project(false, edge_result(false), Value::Null, 0, Some(Ok(packets)));
        assert_eq!(partial["status"], "partial");
        assert_eq!(partial["packet_gc"]["deleted_count"], 50);
        assert_eq!(partial["packet_gc"]["error_count"], 1);
        assert!(details["packet_gc"]["report"].get("applied").is_none());
        assert_eq!(
            details["packet_gc"]["report"]["errors"][0],
            "second deletion failed"
        );
        let (applied, _) = project(false, edge_result(false), Value::Null, 0, None);
        assert_eq!(applied["status"], "applied");
        assert_eq!(applied["apply_requested"], true);
        assert!(applied.get("applied").is_none());
    }

    #[test]
    fn detail_pages_recover_exact_json_and_reject_changed_receipt_or_selection() {
        let mut result = edge_result(false);
        result.delete_errors = Some(vec!["\u{0001}🦀\n".repeat(500)]);
        let (summary, details) = project(false, result, json!({"roots":["protected"]}), 0, None);
        let receipt = receipt(summary, details);
        for detail in [
            "candidates",
            "deleted",
            "errors",
            "exclusions",
            "packets",
            "full",
        ] {
            let mut request =
                params(json!({"receipt_id":receipt.id, "detail":detail, "limit":usize::MAX}));
            let mut combined = String::new();
            loop {
                let encoded = render(&receipt, &request).unwrap();
                assert!(encoded.len() < 4608);
                let page: Value = serde_json::from_str(&encoded).unwrap();
                assert!(page.get("result").is_none());
                assert!(page.get("receipt").is_none());
                assert!(page.get("catalog_gc_exclusions").is_none());
                assert_eq!(page["status"], "partial");
                assert_eq!(page["detail"], detail);
                assert!(serde_json::to_vec(&page["body"]).unwrap().len() <= 4096);
                combined.push_str(page["body"]["text"].as_str().unwrap());
                request.cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
                if request.cursor.is_none() {
                    break;
                }
            }
            let recovered: Value = serde_json::from_str(&combined).unwrap();
            let expected = match detail {
                "candidates" => receipt.details["result"]["candidates"].clone(),
                "deleted" => receipt.details["result"]["deleted"].clone(),
                "errors" => {
                    json!({"edge":receipt.details["result"]["delete_errors"],"packets":null,"packet_stage":null})
                }
                "exclusions" => receipt.details["catalog_gc_exclusions"].clone(),
                "packets" => receipt.details["packet_gc"].clone(),
                "full" => receipt.details.clone(),
                _ => unreachable!(),
            };
            assert_eq!(recovered, expected);
            assert!(!combined.contains("\"applied\":"));
        }
        let mut request = params(json!({"detail":"full", "limit":4}));
        let first: Value = serde_json::from_str(&render(&receipt, &request).unwrap()).unwrap();
        request.cursor = Some(first["body"]["next_cursor"].as_str().unwrap().into());
        request.detail = StorageGcDetail::Candidates;
        assert!(render(&receipt, &request).is_err());
        request.detail = StorageGcDetail::Full;
        let other = self::receipt(receipt.summary.clone(), receipt.details.clone());
        assert!(render(&other, &request).is_err());
    }

    #[test]
    fn first_detail_response_keeps_effect_counts_but_receipt_pages_are_compact() {
        let mut result = edge_result(false);
        result.delete_errors = Some(vec!["failure after deletion".into()]);
        let (summary, details) = project(false, result, Value::Null, 0, None);
        let receipt = receipt(summary, details);
        let first: Value = serde_json::from_str(
            &render(
                &receipt,
                &params(json!({"dry_run":false,"detail":"errors"})),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(first["status"], "partial");
        assert_eq!(first["result"]["deleted_count"], 2);
        assert_eq!(first["result"]["delete_error_count"], 1);
        assert!(first["body"]["text"].is_string());
        let page: Value = serde_json::from_str(
            &render(
                &receipt,
                &params(json!({"receipt_id":receipt.id,"detail":"errors"})),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(page.get("result").is_none());
        assert_eq!(page["body"], first["body"]);
        let reread: Value = serde_json::from_str(
            &render(&receipt, &params(json!({"receipt_id":receipt.id}))).unwrap(),
        )
        .unwrap();
        assert_eq!(reread["result"], first["result"]);
    }

    #[test]
    fn receipt_cache_expires_evicts_and_isolates_daemon_instances() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let owner = Arc::new(SharedState::for_test(&root));
        let other = Arc::new(SharedState::for_test(&root.join("other")));
        let mut cache = Receipts::default();
        let now = Instant::now();
        let mut ids = Vec::new();
        for _ in 0..=MAX_RECEIPTS {
            let mut receipt = receipt(json!({}), json!({}));
            receipt.owner = Arc::downgrade(&owner);
            receipt.created = now;
            ids.push(receipt.id.clone());
            cache.insert(Arc::new(receipt));
        }
        assert_eq!(cache.entries.len(), MAX_RECEIPTS);
        assert!(cache.get(&owner, &ids[0], now).is_err());
        assert!(cache.get(&other, &ids[1], now).is_err());
        assert!(cache.get(&owner, &ids[1], now).is_ok());
        assert!(cache.get(&owner, &ids[1], now + RECEIPT_TTL).is_err());
        assert!(cache.entries.is_empty());
        let mut oversized = receipt(json!({}), json!({}));
        oversized.owner = Arc::downgrade(&owner);
        oversized.bytes = CACHE_BYTE_TARGET + 1;
        let id = oversized.id.clone();
        cache.insert(Arc::new(oversized));
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.get(&owner, &id, now).is_ok());
    }

    #[tokio::test]
    async fn tool_receipt_reads_preserve_original_apply_result_without_rerunning_gc() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let owner = Arc::new(SharedState::for_test(&root));
        let server = crate::server::BlackboxServer::new(Arc::clone(&owner));
        let encoded = publish(
            &owner,
            &params(json!({"dry_run":false})),
            edge_result(false),
            json!({"kind":"exempt_non_catalog_store"}),
            0,
            Some(Err(anyhow::anyhow!("original packet failure"))),
        )
        .unwrap();
        let original: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(original["result"]["deleted_count"], 2);
        for _ in 0..2 {
            let response = server
                .bbox_storage_gc(rmcp::handler::server::wrapper::Parameters(params(
                    json!({"receipt_id":original["receipt_id"],"detail":"errors"}),
                )))
                .await;
            assert!(!response.is_error.unwrap_or(false));
            let page: Value =
                serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(page["status"], "partial");
            assert_eq!(page["apply_requested"], true);
            assert!(page.get("result").is_none());
            let body: Value = serde_json::from_str(page["body"]["text"].as_str().unwrap()).unwrap();
            assert_eq!(body["packet_stage"][0], "original packet failure");
        }
        let response = server
            .bbox_storage_gc(rmcp::handler::server::wrapper::Parameters(params(
                json!({"receipt_id":original["receipt_id"],"dry_run":false}),
            )))
            .await;
        assert_eq!(response.is_error, Some(true));
    }
}
