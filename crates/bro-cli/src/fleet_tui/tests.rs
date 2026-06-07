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
    fn inline_stable_end_holds_back_only_active_last_item() {
        assert_eq!(inline_stable_end(0, true), 0);
        assert_eq!(inline_stable_end(1, true), 0);
        assert_eq!(inline_stable_end(3, true), 2);
        assert_eq!(inline_stable_end(0, false), 0);
        assert_eq!(inline_stable_end(1, false), 1);
        assert_eq!(inline_stable_end(3, false), 3);
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
        let c = sync_activity_clock(&mut clocks, key.clone(), true, 1_000);
        assert_eq!(c.active_since_ms, Some(1_000));
        let c = sync_activity_clock(&mut clocks, key, false, 8_500);
        assert_eq!(c.active_since_ms, None);
        assert_eq!(c.last_duration_ms, Some(7_500));
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
        assert_eq!(
            worktree
                .env_overrides
                .as_ref()
                .and_then(|m| m.get("CARGO_TARGET_DIR"))
                .map(String::as_str),
            Some(
                repo.path()
                    .canonicalize()
                    .unwrap()
                    .join("target")
                    .to_str()
                    .unwrap()
            )
        );
        let env = worktree.env_overrides.as_ref().unwrap();
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
