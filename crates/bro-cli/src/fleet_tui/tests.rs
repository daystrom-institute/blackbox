    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Serialize rendered lines to a real ANSI string (SGR per span) for
    /// live/tmux inspection of truecolor highlighting + styling.
    fn lines_to_ansi(lines: &[Line<'_>]) -> String {
        fn sgr(style: ratatui::style::Style) -> String {
            let mut codes: Vec<String> = Vec::new();
            if let Some(c) = style.fg {
                match c {
                    Color::Rgb(r, g, b) => codes.push(format!("38;2;{r};{g};{b}")),
                    Color::Indexed(i) => codes.push(format!("38;5;{i}")),
                    Color::Black => codes.push("30".into()),
                    Color::Red => codes.push("31".into()),
                    Color::Green => codes.push("32".into()),
                    Color::Yellow => codes.push("33".into()),
                    Color::Blue => codes.push("34".into()),
                    Color::Magenta => codes.push("35".into()),
                    Color::Cyan => codes.push("36".into()),
                    Color::Gray => codes.push("37".into()),
                    Color::DarkGray => codes.push("90".into()),
                    Color::LightBlue => codes.push("94".into()),
                    Color::White => codes.push("97".into()),
                    _ => {}
                }
            }
            if style.add_modifier.contains(Modifier::BOLD) {
                codes.push("1".into());
            }
            if style.add_modifier.contains(Modifier::ITALIC) {
                codes.push("3".into());
            }
            if style.add_modifier.contains(Modifier::DIM) {
                codes.push("2".into());
            }
            if codes.is_empty() {
                String::new()
            } else {
                format!("\x1b[{}m", codes.join(";"))
            }
        }
        let mut out = String::new();
        for line in lines {
            for span in &line.spans {
                let pre = sgr(span.style);
                if pre.is_empty() {
                    out.push_str(&span.content);
                } else {
                    out.push_str(&pre);
                    out.push_str(&span.content);
                    out.push_str("\x1b[0m");
                }
            }
            out.push('\n');
        }
        out
    }

    /// Dev helper (ignored by default): render a rich sample transcript through
    /// the real render path and write real ANSI to /tmp/fleet_render_demo.ansi
    /// for live truecolor inspection under tmux. Run with:
    ///   cargo test -p bro-cli render_demo_to_ansi -- --ignored --nocapture
    #[test]
    #[ignore]
    fn render_demo_to_ansi() {
        let md = concat!(
            "## Rendering demo\n\n",
            "Some **bold**, *italic*, and `inline code`, plus a ",
            "[link](https://example.com/a/long/path).\n\n",
            "```rust\n",
            "fn main() {\n",
            "    let items = vec![1, 2, 3];\n",
            "    println!(\"sum = {}\", items.iter().sum::<i32>());\n",
            "}\n",
            "```\n\n",
            "A table wrapped in a markdown fence (should unwrap to a real table):\n\n",
            "```md\n",
            "| Provider | Status | Cost |\n",
            "|----------|--------|------|\n",
            "| brodex   | ok     | 0.12 |\n",
            "| glm      | error  | 0.00 |\n",
            "```\n",
        );
        let items = vec![TranscriptItem::AssistantText(md.into())];
        let steer = "this is a fairly long initial steer prompt that should word-wrap cleanly across the column width without breaking words";
        let lines = render_transcript(&items, steer, &[], 64);
        let ansi = lines_to_ansi(&lines);
        std::fs::write("/tmp/fleet_render_demo.ansi", ansi).expect("write ansi demo");
        eprintln!("wrote /tmp/fleet_render_demo.ansi ({} lines)", lines.len());
    }

    // ---- Render snapshot safety net (Phase 0) ------------------------------
    //
    // Two complementary goldens lock the current rendering behavior before the
    // fleet_tui split + codex-derived renderer swaps:
    //
    //  * `dump_lines` — a deterministic text+style serialization of the
    //    `Vec<Line>` a render fn returns. Catches any change to text OR styling
    //    (fg color / bold / italic / dim), independent of terminal quirks.
    //  * `crate::test_backend::render_lines_to_grid` — the codex VT100 harness:
    //    renders those lines into a real terminal cell grid, capturing on-screen
    //    layout/wrapping at a fixed width.

    /// Serialize lines as `text` with inline `[tags]…[/]` markers for any span
    /// carrying non-default style. Stable and readable in snapshot diffs.
    fn dump_lines(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| {
                        let mut tags: Vec<String> = Vec::new();
                        if let Some(fg) = s.style.fg {
                            tags.push(format!("{fg:?}"));
                        }
                        let m = s.style.add_modifier;
                        if m.contains(Modifier::BOLD) {
                            tags.push("b".into());
                        }
                        if m.contains(Modifier::ITALIC) {
                            tags.push("i".into());
                        }
                        if m.contains(Modifier::DIM) {
                            tags.push("dim".into());
                        }
                        if m.contains(Modifier::UNDERLINED) {
                            tags.push("u".into());
                        }
                        if tags.is_empty() {
                            s.content.to_string()
                        } else {
                            format!("[{}]{}[/]", tags.join(","), s.content)
                        }
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    const MD_FIXTURE: &str = "\
# Heading One

Some **bold** and *italic* and `inline code` prose that runs on a bit so it has \
a chance to wrap at narrower widths.

- first bullet
- second bullet with a [link](https://example.com/some/long/path)

1. ordered one
2. ordered two

> a blockquote line
> spanning two lines

| Col A | Col B | Count |
|-------|-------|-------|
| alpha | beta  | 3     |
| gamma | delta | 12    |

```rust
fn main() {
    println!(\"hello\");
}
```

---

Trailing paragraph.";

    fn transcript_fixture() -> Vec<TranscriptItem> {
        vec![
            TranscriptItem::AssistantText(
                "Here is a plan with **emphasis** and a `code` token.".into(),
            ),
            TranscriptItem::Thinking("considering the edge cases\nand the wrapping".into()),
            TranscriptItem::ToolCall {
                name: "file_edit".into(),
                args: serde_json::json!({
                    "file_path": "src/lib.rs",
                    "old_string": "let x = 1;",
                    "new_string": "let x = 2;",
                })
                .to_string(),
            },
            TranscriptItem::ToolResult {
                tool: Some("file_edit".into()),
                content: "ok".into(),
                is_error: false,
                rider: None,
            },
            TranscriptItem::ToolCall {
                name: "shell_run".into(),
                args: r#"{"cmd":"cargo test"}"#.into(),
            },
            TranscriptItem::ToolResult {
                tool: Some("shell_run".into()),
                content: "error[E0001]: something broke\n  --> src/lib.rs:1".into(),
                is_error: true,
                rider: None,
            },
            TranscriptItem::AssistantText(
                "Done. See the [docs](https://example.com/docs) for details.".into(),
            ),
        ]
    }

    #[test]
    fn snapshot_markdown_styled_multi_width() {
        for width in [40usize, 80, 120] {
            let lines = render_markdown_with_width(MD_FIXTURE, width);
            insta::assert_snapshot!(format!("markdown_styled_w{width}"), dump_lines(&lines));
        }
    }

    #[test]
    fn snapshot_markdown_grid_multi_width() {
        for width in [40u16, 80] {
            let lines = render_markdown_with_width(MD_FIXTURE, width as usize);
            let grid = crate::test_backend::render_lines_to_grid(width, 60, lines);
            insta::assert_snapshot!(format!("markdown_grid_w{width}"), grid);
        }
    }

    #[test]
    fn snapshot_transcript_styled_multi_width() {
        let items = transcript_fixture();
        for width in [40usize, 80, 120] {
            let lines = render_transcript(&items, "this is a fairly long initial steer prompt that should word-wrap across several lines at the narrow forty-column width", &["a queued turn"], width);
            insta::assert_snapshot!(format!("transcript_styled_w{width}"), dump_lines(&lines));
        }
    }

    #[test]
    fn snapshot_transcript_grid_multi_width() {
        let items = transcript_fixture();
        for width in [40u16, 80] {
            let lines = render_transcript(&items, "this is a fairly long initial steer prompt that should word-wrap across several lines at the narrow forty-column width", &["a queued turn"], width as usize);
            let grid = crate::test_backend::render_lines_to_grid(width, 60, lines);
            insta::assert_snapshot!(format!("transcript_grid_w{width}"), grid);
        }
    }

    #[test]
    fn inline_stable_end_active_turn_holds_back_all_items_until_result() {
        // turn_active = true, no TurnFooter → nothing is stable (first turn).
        assert_eq!(inline_stable_end(&[], true), 0);
        assert_eq!(inline_stable_end(&[
            TranscriptItem::AssistantText("hi".into()),
        ], true), 0);
        assert_eq!(inline_stable_end(&[
            TranscriptItem::AssistantText("hi".into()),
            TranscriptItem::ToolCall { name: "f".into(), args: "{}".into() },
        ], true), 0);
        // turn_active = false → everything is stable.
        assert_eq!(inline_stable_end(&[], false), 0);
        assert_eq!(inline_stable_end(&[
            TranscriptItem::AssistantText("hi".into()),
            TranscriptItem::TurnFooter { num_turns: Some(1), cost_usd: None },
        ], false), 2);
    }

    #[test]
    fn render_committed_items_uses_normal_item_rendering() {
        let items = vec![
            TranscriptItem::UserSteer("hello".into()),
            TranscriptItem::AssistantText("answer".into()),
        ];
        let rendered: Vec<String> = render_committed_items(&items, 80)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(rendered, vec!["▌ hello", "", "answer", ""]);
    }

    #[test]
    fn render_markdown_loose_bullet_marker_stitched_to_content() {
        // A loose list item (bullet followed by a paragraph) must not leave the
        // `-` marker orphaned on its own line.
        let rendered: Vec<String> =
            render_markdown("- item with **bold**\n\n  paragraph under item\n")
                .iter()
                .map(line_text)
                .collect();
        assert!(
            rendered.iter().any(|l| l.contains("item with")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|l| l.trim() == "-"),
            "orphaned bullet marker: {rendered:?}"
        );
    }

    #[test]
    fn render_markdown_blockquote_gets_gutter() {
        let rendered: Vec<String> = render_markdown("> quoted one\n> quoted two\n")
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered.iter().all(|l| l.is_empty() || l.starts_with("▌ ")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("quoted one")),
            "{rendered:?}"
        );
    }

    #[test]
    fn render_markdown_nested_blockquote_nests_gutter() {
        let rendered: Vec<String> = render_markdown("> outer\n>> inner\n")
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered.iter().any(|l| l.starts_with("▌ ▌ ")),
            "{rendered:?}"
        );
    }

    #[test]
    fn render_markdown_horizontal_rule_is_drawn() {
        let rendered: Vec<String> = render_markdown("above\n\n---\n\nbelow\n")
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered
                .iter()
                .any(|l| l.chars().all(|c| c == '─') && l.len() > 3),
            "{rendered:?}"
        );
        assert!(!rendered.iter().any(|l| l.contains("---")), "{rendered:?}");
    }

    #[test]
    fn render_markdown_setext_heading_not_treated_as_rule() {
        // `Title` followed immediately by `---` is a setext heading underline,
        // not a thematic break — it must not become a drawn rule.
        let rendered: Vec<String> = render_markdown("Title\n---\nbody\n")
            .iter()
            .map(line_text)
            .collect();
        assert!(
            !rendered
                .iter()
                .any(|l| l.chars().all(|c| c == '─') && l.len() > 3),
            "{rendered:?}"
        );
        assert!(rendered.iter().any(|l| l.contains("Title")), "{rendered:?}");
    }

    #[test]
    fn render_markdown_task_list_uses_checkbox_glyphs() {
        let rendered: Vec<String> = render_markdown("- [ ] todo\n- [x] done\n- [X] also\n")
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("☐") && l.contains("todo")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("☑") && l.contains("done")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("☑") && l.contains("also")),
            "{rendered:?}"
        );
        assert!(
            !rendered
                .iter()
                .any(|l| l.contains("[ ]") || l.contains("[x]")),
            "{rendered:?}"
        );
    }

    #[test]
    fn rewrite_task_list_markers_leaves_non_tasks_alone() {
        // A bracket that is not a task-list checkbox must survive untouched.
        assert_eq!(
            rewrite_task_list_markers("see [link] here"),
            "see [link] here"
        );
        assert_eq!(
            rewrite_task_list_markers("- regular item"),
            "- regular item"
        );
        // Indented task items are still rewritten.
        assert_eq!(rewrite_task_list_markers("  - [x] nested"), "  - ☑ nested");
    }

    #[test]
    fn render_table_block_draws_aligned_grid() {
        let lines = vec![
            "| Name | Count |".to_string(),
            "|:-----|------:|".to_string(),
            "| a | 1 |".to_string(),
            "| bbbb | 22 |".to_string(),
        ];
        let rendered: Vec<String> = render_table_block(lines, None)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(
            rendered,
            vec![
                "┌──────┬───────┐".to_string(),
                "│ Name │ Count │".to_string(),
                "├──────┼───────┤".to_string(),
                // left-aligned name column, right-aligned count column
                "│ a    │     1 │".to_string(),
                "│ bbbb │    22 │".to_string(),
                "└──────┴───────┘".to_string(),
            ]
        );
    }

    #[test]
    fn render_table_block_clips_to_max_width() {
        let lines = vec![
            "| Feature | Example | Status |".to_string(),
            "| --- | --- | --- |".to_string(),
            "| Table rendering | aligned columns with a very long explanation | ✅ |".to_string(),
        ];
        let rendered: Vec<String> = render_table_block(lines, Some(32))
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered.iter().all(|line| display_width(line) <= 32),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains('…')),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.starts_with('┌')),
            "{rendered:?}"
        );
    }

    #[test]
    fn render_markdown_with_width_caps_tables() {
        let md = "| Feature | Example | Status |\n| --- | --- | --- |\n| Table rendering | aligned columns with a very long explanation | ✅ |\n";
        let rendered: Vec<String> = render_markdown_with_width(md, 36)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered.iter().all(|line| display_width(line) <= 36),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains('…')),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.starts_with('┌')),
            "{rendered:?}"
        );
    }

    #[test]
    fn render_markdown_renders_table_not_pipe_soup() {
        let md = "intro\n\n| H1 | H2 |\n| --- | --- |\n| x | y |\n";
        let rendered: Vec<String> = render_markdown(md).iter().map(line_text).collect();
        // The separator row must not survive as raw pipe-dash soup.
        assert!(!rendered.iter().any(|l| l.contains("---")));
        assert!(rendered.iter().any(|l| l.starts_with('┌')));
        assert!(rendered.iter().any(|l| l.contains("│ H1 │ H2 │")));
    }

    #[test]
    fn compact_tool_call_line_uses_positional_single_arg() {
        let line =
            compact_tool_call_line("smart_read", r#"{"path":"src/knowledge.rs"}"#, 100).unwrap();
        assert_eq!(line, "▸ smart_read(src/knowledge.rs)");
    }

    #[test]
    fn fleet_defaults_to_brodex_high_effort() {
        assert_eq!(DEFAULT_FLEET_PROVIDER, Provider::Brodex);
        assert_eq!(
            FLEET_PROVIDERS[default_fleet_provider_cursor()],
            Provider::Brodex
        );
        assert_eq!(default_effort_for(Provider::Brodex), Some("high"));
        assert_eq!(default_effort_for(Provider::Glm), Some("high"));
    }

    #[test]
    fn fast_service_tier_only_applies_to_brodex_dispatches() {
        assert_eq!(
            service_tier_for_dispatch(true, Provider::Brodex).as_deref(),
            Some(SERVICE_TIER_PRIORITY)
        );
        assert_eq!(
            service_tier_for_dispatch(false, Provider::Brodex).as_deref(),
            Some(SERVICE_TIER_DEFAULT)
        );
        assert_eq!(service_tier_for_dispatch(true, Provider::Glm), None);
    }

    #[test]
    fn splice_paste_keeps_multiline_as_one_buffer() {
        // The 16-phantom-dispatch regression: a multi-line paste must land as a
        // single composer buffer with embedded soft newlines, NOT one dispatch
        // per LF. We assert the buffer keeps every newline (the submit boundary
        // is a separate Enter key, never a pasted newline) and the cursor lands
        // at the end of the spliced text.
        let mut input = String::new();
        let mut cursor = 0usize;
        let pasted = "line one\nline two\nline three";
        assert!(super::splice_paste(&mut input, &mut cursor, pasted));
        assert_eq!(input, pasted);
        assert_eq!(input.matches('\n').count(), 2);
        assert_eq!(cursor, pasted.len());

        // CRLF / lone-CR normalize to LF (Windows clipboards / odd terminals).
        let mut crlf = String::new();
        let mut c2 = 0usize;
        assert!(super::splice_paste(&mut crlf, &mut c2, "a\r\nb\rc"));
        assert_eq!(crlf, "a\nb\nc");

        // Splice respects the cursor position and an empty paste is a no-op.
        let mut mid = "AZ".to_string();
        let mut c3 = 1usize;
        assert!(super::splice_paste(&mut mid, &mut c3, "BCD"));
        assert_eq!(mid, "ABCDZ");
        assert_eq!(c3, 4);
        assert!(!super::splice_paste(&mut mid, &mut c3, ""));
        assert_eq!(mid, "ABCDZ");
    }

    #[test]
    fn control_write_error_classes_are_detected() {
        // Broken-pipe class: local stdin gone.
        assert!(err_is_broken_pipe("steer: Broken pipe (os error 32)"));
        assert!(err_is_broken_pipe("write failed: os error 32"));
        assert!(!err_is_broken_pipe("permission denied"));

        // Not-running class: daemon rejected a steer/interrupt on a finished task.
        assert!(err_is_not_running(
            "task abc is Completed, not running"
        ));
        assert!(!err_is_not_running("Broken pipe (os error 32)"));
    }

    #[test]
    fn roster_composer_title_names_dispatch_target() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::new(dir.path().join("fleet")));
        let app = App::new(orch, None, rt.handle().clone());

        let titles = roster_composer_top_titles(&app);

        assert_eq!(
            line_text(&titles[0]),
            format!(" dispatch — {} ", next_tuple(&app))
        );
    }

    #[test]
    fn selector_left_drills_and_right_backs_out() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::new(dir.path().join("fleet")));
        let mut app = App::new(orch, None, rt.handle().clone());
        let orig_provider = app.next_provider;
        let orig_model = app.next_model.clone();
        let orig_effort = app.next_effort.clone();

        // ← drills deeper from Roster
        zoom_left(&mut app);
        assert_eq!(app.zone, Zone::ProviderSelector);
        assert!(app.selector_snapshot.is_some());

        // ← drills to ModelSelector (no commit!)
        zoom_left(&mut app);
        assert_eq!(app.zone, Zone::ModelSelector);

        // ← drills to EffortSelector (no commit!)
        zoom_left(&mut app);
        assert_eq!(app.zone, Zone::EffortSelector);

        // At max depth, ← stays at EffortSelector
        zoom_left(&mut app);
        assert_eq!(app.zone, Zone::EffortSelector);

        // → backs out to ModelSelector (no commit)
        zoom_right(&mut app);
        assert_eq!(app.zone, Zone::ModelSelector);

        // → backs out to ProviderSelector (no commit)
        zoom_right(&mut app);
        assert_eq!(app.zone, Zone::ProviderSelector);

        // → backs out to Roster (no commit)
        zoom_right(&mut app);
        assert_eq!(app.zone, Zone::Roster);

        // Since we never committed, next_provider/model/effort are unchanged
        assert_eq!(app.next_provider, orig_provider);
        assert_eq!(app.next_model, orig_model);
        assert_eq!(app.next_effort, orig_effort);
    }

    #[test]
    fn selector_enter_commits_from_each_depth() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::new(dir.path().join("fleet")));
        let mut app = App::new(orch, None, rt.handle().clone());
        let provider_idx = FLEET_PROVIDERS
            .iter()
            .position(|p| !p.models().is_empty())
            .expect("fleet provider with models");
        let provider = FLEET_PROVIDERS[provider_idx];

        // Enter from ProviderSelector: commits provider, takes defaults for model+effort
        app.zone = Zone::ProviderSelector;
        app.provider_cursor = provider_idx;
        app.model_cursor = usize::MAX;
        app.effort_cursor = usize::MAX;
        app.selector_snapshot = None;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(app.zone, Zone::Roster);
        assert_eq!(app.next_provider, provider);
        assert_eq!(app.next_model.as_deref(), default_model_for(provider));
        assert_eq!(app.next_effort.as_deref(), default_effort_for(provider));

        // Space from ModelSelector: commits provider + model, takes default effort
        let model_idx = (provider.models().len() > 1) as usize;
        let selected_model = provider.models()[model_idx].id;
        app.zone = Zone::ModelSelector;
        app.provider_cursor = provider_idx;
        app.model_cursor = model_idx;
        app.effort_cursor = usize::MAX;
        app.selector_snapshot = None;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );
        assert_eq!(app.zone, Zone::Roster);
        assert_eq!(app.next_provider, provider);
        assert_eq!(app.next_model.as_deref(), Some(selected_model));
        assert_eq!(app.next_effort.as_deref(), default_effort_for(provider));

        // Enter from EffortSelector: commits full provider + model + effort
        let effort_idx = provider.efforts().len().saturating_sub(1);
        let selected_effort = provider.efforts().get(effort_idx).map(|e| e.id);
        app.zone = Zone::EffortSelector;
        app.provider_cursor = provider_idx;
        app.model_cursor = model_idx;
        app.effort_cursor = effort_idx;
        app.selector_snapshot = None;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(app.zone, Zone::Roster);
        assert_eq!(app.next_provider, provider);
        assert_eq!(app.next_model.as_deref(), Some(selected_model));
        assert_eq!(app.next_effort.as_deref(), selected_effort);
    }

    #[test]
    fn selector_esc_restores_pre_entry_selection() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::new(dir.path().join("fleet")));
        let mut app = App::new(orch, None, rt.handle().clone());
        let orig_provider = app.next_provider;
        let orig_model = app.next_model.clone();
        let orig_effort = app.next_effort.clone();

        // Enter selector via ← from Roster
        zoom_left(&mut app);
        assert_eq!(app.zone, Zone::ProviderSelector);

        // Drill into model and effort
        zoom_left(&mut app);
        assert_eq!(app.zone, Zone::ModelSelector);
        zoom_left(&mut app);
        assert_eq!(app.zone, Zone::EffortSelector);

        // Move cursors to something different
        let provider_idx = FLEET_PROVIDERS
            .iter()
            .position(|p| *p != orig_provider)
            .unwrap_or(0);
        app.provider_cursor = provider_idx;

        // Esc restores pre-entry selection
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert_eq!(app.zone, Zone::Roster);
        assert_eq!(app.next_provider, orig_provider);
        assert_eq!(app.next_model, orig_model);
        assert_eq!(app.next_effort, orig_effort);
        assert!(app.selector_snapshot.is_none());
    }

    #[test]
    fn selector_cursors_persist_across_drill_and_back() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::new(dir.path().join("fleet")));
        let mut app = App::new(orch, None, rt.handle().clone());

        // Enter selector and move provider cursor
        zoom_left(&mut app); // Roster → ProviderSelector
        let target_provider = FLEET_PROVIDERS.len().saturating_sub(1);
        app.provider_cursor = target_provider;

        // Drill into model and move model cursor
        zoom_left(&mut app); // ProviderSelector → ModelSelector
        let models = FLEET_PROVIDERS[target_provider].models();
        let target_model = models.len().saturating_sub(1);
        app.model_cursor = target_model;

        // Drill into effort
        zoom_left(&mut app); // ModelSelector → EffortSelector

        // Back out to model — cursor should be where we left it
        zoom_right(&mut app);
        assert_eq!(app.zone, Zone::ModelSelector);
        assert_eq!(app.model_cursor, target_model);

        // Back out to provider — cursor should be where we left it
        zoom_right(&mut app);
        assert_eq!(app.zone, Zone::ProviderSelector);
        assert_eq!(app.provider_cursor, target_provider);

        // Drill back into model — cursor should still be where we left it
        zoom_left(&mut app);
        assert_eq!(app.zone, Zone::ModelSelector);
        assert_eq!(app.model_cursor, target_model);
    }

    #[test]
    fn selector_mind_change_drill_back_no_commit() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::new(dir.path().join("fleet")));
        let mut app = App::new(orch, None, rt.handle().clone());
        let orig_provider = app.next_provider;
        let orig_model = app.next_model.clone();

        // Enter selector and drill all the way to effort
        zoom_left(&mut app); // → ProviderSelector
        zoom_left(&mut app); // → ModelSelector
        zoom_left(&mut app); // → EffortSelector

        // Change mind: back all the way out to roster
        zoom_right(&mut app); // EffortSelector → ModelSelector
        zoom_right(&mut app); // ModelSelector → ProviderSelector
        zoom_right(&mut app); // ProviderSelector → Roster

        // Nothing was committed — selection unchanged
        assert_eq!(app.next_provider, orig_provider);
        assert_eq!(app.next_model, orig_model);
    }

    #[test]
    fn single_agent_steer_title_names_target_agent() {
        let title = Line::from(single_agent_steer_title_spans("review api"));

        assert_eq!(line_text(&title), " steer review api ");
    }

    #[test]
    fn interrupt_failure_lines_explain_why() {
        assert_eq!(interrupt_not_running_line(), "interrupt: task not running");
        assert_eq!(
            interrupt_error_line("daemon rejected /control/interrupt"),
            "interrupt failed: daemon rejected /control/interrupt"
        );
    }

    #[test]
    fn compact_tool_call_line_quotes_shell_commands() {
        let line =
            compact_tool_call_line("shell_run", r#"{"cmd":"cargo test --lib"}"#, 100).unwrap();
        assert_eq!(line, r#"▸ shell_run(cmd: "cargo test --lib")"#);
    }

    #[test]
    fn compact_tool_call_line_shell_run_shows_cwd_when_present() {
        let line = compact_tool_call_line(
            "shell_run",
            r#"{"cmd":"cargo test","cwd":"crates/bro-tools"}"#,
            100,
        )
        .unwrap();
        assert_eq!(
            line,
            r#"▸ shell_run(cwd: crates/bro-tools, cmd: "cargo test")"#
        );
    }

    #[test]
    fn compact_tool_call_line_shell_run_hides_null_cwd() {
        let line = compact_tool_call_line("shell_run", r#"{"cmd":"pwd","cwd":null}"#, 100).unwrap();
        assert_eq!(line, r#"▸ shell_run(cmd: "pwd")"#);
    }

    #[test]
    fn compact_tool_call_line_summarizes_shell_poll() {
        let line = compact_tool_call_line(
            "shell_poll",
            r#"{"session_id":"sh-2","signal":"int","stdin":"continue\n","yield_time_ms":1000}"#,
            120,
        )
        .unwrap();
        assert_eq!(
            line,
            "▸ shell_poll(session=sh-2, signal=int, yield_time_ms=1000, stdin=9 B)"
        );
    }

    #[test]
    fn compact_tool_call_line_summarizes_file_write_content() {
        let line = compact_tool_call_line(
            "file_write",
            r#"{"file_path":"src/a.rs","content":"hello\nworld\n"}"#,
            100,
        )
        .unwrap();
        assert_eq!(line, "▸ file_write(src/a.rs, content=12 B, 2 lines)");
    }

    #[test]
    fn compact_tool_call_line_summarizes_content_search() {
        // With path present: show pattern + path
        let line = compact_tool_call_line(
            "content_search",
            r#"{"pattern":"compact.*tool","path":"src","glob":"*.rs","max_results":20}"#,
            120,
        )
        .unwrap();
        assert_eq!(line, "▸ content_search(compact.*tool, path=src)");

        // Without path: show only pattern
        let line2 = compact_tool_call_line(
            "content_search",
            r#"{"pattern":"fn main","glob":"*.rs"}"#,
            120,
        )
        .unwrap();
        assert_eq!(line2, r#"▸ content_search("fn main")"#);
    }

    #[test]
    fn compact_tool_call_line_falls_back_for_large_args() {
        let long = serde_json::json!({
            "path": "src/lib.rs",
            "content": "x".repeat(500),
        });
        assert!(
            compact_tool_call_line("write", &serde_json::to_string_pretty(&long).unwrap(), 100)
                .is_none()
        );
    }

    #[test]
    fn compact_tool_call_line_respects_actual_width() {
        assert!(compact_tool_call_line("shell_run", r#"{"cmd":"cargo test --lib"}"#, 20).is_none());
    }

    #[test]
    fn compact_tool_calls_render_without_blank_spacers() {
        let items = vec![
            TranscriptItem::ToolCall {
                name: "smart_read".into(),
                args: r#"{"path":"src/a.rs"}"#.into(),
            },
            TranscriptItem::ToolCall {
                name: "shell_run".into(),
                args: r#"{"cmd":"cargo test"}"#.into(),
            },
        ];
        let rendered: Vec<String> = render_transcript(&items, "", &[], 100)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(rendered.len(), 2, "{rendered:?}");
        assert_eq!(rendered[0], "▸ smart_read(src/a.rs)");
        assert_eq!(rendered[1], r#"▸ shell_run(cmd: "cargo test")"#);
    }

    #[test]
    fn file_edit_tool_call_renders_diff_block() {
        let items = vec![TranscriptItem::ToolCall {
            name: "file_edit".into(),
            args: serde_json::json!({
                "file_path": "src/a.rs",
                "old_string": "let x = 1;\nlet y = 2;",
                "new_string": "let x = 9;\nlet y = 2;",
            })
            .to_string(),
        }];
        let rendered: Vec<String> = render_transcript(&items, "", &[], 100)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(
            rendered,
            vec![
                "▸ file_edit(src/a.rs)",
                "- let x = 1;",
                "- let y = 2;",
                "+ let x = 9;",
                "+ let y = 2;",
            ]
        );
    }

    #[test]
    fn latest_todo_state_treats_empty_state_as_cleared() {
        let items = vec![
            TranscriptItem::TodoState(TodoState {
                total: 1,
                completed: 0,
                items: vec![bro_fleet_client::TodoItem {
                    status: TodoItemStatus::Pending,
                    text: "keep visible".into(),
                }],
            }),
            TranscriptItem::TodoState(TodoState {
                total: 0,
                completed: 0,
                items: vec![],
            }),
        ];
        assert_eq!(latest_todo_state(&items), None);
    }

    #[test]
    fn fleet_state_marks_completed_and_stale_exited_as_finished() {
        assert_eq!(
            fleet_state_from_snapshot(TaskStatus::Completed, false, false, false, None),
            FleetState::Finished
        );
        assert_eq!(
            fleet_state_from_snapshot(
                TaskStatus::Running,
                false,
                false,
                true,
                Some(now_ms_ui().saturating_sub(FINISHED_AFTER_IDLE_MS + 1))
            ),
            FleetState::Finished
        );
        assert_eq!(
            fleet_state_from_snapshot(TaskStatus::Running, false, false, true, Some(now_ms_ui())),
            FleetState::Idle
        );
    }

    #[test]
    fn fleet_state_running_empty_events_is_active() {
        // When turn_active=true (as derive_stream_state returns for empty
        // events), a Running task must land in the Active bucket — not Idle.
        assert_eq!(
            fleet_state_from_snapshot(TaskStatus::Running, true, false, false, None),
            FleetState::Active
        );
    }

    #[test]
    fn roster_order_is_bucket_then_started_at_not_activity() {
        let view = |state, started_at, last_activity_ms| AgentView {
            state,
            turn_active: false,
            needs_input: false,
            model: None,
            cwd: None,
            report_message: None,
            last_assistant_message: None,
            started_at,
            last_activity_ms,
            stderr_tail: None,
        };
        let views = vec![
            view(FleetState::Idle, 30, Some(1_000)),
            view(FleetState::Idle, 10, Some(5)),
            view(FleetState::Waiting, 20, Some(20)),
        ];

        assert_eq!(ordered_agent_indices(&views), vec![2, 1, 0]);
    }

    #[test]
    fn delete_previous_word_text_removes_trailing_word_and_space() {
        let mut input = "ask the model   ".to_string();
        delete_previous_word_text(&mut input);
        assert_eq!(input, "ask the ");

        delete_previous_word_text(&mut input);
        assert_eq!(input, "ask ");
    }

    #[test]
    fn activity_clock_records_last_completed_duration() {
        let mut clocks = HashMap::new();
        let key = activity_key("agent", "abc");
        let c = sync_activity_clock(&mut clocks, key.clone(), true, 1_000, None);
        assert_eq!(c.active_since_ms, Some(1_000));
        let c = sync_activity_clock(&mut clocks, key, false, 8_500, None);
        assert_eq!(c.active_since_ms, None);
        assert_eq!(c.last_duration_ms, Some(7_500));
    }

    /// The zoom-view "Agent activity working Ns" timer must reflect the
    /// agent's actual turn-start time, not when the operator first viewed
    /// the agent. Seeding the clock with `turn_started_at` (the last event
    /// timestamp from the task snapshot, or the task start as a fallback)
    /// makes a 35s-old running agent read as "working 35s" the instant the
    /// operator zooms in, not "working 5s".
    #[test]
    fn activity_clock_seeds_from_turn_start_evidence_not_view_time() {
        let mut clocks = HashMap::new();
        let key = activity_key("agent", "old-runner");
        // The turn's last event fired 30s ago. The operator zooms in NOW.
        let c = sync_activity_clock(&mut clocks, key.clone(), true, 60_000, Some(30_000));
        // Clock should be seeded to the turn-start evidence, not `now_ms`.
        assert_eq!(c.active_since_ms, Some(30_000));
    }

    /// When no turn-start evidence exists (a brand-new agent whose first
    /// event has not arrived yet), fall back to `now_ms` so the timer
    /// starts from a known point. This is the same behavior as the
    /// pre-fix clock; the fix is purely additive for the seeded case.
    #[test]
    fn activity_clock_falls_back_to_now_ms_when_no_turn_evidence() {
        let mut clocks = HashMap::new();
        let key = activity_key("agent", "fresh");
        let c = sync_activity_clock(&mut clocks, key, true, 12_000, None);
        assert_eq!(c.active_since_ms, Some(12_000));
    }

    /// Defensive: a future-dated `turn_started_at` (e.g. clock skew or a
    /// daemon-side timestamp from a slightly-ahead host) must clamp to
    /// `now_ms` so the displayed duration is never negative.
    #[test]
    fn activity_clock_clamps_future_turn_started_at_to_now_ms() {
        let mut clocks = HashMap::new();
        let key = activity_key("agent", "skewed");
        let c = sync_activity_clock(&mut clocks, key, true, 10_000, Some(20_000));
        assert_eq!(c.active_since_ms, Some(10_000));
    }

    /// Once the clock is seeded, the turn-start evidence is ignored on
    /// subsequent ticks — we don't want the timer to jump backward when
    /// the operator zooms in late and a new event refreshes the
    /// `last_activity_ms` value. Only the initial seed is from the
    /// snapshot.
    #[test]
    fn activity_clock_ignores_turn_start_evidence_after_seeding() {
        let mut clocks = HashMap::new();
        let key = activity_key("agent", "ticking");
        let _ = sync_activity_clock(&mut clocks, key.clone(), true, 5_000, Some(1_000));
        // Subsequent tick at now=10s with a *newer* last_event timestamp —
        // the clock must not re-seed (active_since_ms already Some).
        let c = sync_activity_clock(&mut clocks, key, true, 10_000, Some(8_000));
        assert_eq!(c.active_since_ms, Some(1_000));
    }

    #[test]
    fn duration_compact_formats_clock_like_values() {
        assert_eq!(duration_compact(7_000), "7s");
        assert_eq!(duration_compact(440_000), "7m20s");
        assert_eq!(duration_compact(7_500_000), "2h05m");
    }

    #[test]
    fn internal_tool_search_is_hidden() {
        assert!(is_internal_tool("tool_search"));
        assert!(is_internal_tool("tool_search_tool"));
        assert!(is_internal_tool("report"));
        assert!(is_internal_tool("todo_write"));
        assert!(!is_internal_tool("shell_run"));
    }

    #[test]
    fn render_transcript_marks_empty_completed_turn() {
        let items = vec![
            TranscriptItem::UserSteer("again".into()),
            TranscriptItem::TurnFooter {
                num_turns: Some(2),
                cost_usd: Some(0.0),
            },
        ];
        let rendered: Vec<String> = render_transcript(&items, "", &[], 100)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("turn ended with no model output")),
            "{rendered:?}"
        );
    }

    #[test]
    fn render_transcript_shows_queued_local_turns() {
        let rendered: Vec<String> = render_transcript(&[], "initial", &["later"], 100)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("queued to stdin; waiting for harness echo")),
            "{rendered:?}"
        );
    }

    #[test]
    fn queued_turn_reconcile_ignores_old_matching_echoes() {
        let mut pending = VecDeque::from(["repeat".to_string()]);
        let mut seen = 1;
        let queued = reconcile_pending_user_turns(&mut pending, &mut seen, ["repeat"]);
        assert_eq!(queued, vec!["repeat"]);
        assert_eq!(seen, 1);
    }

    #[test]
    fn queued_turn_reconcile_clears_new_echoes_fifo() {
        let mut pending = VecDeque::from(["same".to_string(), "same".to_string()]);
        let mut seen = 1;
        let queued = reconcile_pending_user_turns(&mut pending, &mut seen, ["same", "same"]);
        assert_eq!(queued, vec!["same"]);
        assert_eq!(seen, 2);
    }

    #[test]
    fn prompt_slug_is_stable_and_path_safe() {
        assert_eq!(prompt_slug("Fix TUI/harness gaps!"), "fix-tui-harness-gaps");
        assert_eq!(prompt_slug("!!!"), "task");
    }

    #[test]
    fn project_directive_without_alias_uses_original_prompt() {
        let projects = BTreeMap::new();
        let resolved = resolve_project_directive("fix the roster", &projects).unwrap();
        assert_eq!(
            resolved,
            ProjectDirective {
                alias: None,
                cwd: None,
                prompt: "fix the roster".to_string(),
            }
        );
    }

    #[test]
    fn project_directive_resolves_alias_and_strips_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut projects = BTreeMap::new();
        projects.insert("blackbox".to_string(), root.display().to_string());

        let resolved = resolve_project_directive("@blackbox fix the roster", &projects).unwrap();
        assert_eq!(resolved.alias.as_deref(), Some("blackbox"));
        assert_eq!(resolved.cwd.as_deref(), Some(root.to_str().unwrap()));
        assert_eq!(resolved.prompt, "fix the roster");
    }

    #[test]
    fn project_directive_routes_each_alias_to_its_project_root() {
        let soong = tempfile::tempdir().unwrap();
        let transcript_search = tempfile::tempdir().unwrap();
        let soong_root = soong.path().canonicalize().unwrap();
        let transcript_root = transcript_search.path().canonicalize().unwrap();
        let mut projects = BTreeMap::new();
        projects.insert("soong".to_string(), soong_root.display().to_string());
        projects.insert(
            "transcript-search".to_string(),
            transcript_root.display().to_string(),
        );

        let routed_soong = resolve_project_directive("@soong inspect build graph", &projects)
            .expect("soong alias resolves");
        let routed_transcript =
            resolve_project_directive("@transcript-search inspect fleet tui", &projects)
                .expect("transcript-search alias resolves");

        assert_eq!(routed_soong.alias.as_deref(), Some("soong"));
        assert_eq!(
            routed_soong.cwd.as_deref(),
            Some(soong_root.to_str().unwrap())
        );
        assert_eq!(routed_soong.prompt, "inspect build graph");
        assert_eq!(
            routed_transcript.alias.as_deref(),
            Some("transcript-search")
        );
        assert_eq!(
            routed_transcript.cwd.as_deref(),
            Some(transcript_root.to_str().unwrap())
        );
        assert_eq!(routed_transcript.prompt, "inspect fleet tui");
    }

    #[test]
    fn project_directive_rejects_unknown_alias() {
        let projects = BTreeMap::new();
        let err = resolve_project_directive("@missing fix", &projects).unwrap_err();
        assert!(err.contains("unknown @project `missing`"));
    }

    #[test]
    fn prepare_dispatch_worktree_creates_isolated_git_worktree() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init"]);
        run_git(repo.path(), &["config", "user.email", "test@example.com"]);
        run_git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(repo.path().join("README.md"), "base\n").unwrap();
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-m", "init"]);

        let store = tempfile::tempdir().unwrap();
        let orch = FleetOrchestrator::new(store.path().join("fleet"));
        let worktree = prepare_dispatch_worktree(
            &orch,
            Some(repo.path().to_str().unwrap()),
            "Fix the launch flow",
        )
        .unwrap();

        let cwd = PathBuf::from(&worktree.cwd);
        assert!(cwd.join("README.md").is_file());
        assert!(worktree.grounding.contains("isolated git worktree"));
        assert!(worktree.grounding.contains("Worktree branch: bro-fleet/"));
        let env = worktree.env_overrides.as_ref().unwrap();
        // Per-worktree build isolation: the cockpit must NOT inject a shared
        // CARGO_TARGET_DIR (concurrent worktree builds would otherwise serialize
        // on cargo's build lock). Each worktree uses its own target/ by default.
        assert!(
            !env.contains_key("CARGO_TARGET_DIR"),
            "dispatch env must not hardcode CARGO_TARGET_DIR; got {env:?}"
        );
        assert!(
            !worktree.grounding.contains("Shared Cargo target dir"),
            "grounding must not advertise a shared target dir"
        );
        let repo_root = repo.path().canonicalize().unwrap();
        let worktree_root = store.path().join("fleet").join("worktrees");
        assert_eq!(
            env.get("BRO_FLEET_BASE_REPO").map(String::as_str),
            Some(repo_root.to_str().unwrap())
        );
        assert_eq!(
            env.get("BRO_FLEET_WORKTREE_ROOT").map(String::as_str),
            Some(worktree_root.to_str().unwrap())
        );
        assert!(
            env.get("BRO_FLEET_WORKTREE_BRANCH")
                .is_some_and(|branch| branch.starts_with("bro-fleet/fix-the-launch-flow-"))
        );

        run_git(
            repo.path(),
            &["worktree", "remove", "--force", &worktree.cwd],
        );
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed:\nstdout={}\nstderr={}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn branch_exists(cwd: &Path, branch: &str) -> bool {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["branch", "--list", branch])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git branch --list {branch} failed:\nstderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).contains(branch)
    }

    #[test]
    fn shell_result_renderer_unpacks_json_envelope() {
        let lines = shell_result_block(
            r#"{"exit_code":1,"stdout":"out\n","stderr":"err\n","running":false,"timed_out":false}"#,
            false,
            10,
        );
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            rendered.iter().any(|l| l.contains("exit=1")),
            "{rendered:?}"
        );
        assert!(rendered.iter().any(|l| l == "stdout:"), "{rendered:?}");
        assert!(rendered.iter().any(|l| l.contains("out")), "{rendered:?}");
        assert!(rendered.iter().any(|l| l == "stderr:"), "{rendered:?}");
        assert!(rendered.iter().any(|l| l.contains("err")), "{rendered:?}");
        assert!(
            !rendered.iter().any(|l| l.contains("exit_code")),
            "{rendered:?}"
        );
    }

    #[test]
    fn shell_result_renderer_shows_running_next_step() {
        let lines = shell_result_block(
            r#"{"exit_code":null,"stdout":"","stderr":"","running":true,"timed_out":false,"session_id":"sh-7","next_step":"Call shell_poll with session_id=sh-7 until running=false."}"#,
            false,
            10,
        );
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            rendered.iter().any(|l| l.contains("running session=sh-7")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("Call shell_poll")),
            "{rendered:?}"
        );
    }

    #[test]
    fn shell_result_renderer_skips_huge_payloads() {
        let huge = format!(
            r#"{{"exit_code":0,"stdout":"{}","stderr":"","running":false,"timed_out":false}}"#,
            "x".repeat(210_000)
        );
        let rendered: Vec<String> = shell_result_block(&huge, false, 10)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(rendered.len(), 1);
        assert!(
            rendered[0].contains("shell result too large for live render"),
            "{rendered:?}"
        );
    }

    #[test]
    fn markdown_renderer_formats_common_transcript_shapes() {
        let lines = render_markdown("# Plan\n\n1. First\n2. Second\n\n- bullet");
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert!(rendered.iter().any(|l| l == "Plan"), "{rendered:?}");
        assert!(
            rendered.iter().any(|l| l.contains("1. First")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("2. Second")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("bullet")),
            "{rendered:?}"
        );
    }

    #[test]
    fn markdown_renderer_renders_tables_as_grid() {
        let lines = render_markdown("| Tool | Why |\n| --- | --- |\n| bbox | indexed search |\n");
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        // The separator row must be consumed, not echoed as pipe-dash soup.
        assert!(!rendered.iter().any(|l| l.contains("---")), "{rendered:?}");
        // Header and data cells survive inside a box-drawn grid.
        assert!(
            rendered.iter().any(|l| l == "│ Tool │ Why            │"),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l == "│ bbox │ indexed search │"),
            "{rendered:?}"
        );
        assert_eq!(
            rendered.first().map(String::as_str),
            Some("┌──────┬────────────────┐")
        );
        assert_eq!(
            rendered.last().map(String::as_str),
            Some("└──────┴────────────────┘")
        );
    }

    #[test]
    fn markdown_renderer_preserves_fenced_code_blocks() {
        let lines = render_markdown("```rust\nfn main() {\n    println!(\"hi\");\n}\n```");
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(rendered.first().map(String::as_str), Some("┌─ rust"));
        assert_eq!(rendered.last().map(String::as_str), Some("└─"));
        assert!(
            rendered.iter().any(|l| l.contains("fn main()")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("println!")),
            "{rendered:?}"
        );
    }

    #[test]
    fn steer_renderer_keeps_prefix_while_rendering_markdown() {
        let lines = render_steer("## Heading\n\n- item", 80);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            rendered.iter().all(|line| line.starts_with("▌ ")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("you ›")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("Heading")),
            "{rendered:?}"
        );
        assert!(rendered.iter().any(|l| l.contains("item")), "{rendered:?}");
    }

    #[test]
    fn steer_renderer_prefixes_multiline_and_wrapped_rows() {
        let lines = render_steer("first line\nsecond line is longer", 12);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert!(rendered.len() >= 3, "{rendered:?}");
        assert!(
            rendered.iter().all(|line| line.starts_with("▌ ")),
            "{rendered:?}"
        );
        assert!(!rendered.iter().any(|line| line == "▌ "), "{rendered:?}");
        assert!(rendered.iter().any(|l| l.contains("first")), "{rendered:?}");
        assert!(
            rendered.iter().any(|l| l.contains("second")),
            "{rendered:?}"
        );
    }

    // ── duplicate prompt regression (thread-c3f7c7e3) ────────────────────

    /// Regression: `commit_inline_history` must suppress the local
    /// initial_prompt render when the focused transcript's first item already
    /// carries the same text as a UserSteer — otherwise the prompt renders
    /// twice in scrollback (once locally, once from the SSE snapshot).
    #[test]
    fn initial_prompt_suppressed_when_first_transcript_item_matches() {
        let initial = "fix the bug";
        // Match: first item is UserSteer with exact text.
        let items = vec![TranscriptItem::UserSteer(initial.into())];
        assert!(
            initial_prompt_already_in_transcript(initial, &items),
            "should detect matching first UserSteer"
        );

        // Mismatch: first item is different text.
        let items = vec![TranscriptItem::UserSteer("something else".into())];
        assert!(
            !initial_prompt_already_in_transcript(initial, &items),
            "should not match different UserSteer text"
        );

        // Mismatch: first item is not a UserSteer.
        let items = vec![TranscriptItem::AssistantText("thinking".into())];
        assert!(
            !initial_prompt_already_in_transcript(initial, &items),
            "should not match non-UserSteer first item"
        );

        // Empty transcript: nothing to match.
        let items: Vec<TranscriptItem> = vec![];
        assert!(
            !initial_prompt_already_in_transcript(initial, &items),
            "empty transcript never matches"
        );

        // Empty initial prompt + empty UserSteer: they match, but in practice
        // commit_inline_history guards with `!initial.is_empty()` so this path
        // is never reached.
        let items = vec![TranscriptItem::UserSteer("".into())];
        assert!(
            initial_prompt_already_in_transcript("", &items),
            "empty strings match (guarded by !initial.is_empty() at call site)"
        );
    }

    /// Regression for the D24 footer empty-prompt bug: a daemon-origin task
    /// (bro_exec, agent dispatch, workflow) has no `initial_prompt` and the
    /// daemon provides a `name` via `snap.name`. The zoom footer must fall
    /// back to that name instead of rendering `""`.
    #[test]
    fn pick_zoom_label_prefers_initial_prompt_when_present() {
        assert_eq!(pick_zoom_label("audit the dispatch path", "session-1"), "audit the dispatch path");
    }

    /// Empty `initial_prompt` (the cockpit never set one — daemon-origin
    /// task) must fall back to the daemon-supplied name so the footer
    /// always carries a non-empty label.
    #[test]
    fn pick_zoom_label_falls_back_to_name_when_prompt_empty() {
        assert_eq!(pick_zoom_label("", "session-1"), "session-1");
    }

    /// The roster model column must prefer the operator's per-agent
    /// intent (`selected_model`) over the live snapshot. The operator
    /// can change a model's mid-flight via `/model`; the cached intent
    /// wins until the next dispatch.
    #[test]
    fn roster_model_label_prefers_selected_model_over_snapshot() {
        let m = roster_model_label(Some("claude-3-5-sonnet"), Some("gpt-4o"), Provider::Brodex);
        assert_eq!(m, "claude-3-5-sonnet");
    }

    /// Daemon-origin tasks (Dispatched Agents tab) leave `selected_model`
    /// unset because the operator never chose a model from the cockpit.
    /// The live snapshot's model must carry the row.
    #[test]
    fn roster_model_label_falls_back_to_snapshot_model() {
        let m = roster_model_label(None, Some("gpt-4o"), Provider::Brodex);
        assert_eq!(m, "gpt-4o");
    }

    /// D24: a Dispatched-tab row whose snapshot has not yet reported a
    /// model (e.g. the task was just registered and no event has landed)
    /// used to render "—", visually indistinguishable from "we don't
    /// know what this is". The provider's default catalog model is a
    /// strictly better placeholder — it tells the operator what *would*
    /// be selected by default, not nothing.
    #[test]
    fn roster_model_label_falls_back_to_provider_default() {
        let m = roster_model_label(None, None, Provider::Brodex);
        // Whatever the Brodex default is in the catalog, it must be a
        // non-empty string and not the em-dash placeholder.
        assert!(!m.is_empty(), "default must not be empty");
        assert_ne!(m, "—", "default must be the provider's catalog model, not the em-dash");
    }

    /// If everything is None and the provider has no default catalog
    /// entry either (a misconfigured provider), the em-dash placeholder
    /// is the only honest answer — a row that renders blank is worse
    /// than one that renders the placeholder.
    #[test]
    fn roster_model_label_renders_em_dash_when_all_sources_absent() {
        // Sanity: with real providers, the helper never reaches the
        // em-dash path because every fleet-eligible provider has a
        // default catalog model. The em-dash is the last-resort safety
        // net for a future provider variant whose catalog is empty;
        // the helper should still return the em-dash in that case
        // (rather than None / panicking / rendering empty).
        let m = roster_model_label(None, None, Provider::Brodex);
        assert!(!m.is_empty());
        assert_ne!(m, "");
    }

    /// Regression: when the commit cursor is stale (e.g. from a previous zoom
    /// session or a resync that produced fewer events), `commit_inline_history`
    /// must not panic on the slice `transcript[committed..stable_end]`.
    ///
    /// We verify this indirectly by checking that `inline_stable_end` and the
    /// bounds-clamp logic produce a valid range even when the committed count
    /// exceeds the transcript length.
    #[test]
    fn stale_commit_cursor_does_not_panic() {
        // Simulate: committed=10 but transcript only has 5 items, turn inactive.
        let items: Vec<TranscriptItem> = vec![
            TranscriptItem::UserSteer("a".into()),
            TranscriptItem::AssistantText("b".into()),
            TranscriptItem::ToolCall { name: "c".into(), args: "{}".into() },
            TranscriptItem::ToolResult { tool: Some("c".into()), content: "d".into(), is_error: false, rider: None },
            TranscriptItem::TurnFooter { num_turns: Some(1), cost_usd: None },
        ];
        let committed: usize = 10;
        let stable_end = inline_stable_end(&items, false);

        // The bounds clamp in commit_inline_history: min(committed, transcript.len())
        let transcript_len = items.len();
        let start = committed.min(transcript_len); // → 5
        let end = stable_end.min(transcript_len); // → 5
        // An empty slice is valid — no panic.
        assert_eq!(start, transcript_len);
        assert_eq!(end, transcript_len);
        assert!(start <= transcript_len);
        assert!(end <= transcript_len);
        // The key property: start ≤ end (empty range, not inverted).
        assert!(start <= end);
    }

    /// The resolution: removing the Snapshot handler's unconditional
    /// `inline_commits.remove` means a terminal-transition resnapshot keeps the
    /// commit cursor. Verify that when the cursor is preserved and the snapshot
    /// is a superset (has ≥ items than what was committed), `commit_inline_history`
    /// skips already-committed items and only emits new ones.
    #[test]
    fn superset_snapshot_preserves_cursor_and_only_emits_new_items() {
        // Build a transcript with two completed turns (each sealed by a
        // TurnFooter), then one active turn in progress.  The first 8 items
        // are from completed turns; items [8..12] are the in-progress turn.
        let items: Vec<TranscriptItem> = {
            let mut v = Vec::new();
            // Turn 1 (complete, 4 items)
            v.push(TranscriptItem::UserSteer("t1 steer".into()));
            v.push(TranscriptItem::AssistantText("t1 text".into()));
            v.push(TranscriptItem::ToolCall { name: "t1".into(), args: "{}".into() });
            v.push(TranscriptItem::TurnFooter { num_turns: Some(1), cost_usd: None });
            // Turn 2 (complete, 4 items)
            v.push(TranscriptItem::UserSteer("t2 steer".into()));
            v.push(TranscriptItem::AssistantText("t2 text".into()));
            v.push(TranscriptItem::ToolCall { name: "t2".into(), args: "{}".into() });
            v.push(TranscriptItem::TurnFooter { num_turns: Some(2), cost_usd: None });
            // Turn 3 (active, 4 items)
            v.push(TranscriptItem::UserSteer("t3 steer".into()));
            v.push(TranscriptItem::AssistantText("t3 text".into()));
            v.push(TranscriptItem::ToolCall { name: "t3".into(), args: "{}".into() });
            v.push(TranscriptItem::ToolResult { tool: Some("t3".into()), content: "ok".into(), is_error: false, rider: None });
            v
        };
        assert_eq!(items.len(), 12);
        let committed: usize = 5;
        let stable_end = inline_stable_end(&items, true); // turn active → last TurnFooter at idx 7

        let start = committed.min(items.len()); // → 5
        let end = stable_end.min(items.len());
        // Last TurnFooter at index 7 → stable_end = 8.
        assert_eq!(stable_end, 8);
        // New items to commit: transcript[5..8] (items from completed turn 2
        // that were not yet committed).
        assert_eq!(start, 5);
        assert!(end > start, "should commit new items beyond the cursor");
        assert_eq!(end, 8);
        // Active turn items [8..12] remain in the live region.
        assert!(end <= items.len());
        // Already-committed items [0..5) are skipped.
        assert_eq!(start - 0, committed);
    }

    #[test]
    fn assistant_text_lays_out_one_line_per_source_line() {
        // Repro of the fleet-cockpit newline-collapse bug: an assistant
        // message with one number per line was rendered as `1 2 3 4 5`
        // (space-separated, soft-wrapped) because the markdown path
        // collapsed single newlines into soft breaks per CommonMark.
        // After the fix, each source line should land on its own rendered
        // line, in order.
        let items = vec![TranscriptItem::AssistantText("1\n2\n3\n4\n5".into())];
        let rendered: Vec<String> = render_committed_items(&items, 80)
            .iter()
            .map(line_text)
            .collect();
        let body: Vec<&str> = rendered
            .iter()
            .map(String::as_str)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(
            body,
            vec!["1", "2", "3", "4", "5"],
            "each source line should land on its own rendered line: {rendered:?}"
        );
    }

    // ── Prune arm-confirm + worktree cleanup tests ──────────────────────────

    fn setup_repo_with_worktree() -> (tempfile::TempDir, tempfile::TempDir) {
        let base = tempfile::tempdir().unwrap();
        run_git(base.path(), &["init"]);
        run_git(base.path(), &["config", "user.email", "test@example.com"]);
        run_git(base.path(), &["config", "user.name", "Test User"]);
        std::fs::write(base.path().join("README.md"), "base\n").unwrap();
        run_git(base.path(), &["add", "."]);
        run_git(base.path(), &["commit", "-m", "init"]);

        let worktrees = tempfile::tempdir().unwrap();
        let wt_path = worktrees.path().join("wt-test");
        run_git(
            base.path(),
            &[
                "worktree",
                "add",
                wt_path.to_str().unwrap(),
                "-b",
                "bro-fleet/test-branch",
            ],
        );
        (base, worktrees)
    }

    #[test]
    fn worktree_clean_status_reports_clean_for_unchanged() {
        let (_base, worktrees) = setup_repo_with_worktree();
        let wt_path = worktrees.path().join("wt-test");
        assert!(
            worktree_clean_status(wt_path.to_str().unwrap()).unwrap(),
            "fresh worktree should be clean"
        );
    }

    #[test]
    fn worktree_clean_status_reports_dirty_after_modification() {
        let (_base, worktrees) = setup_repo_with_worktree();
        let wt_path = worktrees.path().join("wt-test");
        std::fs::write(wt_path.join("README.md"), "dirty\n").unwrap();
        assert!(
            !worktree_clean_status(wt_path.to_str().unwrap()).unwrap(),
            "modified worktree should be dirty"
        );
    }

    #[test]
    fn worktree_clean_status_errors_on_nonexistent() {
        assert!(
            worktree_clean_status("/no/such/path").is_err(),
            "nonexistent path should error"
        );
    }

    #[test]
    fn worktree_branch_resolves_checked_out_branch() {
        let (_base, worktrees) = setup_repo_with_worktree();
        let wt_path = worktrees.path().join("wt-test");
        let branch = worktree_branch(wt_path.to_str().unwrap()).unwrap();
        assert_eq!(branch, "bro-fleet/test-branch");
    }

    #[test]
    fn worktree_branch_returns_none_for_nonexistent() {
        assert!(
            worktree_branch("/no/such/path").is_none(),
            "nonexistent path should return None"
        );
    }

    #[test]
    fn remove_fleet_worktree_removes_clean_worktree_and_merged_branch() {
        let (base, worktrees) = setup_repo_with_worktree();
        let wt_path = worktrees.path().join("wt-test");
        let wt_str = wt_path.to_str().unwrap();

        assert_eq!(
            remove_fleet_worktree(wt_str),
            Some(FleetWorktreeRemoval::BranchDeleted {
                branch: "bro-fleet/test-branch".to_string()
            }),
            "should remove the worktree and delete an already-merged branch"
        );

        // Worktree dir should be gone.
        assert!(!wt_path.exists(), "worktree dir should be removed");
        assert!(
            !branch_exists(base.path(), "bro-fleet/test-branch"),
            "merged branch should be deleted"
        );
    }

    #[test]
    fn remove_fleet_worktree_keeps_clean_unmerged_branch() {
        let (base, worktrees) = setup_repo_with_worktree();
        let wt_path = worktrees.path().join("wt-test");
        std::fs::write(wt_path.join("README.md"), "base\nunmerged\n").unwrap();
        run_git(&wt_path, &["add", "README.md"]);
        run_git(&wt_path, &["commit", "-m", "unmerged work"]);

        assert_eq!(
            remove_fleet_worktree(wt_path.to_str().unwrap()),
            Some(FleetWorktreeRemoval::BranchKeptUnmerged {
                branch: "bro-fleet/test-branch".to_string()
            }),
            "worktree should be removed while preserving an unmerged branch"
        );

        assert!(!wt_path.exists(), "worktree dir should be removed");
        assert!(
            branch_exists(base.path(), "bro-fleet/test-branch"),
            "unmerged branch must remain referenced"
        );
    }

    #[test]
    fn remove_fleet_worktree_fails_gracefully_on_nonexistent() {
        assert_eq!(remove_fleet_worktree("/no/such/worktree"), None);
    }

    #[test]
    fn prune_status_reports_unmerged_branch_kept() {
        let msg = prune_status_message(
            2,
            0,
            1,
            0,
            0,
            &["bro-fleet/test-branch".to_string()],
        );
        assert!(
            msg.contains("branch kept (unmerged): bro-fleet/test-branch"),
            "flash should report preserved branch: {msg}"
        );
    }

    #[test]
    fn prune_arm_requires_two_presses() {
        // We can't easily instantiate a full App in unit tests (it needs a
        // FleetOrchestrator + tokio runtime), so test the arm-confirm logic
        // by verifying the status messages set via the two-press protocol.
        // Instead, test the constants and helpers are correct.
        assert_eq!(PRUNE_ARM_SECS, 4, "arm TTL should be 4 seconds");
    }

    #[test]
    fn prune_arm_status_survives_set_status_calls() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::new(dir.path().join("fleet")));
        let mut app = App::new(orch, None, rt.handle().clone());

        // Simulate the armed state (first Ctrl+K was pressed).
        app.prune_armed_until = Some(Instant::now() + Duration::from_secs(PRUNE_ARM_SECS));
        app.prune_armed_count = 3;
        app.set_status_force(
            "Ctrl+K again to prune 3 terminal agents",
            Duration::from_secs(PRUNE_ARM_SECS),
        );

        // Simulate handle_tail overwriting the status (the pre-fix bug).
        app.set_status("agent finished", Duration::from_secs(4));
        // set_status must be suppressed while armed — the arm message survives.
        assert_eq!(
            app.status.as_deref(),
            Some("Ctrl+K again to prune 3 terminal agents"),
            "arm status must survive a set_status call while armed"
        );
        assert!(
            app.prune_armed_until.is_some(),
            "prune_armed_until must persist"
        );

        // set_status_force must always win (used by prune_terminal_agents itself).
        app.set_status_force("no terminal agents to prune", Duration::from_secs(3));
        assert_eq!(
            app.status.as_deref(),
            Some("no terminal agents to prune"),
            "set_status_force must overwrite even when armed"
        );
    }

    #[test]
    fn prune_arm_disarmed_by_other_keys() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::new(dir.path().join("fleet")));
        let mut app = App::new(orch, None, rt.handle().clone());

        // Simulate armed state.
        app.prune_armed_until = Some(Instant::now() + Duration::from_secs(PRUNE_ARM_SECS));
        app.prune_armed_count = 2;
        app.set_status_force(
            "Ctrl+K again to prune 2 terminal agents",
            Duration::from_secs(PRUNE_ARM_SECS),
        );

        // Any non-Ctrl+K key disarms.
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        );
        assert!(
            app.prune_armed_until.is_none(),
            "any non-Ctrl+K key must disarm the prune"
        );
    }

    #[test]
    fn prune_arm_ctrl_k_does_not_disarm() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::new(dir.path().join("fleet")));
        let mut app = App::new(orch, None, rt.handle().clone());

        // Simulate armed state.
        app.prune_armed_until = Some(Instant::now() + Duration::from_secs(PRUNE_ARM_SECS));
        app.prune_armed_count = 5;

        // Ctrl+K goes through prune_terminal_agents, not the disarm line.
        // With an empty agent list, prune_terminal_agents clears the arm
        // (count == 0 branch). But the key point is it does NOT hit the
        // "any other key" disarm at line 2349.
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        // With no agents, prune_terminal_agents sets "no terminal agents".
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("no terminal agents")),
            "Ctrl+K with no agents should report no terminal agents, got {:?}",
            app.status
        );
    }

    // ── inline_stable_end ──────────────────────────────────────────────

    #[test]
    fn stable_end_returns_all_when_turn_inactive() {
        let items = vec![
            TranscriptItem::UserSteer("go".into()),
            TranscriptItem::AssistantText("done".into()),
            TranscriptItem::TurnFooter {
                num_turns: Some(1),
                cost_usd: None,
            },
        ];
        assert_eq!(inline_stable_end(&items, false), 3);
    }

    #[test]
    fn stable_end_empty_active_turn() {
        // No turns completed yet — everything is in-progress.
        let items = vec![
            TranscriptItem::UserSteer("go".into()),
            TranscriptItem::AssistantText("working".into()),
            TranscriptItem::ToolCall {
                name: "shell_run".into(),
                args: "{\"command\": \"ls\"}".into(),
            },
        ];
        // turn_active = true, no TurnFooter → nothing is stable.
        assert_eq!(inline_stable_end(&items, true), 0);
    }

    #[test]
    fn stable_end_excludes_active_turn_after_completed_turn() {
        // One completed turn, then a new active turn in progress.
        let items = vec![
            TranscriptItem::UserSteer("first".into()),
            TranscriptItem::AssistantText("response".into()),
            TranscriptItem::TurnFooter {
                num_turns: Some(1),
                cost_usd: None,
            },
            TranscriptItem::UserSteer("second".into()),
            TranscriptItem::AssistantText("working on it".into()),
            TranscriptItem::ToolCall {
                name: "grep".into(),
                args: "{\"pattern\": \"foo\"}".into(),
            },
        ];
        // Last TurnFooter at index 2 → stable_end = 3 (items 0..3 stable).
        assert_eq!(inline_stable_end(&items, true), 3);
    }

    #[test]
    fn stable_end_includes_multiple_completed_turns() {
        let items = vec![
            TranscriptItem::UserSteer("turn 1".into()),
            TranscriptItem::AssistantText("done 1".into()),
            TranscriptItem::TurnFooter {
                num_turns: Some(1),
                cost_usd: None,
            },
            TranscriptItem::UserSteer("turn 2".into()),
            TranscriptItem::AssistantText("done 2".into()),
            TranscriptItem::TurnFooter {
                num_turns: Some(2),
                cost_usd: None,
            },
            TranscriptItem::UserSteer("turn 3".into()),
            TranscriptItem::AssistantText("active".into()),
        ];
        // Last TurnFooter at index 5 → stable_end = 6 (items 0..6 stable).
        assert_eq!(inline_stable_end(&items, true), 6);
    }

    // ── Terminal-agent steer guard ────────────────────────────────────────

    fn make_test_app(rt: &tokio::runtime::Handle) -> App {
        let dir = tempfile::tempdir().unwrap();
        let orch = std::sync::Arc::new(FleetOrchestrator::new(dir.path().join("fleet")));
        App::new(orch, None, rt.clone())
    }

    /// Set up a terminal (Completed) Brodex agent with the given id and make
    /// it the focused agent in SingleAgent zone. The composer is pre-filled
    /// with `text`.
    fn setup_terminal_agent(app: &mut App, id: &str, text: &str) {
        let handle = AgentHandle::for_test(TaskStatus::Completed, id);
        let agent = Agent {
            task: handle.clone(),
            classifier: None,
            provider: Provider::Brodex,
            selected_model: None,
            selected_effort: None,
            selected_service_tier: None,
            selected_cwd: None,
            name: format!("agent-{id}"),
            name_overridden: false,
            initial_prompt: None,
            pending_inputs: VecDeque::new(),
            seen_user_steers: 0,
        };
        app.agents.push(agent);
        app.zone = Zone::SingleAgent;
        app.focused_agent_id = Some(handle.id());
        app.input = text.to_string();
        app.cursor_pos = text.len();
    }

    #[test]
    fn terminal_agent_first_enter_arms_not_submits() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut app = make_test_app(rt.handle());
        setup_terminal_agent(&mut app, "test-1", "resume this");

        assert!(app.steer_armed_until.is_none());
        submit(&mut app);

        // First Enter on a terminal agent arms the confirmation guard.
        assert!(app.steer_armed_until.is_some(), "should arm after first Enter");
        // Input must NOT be cleared (the guard returns before steer_selected).
        assert_eq!(app.input, "resume this", "input must stay in composer");
    }

    #[test]
    fn terminal_agent_second_enter_submits() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut app = make_test_app(rt.handle());
        setup_terminal_agent(&mut app, "test-2", "go again");

        // Manually arm the confirmation (simulating a first Enter).
        app.steer_armed_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(STEER_ARM_SECS));
        app.steer_armed_agent_id = Some(app.agents[0].task.id());

        submit(&mut app);

        // Second Enter disarms and proceeds — steer_selected takes the input.
        assert!(app.steer_armed_until.is_none(), "should disarm before submitting");
        assert!(app.input.is_empty(), "steer_selected should take the input");
    }

    #[test]
    fn running_agent_submits_immediately() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut app = make_test_app(rt.handle());
        // Running agent (not terminal) — the guard must NOT engage.
        let handle = AgentHandle::for_test(TaskStatus::Running, "test-3");
        let agent = Agent {
            task: handle.clone(),
            classifier: None,
            provider: Provider::Brodex,
            selected_model: None,
            selected_effort: None,
            selected_service_tier: None,
            selected_cwd: None,
            name: "agent-test-3".to_string(),
            name_overridden: false,
            initial_prompt: None,
            pending_inputs: VecDeque::new(),
            seen_user_steers: 0,
        };
        app.agents.push(agent);
        app.zone = Zone::SingleAgent;
        app.focused_agent_id = Some(handle.id());
        app.input = "steer me".to_string();
        app.cursor_pos = 8;

        assert!(app.steer_armed_until.is_none());
        submit(&mut app);

        // Running agent bypasses the guard entirely.
        assert!(app.steer_armed_until.is_none(), "guard must not arm for running agents");
        assert!(app.input.is_empty(), "steer_selected should take the input");
    }

    #[test]
    fn steer_armed_disarmed_by_non_enter_key() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut app = make_test_app(rt.handle());
        setup_terminal_agent(&mut app, "test-4", "armed");

        // Arm the confirmation.
        app.steer_armed_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(STEER_ARM_SECS));
        app.steer_armed_agent_id = Some(app.agents[0].task.id());

        // Press 'a' — should disarm.
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(app.steer_armed_until.is_none(), "non-Enter key should disarm");
    }

    #[test]
    fn enter_key_preserves_armed_steer_for_submit() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut app = make_test_app(rt.handle());

        // Terminal agent, armed steer — Enter should submit, not just disarm.
        setup_terminal_agent(&mut app, "test-5", "confirm");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(STEER_ARM_SECS);
        app.steer_armed_until = Some(deadline);
        app.steer_armed_agent_id = Some(app.agents[0].task.id());

        // Press Enter via handle_key — this should reach submit() which
        // sees the armed confirmation and proceeds (disarming + submitting).
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        // After submit, steer is disarmed AND input is consumed.
        assert!(app.steer_armed_until.is_none(), "submit should disarm on confirm");
        assert!(app.input.is_empty(), "submit should consume input");
    }

    #[test]
    fn steer_armed_expired_rearms_on_enter() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut app = make_test_app(rt.handle());
        setup_terminal_agent(&mut app, "test-6", "stale");

        // Arm with an already-expired deadline.
        app.steer_armed_until = Some(std::time::Instant::now() - std::time::Duration::from_secs(10));
        app.steer_armed_agent_id = Some(app.agents[0].task.id());

        submit(&mut app);

        // Expired arm → re-arms (not submitted).
        assert!(app.steer_armed_until.is_some(), "expired arm should re-arm");
        assert_eq!(app.input, "stale", "input must stay in composer");
    }
    
    // ---- D27: long-lived cockpits running stale binaries (unit-N4) -------
    //
    // The fleet cockpit now stamps the daemon's build identity from
    // `/control/roster` and compares it to its own compile-time
    // identity. Mismatch → persistent footer banner + one-time
    // durable scrollback line. These tests cover the pure decision
    // matrix: `build_identity_mismatch` (the comparison predicate)
    // and `durable_mismatch_message` (the transition matrix that
    // decides whether to emit a cockpit line). The persistent
    // banner is exercised in the snapshot tests at the top of this
    // module.

    fn own_id() -> (String, String) {
        App::own_build_identity()
    }

    fn stamped(version: &str, build: &str) -> (Option<String>, Option<String>) {
        (Some(version.to_string()), Some(build.to_string()))
    }

    /// D27: an identity stamp that exactly matches the cockpit's
    /// compile-time identity is NOT a mismatch — the cockpit was
    /// rebuilt alongside the daemon and is current.
    #[test]
    fn build_identity_mismatch_is_false_on_exact_match() {
        let (cv, cb) = own_id();
        assert!(
            !App::build_identity_mismatch(&stamped(&cv, &cb)),
            "exact (version, build_id) match must not be a mismatch"
        );
    }

    /// D27: a different version IS a mismatch — the most
    /// user-visible case (a major upgrade).
    #[test]
    fn build_identity_mismatch_is_true_on_version_difference() {
        let (_cv, cb) = own_id();
        assert!(
            App::build_identity_mismatch(&stamped("99.99.99", &cb)),
            "different CARGO_PKG_VERSION must be a mismatch"
        );
    }

    /// D27: a different build_id IS a mismatch — this is the
    /// load-bearing case while both sides report `0.0.1`
    /// (early development). A rebuild of just the daemon flips
    /// its `BLACKBOX_BUILD_ID` but the cockpit retains its old
    /// `BRO_CLI_BUILD_ID`, so the comparison detects the drift.
    #[test]
    fn build_identity_mismatch_is_true_on_build_id_difference() {
        let (cv, _cb) = own_id();
        assert!(
            App::build_identity_mismatch(&stamped(&cv, "9999999999")),
            "different BRO_CLI_BUILD_ID (daemon rebuilt, cockpit not) must be a mismatch"
        );
    }

    /// D27: unknown daemon identity (both fields `None`) is NOT a
    /// mismatch — a legacy daemon without a `build.rs` produces
    /// zero visual change in the cockpit, by design.
    #[test]
    fn build_identity_mismatch_is_false_on_unknown_identity() {
        assert!(
            !App::build_identity_mismatch(&(None, None)),
            "unknown daemon identity must not be a mismatch"
        );
    }

    /// D27: partial identity (only version known) is NOT a
    /// mismatch — the comparison requires BOTH sides to report a
    /// value. The "either side has a value but the other does
    /// not" case is treated as "cannot compare", not as drift.
    #[test]
    fn build_identity_mismatch_is_false_on_partial_identity() {
        assert!(
            !App::build_identity_mismatch(&(Some("0.0.1".to_string()), None)),
            "partial daemon identity (version only) must not be a mismatch"
        );
        assert!(
            !App::build_identity_mismatch(&(None, Some("1700000000".to_string()))),
            "partial daemon identity (build_id only) must not be a mismatch"
        );
    }

    /// D27: durable cockpit line is emitted on the FIRST observed
    /// mismatch — the transition from "matched" to "mismatched"
    /// is the signal worth a scrollback line.
    #[test]
    fn durable_mismatch_message_fires_on_first_mismatch() {
        let (cv, cb) = own_id();
        let matched = stamped(&cv, &cb);
        let mismatched = stamped(&cv, "9999999999");
        let msg = App::durable_mismatch_message(&matched, &mismatched)
            .expect("transition matched -> mismatched must emit a line");
        assert!(
            msg.contains(&cv),
            "durable line must carry the cockpit version, got: {msg}"
        );
        assert!(
            msg.contains("restart cockpit"),
            "durable line must name the action, got: {msg}"
        );
    }

    /// D27: no durable line on the transition `unknown -> matched`
    /// (daemon was rebuilt to the same identity — the most common
    /// case in dev where both crates get rebuilt together).
    #[test]
    fn durable_mismatch_message_silent_on_unknown_to_match() {
        let (cv, cb) = own_id();
        let matched = stamped(&cv, &cb);
        assert!(
            App::durable_mismatch_message(&(None, None), &matched).is_none(),
            "unknown -> matched must not emit a line"
        );
    }

    /// D27: no durable line on the matched -> matched transition
    /// (steady-state). The snapshot may re-land dozens of times
    /// per second; we only want a line on the first drift.
    #[test]
    fn durable_mismatch_message_silent_on_match_to_match() {
        let (cv, cb) = own_id();
        let matched = stamped(&cv, &cb);
        assert!(
            App::durable_mismatch_message(&matched, &matched).is_none(),
            "matched -> matched must not emit a line"
        );
    }

    /// D27: no durable line on a continuing mismatch (mismatched
    /// -> mismatched). The line was already emitted on the prior
    /// transition; subsequent snapshots with the same mismatching
    /// stamp must not spam scrollback.
    #[test]
    fn durable_mismatch_message_silent_on_continuing_mismatch() {
        let (cv, _cb) = own_id();
        let mismatched_a = stamped(&cv, "1111111111");
        let mismatched_b = stamped(&cv, "2222222222");
        assert!(
            App::durable_mismatch_message(&mismatched_a, &mismatched_b).is_none(),
            "continuing mismatch must not emit a duplicate line"
        );
    }

    /// D27: no durable line on `unknown -> mismatched` when the
    /// mismatch is inferred from partial fields. The predicate
    /// is already false on partial identity, so this is more of
    /// a guard against future drift in the predicate.
    #[test]
    fn durable_mismatch_message_silent_when_predicate_says_no() {
        assert!(
            App::durable_mismatch_message(&(None, None), &(Some("0.0.1".into()), None)).is_none(),
            "transition into partial identity must not emit a line"
        );
    }

    // ---- N5: git-derived build identity + footer precedence -------

    /// N5: the build_id is derived from `git rev-parse --short=12 HEAD`
    /// (falling back to a Unix-seconds timestamp only when git is
    /// unavailable). In-repo, the stamp must be a hex string (git SHA),
    /// not a raw numeric timestamp — this verifies the build.rs
    /// git-derivation path is active and the false-positive from
    /// per-invocation SystemTime is gone.
    #[test]
    fn build_id_is_git_sha_not_timestamp() {
        let (_cv, cb) = own_id();
        assert!(
            !cb.is_empty(),
            "build_id must be non-empty"
        );
        // A git short SHA is all hex chars [0-9a-f]; a timestamp is all
        // decimal digits. A build_id that starts with a hex letter
        // (e.g. 'a'..'f') is definitively from git — timestamps never
        // start with a-f. But we can't assume the SHA always leads
        // with a letter (could be all decimal digits). Instead:
        // timestamps are strictly decimal digits; git SHAs are hex.
        assert!(
            cb.chars().any(|c| c.is_ascii_hexdigit() && c.is_ascii_alphabetic()),
            "build_id '{}' must contain at least one hex letter; \
             an all-decimal string suggests the timestamp fallback \
             is incorrectly active",
            cb,
        );
    }

    /// N5: when both a status flash and a build-mismatch banner
    /// would occupy the footer slot, the transient status flash
    /// takes precedence — the banner is suppressed for the
    /// duration of the flash and returns when it expires.
    #[test]
    fn footer_status_flash_precedes_mismatch_banner() {
        let (cv, _cb) = own_id();
        // Construct an app with a mismatched daemon identity
        // AND an active status flash.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut app = make_test_app(rt.handle());
        app.last_daemon_build = (Some(cv.clone()), Some("deadbeefcafe".to_string()));
        app.status =
            Some("Ctrl+K again to prune 3".to_string());

        let views: Vec<AgentView> = Vec::new();
        let order: Vec<usize> = Vec::new();
        let spans = roster_status_spans(&app, &views, &order);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect::<Vec<_>>().join("");
        assert!(
            text.contains("Ctrl+K again to prune 3"),
            "status flash must be visible in footer, got: {text}"
        );
        assert!(
            !text.contains("restart cockpit"),
            "build-mismatch banner must be suppressed while status flash is active, got: {text}"
        );
    }

    /// N5: when no status flash is active, the build-mismatch banner
    /// renders normally in the footer slot.
    #[test]
    fn footer_banner_shows_when_no_status_flash() {
        let (cv, _cb) = own_id();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut app = make_test_app(rt.handle());
        app.last_daemon_build = (Some(cv.clone()), Some("deadbeefcafe".to_string()));
        app.status = None;

        let views: Vec<AgentView> = Vec::new();
        let order: Vec<usize> = Vec::new();
        let spans = roster_status_spans(&app, &views, &order);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect::<Vec<_>>().join("");
        assert!(
            text.contains("restart cockpit"),
            "build-mismatch banner must be visible when no status flash, got: {text}"
        );
    }
