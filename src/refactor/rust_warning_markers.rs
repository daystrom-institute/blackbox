//! RX-W1a — warning marker grammar + `apply_warning_markers_to_text`.
//!
//! Stable warning grammar: `// FIXME(refactor-warning): <category> — <description>. <hints>.`

use std::collections::BTreeMap;

/// Produce a stable warning marker string.
///
/// Grammar: `// FIXME(refactor-warning): <category> — <description>. <hints>.`
pub fn emit_warning_marker(category: &str, description: &str, hints: &str) -> String {
    format!("// FIXME(refactor-warning): {category} — {description}. {hints}.")
}

/// Insert marker lines above each named line in `text`.
///
/// `site_lines` is a list of `(1-indexed line, marker text)` pairs.
/// Multiple markers targeting the same line stack above it in the order
/// they appear in the slice (first marker is closest to the code).
/// Processes in reverse line-number order for correct offset handling.
pub fn apply_warning_markers_to_text(text: &str, site_lines: &[(usize, String)]) -> String {
    if site_lines.is_empty() {
        return text.to_string();
    }

    // Compute 0-indexed start byte for each line (1-indexed line numbers).
    let line_starts = {
        let mut starts: Vec<usize> = vec![0]; // line 1 → byte 0
        for (i, _) in text.match_indices('\n') {
            starts.push(i + 1);
        }
        starts
    };

    // Group markers by line number so multiple markers on the same line
    // get stacked correctly.
    let mut by_line: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
    for (line, marker) in site_lines {
        by_line.entry(*line).or_default().push(marker);
    }

    // Process lines in descending order — inserting at a higher line
    // never shifts the byte position of a lower line.
    let mut result = text.to_string();
    for (line_num, markers) in by_line.into_iter().rev() {
        let idx = line_num.saturating_sub(1);
        let pos = line_starts.get(idx).copied().unwrap_or(result.len());

        // Reverse within the same line so the first entry in the slice
        // ends up closest to the code (last inserted).
        let block: String = markers.iter().rev().map(|m| format!("{m}\n")).collect();

        result.insert_str(pos, &block);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── W1a: emit_warning_marker ──────────────────────────────────────

    #[test]
    fn emit_warning_marker_produces_stable_grammar() {
        let marker = emit_warning_marker(
            "borrow promotion",
            "this access now goes through &mut self.dlgt even though the original read was through &self.fld",
            "cross-check no concurrent borrow",
        );
        assert_eq!(
            marker,
            "// FIXME(refactor-warning): borrow promotion — this access now goes through &mut self.dlgt even though the original read was through &self.fld. cross-check no concurrent borrow."
        );
    }

    #[test]
    fn emit_warning_marker_roundtrips_any_category() {
        let m = emit_warning_marker("cat", "desc", "hint");
        assert!(m.starts_with("// FIXME(refactor-warning): cat — desc. hint."));
    }

    // ── W1a: apply_warning_markers_to_text ─────────────────────────────

    #[test]
    fn apply_markers_empty_site_lines_is_noop() {
        let text = "line1\nline2\nline3\n";
        let result = apply_warning_markers_to_text(text, &[]);
        assert_eq!(result, text);
    }

    #[test]
    fn apply_markers_inserts_above_specified_line() {
        let text = "line1\nline2\nline3\n";
        let markers = vec![(
            2_usize,
            "// FIXME(refactor-warning): cat — desc. hints.".to_string(),
        )];
        let result = apply_warning_markers_to_text(text, &markers);
        assert_eq!(
            result,
            "line1\n// FIXME(refactor-warning): cat — desc. hints.\nline2\nline3\n"
        );
    }

    #[test]
    fn apply_markers_inserts_above_line_one() {
        let text = "first\nsecond\n";
        let markers = vec![(1_usize, "// FIXME: above first".to_string())];
        let result = apply_warning_markers_to_text(text, &markers);
        assert_eq!(result, "// FIXME: above first\nfirst\nsecond\n");
    }

    #[test]
    fn apply_markers_multiple_lines_descending_order() {
        let text = "a\nb\nc\nd\n";
        let markers = vec![
            (4_usize, "marker4".to_string()),
            (2_usize, "marker2".to_string()),
        ];
        let result = apply_warning_markers_to_text(text, &markers);
        let expected = "a\nmarker2\nb\nc\nmarker4\nd\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn apply_markers_multiple_markers_same_line() {
        let text = "top\nmiddle\nbottom\n";
        let markers = vec![
            (2_usize, "first".to_string()),
            (2_usize, "second".to_string()),
        ];
        let result = apply_warning_markers_to_text(text, &markers);
        // First marker in slice order is closest to the code.
        assert_eq!(result, "top\nsecond\nfirst\nmiddle\nbottom\n");
    }

    #[test]
    fn apply_markers_line_beyond_end_appends() {
        let text = "a\nb\n";
        let markers = vec![(99_usize, "end_marker".to_string())];
        let result = apply_warning_markers_to_text(text, &markers);
        assert_eq!(result, "a\nb\nend_marker\n");
    }

    // ── W1b integration with rust_update_callers ───────────────────────

    use super::super::rust_update_callers::plan_update_callers;
    use super::super::*;

    fn make_params_with_markers(
        source: &std::path::Path,
        struct_name: &str,
        delegate_field: &str,
        item_names: &[&str],
        emit_markers: bool,
    ) -> RefactorPlanParams {
        let toml_entries = if emit_markers {
            let mut m = std::collections::BTreeMap::new();
            m.insert(
                "emit_applied_markers".to_string(),
                serde_json::Value::Bool(true),
            );
            Some(m)
        } else {
            None
        };
        RefactorPlanParams {
            kind: "update_rust_callers".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: None,
            item_names: Some(item_names.iter().map(|&s| s.to_string()).collect()),
            item_kinds: None,
            impl_name: Some(struct_name.to_string()),
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries,
            project_dir: None,
            fields: None,
            parameters: None,
            assign_to_fields: None,
            move_fields: None,
            delegate_field: Some(delegate_field.to_string()),
            delegate_type: None,
            keep_copy: None,
            deep_analysis: None,
            rewrite_remaining_accessors: None,
            boolean_getter_strategy: None,
            declaring_class: None,
            summary_only: None,
            propagate_class_annotations: None,
            source_delegate_wrappers: None,
            wiring_mode: None,
            callback_externals: None,
            output_path: None,
            ..Default::default()
        }
    }

    /// Return the new_text from the plan JSON (pre-computed post-edit text
    /// with warning markers inserted), or None if absent.
    /// Plan fields are at the top level via `#[serde(flatten)]`.
    fn plan_new_text(plan_json: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(plan_json).unwrap();
        let edits_arr = v["edits"].as_array()?;
        let first_file = edits_arr.first()?;
        first_file["new_text"].as_str().map(|s| s.to_string())
    }

    /// Borrow-promotions field from the response (top-level extra field).
    fn plan_borrow_promotions(plan_json: &str) -> Vec<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_str(plan_json).unwrap();
        v["borrow_promotions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// fixme_count is a plan field at the top level via flatten.
    fn plan_fixme_count(plan_json: &str) -> Option<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_str(plan_json).unwrap();
        v["fixme_count"].clone().as_object()?;
        Some(v["fixme_count"].clone())
    }

    const BORROW_SOURCE: &str = r#"
struct BigServer {
    count: u32,
    state: ServerState,
}

impl BigServer {
    fn peek(&self) -> u32 {
        self.count
    }
}
"#;

    #[test]
    fn emit_applied_markers_false_no_warning_markers() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(&src, BORROW_SOURCE).unwrap();

        let p = make_params_with_markers(&src, "BigServer", "state", &["count"], false);
        let plan_json = plan_update_callers(&p).unwrap();

        // Borrow promotions should still be populated in the response.
        let promotions = plan_borrow_promotions(&plan_json);
        assert!(
            !promotions.is_empty(),
            "expected borrow_promotions even with emit_applied_markers=false"
        );

        // But new_text must be absent (no markers).
        assert!(
            plan_new_text(&plan_json).is_none(),
            "expected no new_text when emit_applied_markers=false"
        );

        // fixme_count should also be absent.
        assert!(
            plan_fixme_count(&plan_json).is_none(),
            "expected no fixme_count when emit_applied_markers=false"
        );
    }

    #[test]
    fn emit_applied_markers_true_warning_marker_inserted() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(&src, BORROW_SOURCE).unwrap();

        let p = make_params_with_markers(&src, "BigServer", "state", &["count"], true);
        let plan_json = plan_update_callers(&p).unwrap();

        // Borrow promotions should still be present.
        let promotions = plan_borrow_promotions(&plan_json);
        assert!(
            !promotions.is_empty(),
            "expected borrow_promotions with emit_applied_markers=true"
        );

        // new_text should contain the warning marker above the borrow site.
        let nt =
            plan_new_text(&plan_json).expect("expected new_text when emit_applied_markers=true");
        assert!(
            nt.contains("FIXME(refactor-warning): borrow promotion"),
            "expected warning marker in new_text:\n{nt}"
        );
        assert!(
            nt.contains("&mut self.state"),
            "expected delegate ref in marker:\n{nt}"
        );
        assert!(
            nt.contains("&self.count"),
            "expected original ref in marker:\n{nt}"
        );

        // fixme_count.warning should match the borrow_promotion count.
        let fc = plan_fixme_count(&plan_json)
            .expect("expected fixme_count when emit_applied_markers=true");
        assert_eq!(
            fc["warning"].as_u64().unwrap() as usize,
            promotions.len(),
            "fixme_count.warning should match borrow_promotion count"
        );
    }

    #[test]
    fn warning_marker_post_edit_parses() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(&src, BORROW_SOURCE).unwrap();

        let p = make_params_with_markers(&src, "BigServer", "state", &["count"], true);
        let plan_json = plan_update_callers(&p).unwrap();

        let nt = plan_new_text(&plan_json).expect("expected new_text for parse verification");

        // Post-edit text + markers must parse as valid Rust.
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("rust language");
        let tree = parser
            .parse(&nt, None)
            .expect("tree-sitter parse of post-edit text");
        assert!(
            !tree.root_node().has_error(),
            "post-edit text with warning markers should parse:\n{nt}"
        );
    }
}
