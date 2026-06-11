/// ISO-8601 UTC timestamp with second precision and a trailing `Z`.
/// Canonical format for every timestamp written into bbox stores
/// (knowledge, threads, notes, tool_docs, packets). Hoisted from per-store
/// `Self::now_iso()` duplicates so the format stays consistent; moved here
/// from `blackbox::util` so leaf crates (bbox-packets) share it.
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
