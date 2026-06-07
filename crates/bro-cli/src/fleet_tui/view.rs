use super::*;

// ── Drawing ──────────────────────────────────────────────────────────────────

pub(super) fn draw(f: &mut Frame, app: &mut App) {
    let single_agent = app.zone == Zone::SingleAgent;
    let config = app.zone == Zone::Config;
    let composer_height = composer_height(app, f.area());
    let constraints = vec![
        Constraint::Min(0),                  // body/transcript
        Constraint::Length(composer_height), // composer
    ];
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    let (views, order) = app.ordered_agents();

    if single_agent {
        draw_single_agent(f, chunks[0], app, &views);
        let top_titles = app
            .rename_target
            .is_none()
            .then(|| single_agent_composer_top_titles(app, &views, &order));
        let bottom_title = Some(Line::from(single_agent_status_spans(app, &views, &order)));
        draw_composer(f, chunks[1], app, top_titles, bottom_title);
        if slash_active(app) {
            draw_slash_menu(f, chunks[1], app);
        }
    } else if config {
        app.transcript_y_range = None;
        app.last_transcript_height = 0;
        draw_config_body(f, chunks[0], app);
        let bottom_title = Some(Line::from(roster_status_spans(app, &views)));
        draw_composer(f, chunks[1], app, None, bottom_title);
    } else {
        app.transcript_y_range = None;
        app.last_transcript_height = 0;
        draw_roster_body(f, chunks[0], app, &views, &order);
        let top_titles = app
            .rename_target
            .is_none()
            .then(|| roster_composer_top_titles(app));
        let bottom_title = Some(Line::from(roster_status_spans(app, &views)));
        draw_composer(f, chunks[1], app, top_titles, bottom_title);
        if slash_active(app) {
            draw_slash_menu(f, chunks[1], app);
        } else if project_active(app) {
            draw_project_menu(f, chunks[1], app);
        }
    }

    if app.help_visible {
        draw_help_overlay(f, app);
    }
}

/// Popup list of slash completions, anchored above the composer.
pub(super) fn draw_slash_menu(f: &mut Frame, composer: Rect, app: &App) {
    let cmds = filtered_slash(app);
    if cmds.is_empty() {
        return;
    }
    let h = (cmds.len() as u16 + 2).min(8);
    let w = 46.min(composer.width);
    let y = composer.y.saturating_sub(h);
    let area = Rect {
        x: composer.x,
        y,
        width: w,
        height: h,
    };
    let sel = app.slash_cursor.min(cmds.len() - 1);
    let items: Vec<ListItem<'static>> = cmds
        .iter()
        .map(|c| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<10}", c.name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(c.desc.to_string(), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(sel));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " /commands — ↑/↓ · Tab completes ",
            Style::default().fg(Color::Yellow),
        ));
    f.render_widget(Clear, area);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, inner, &mut state);
}

/// Popup list of roster @project completions, anchored above the composer.
pub(super) fn draw_project_menu(f: &mut Frame, composer: Rect, app: &App) {
    let projects = filtered_projects(app);
    if projects.is_empty() {
        return;
    }
    let h = (projects.len() as u16 + 2).min(8);
    let w = 64.min(composer.width);
    let y = composer.y.saturating_sub(h);
    let area = Rect {
        x: composer.x,
        y,
        width: w,
        height: h,
    };
    let sel = app.project_cursor.min(projects.len() - 1);
    let items: Vec<ListItem<'static>> = projects
        .iter()
        .map(|(key, path)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("@{}  ", key),
                    Style::default()
                        .fg(Color::LightGreen)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(truncate(path, 42), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(sel));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightGreen))
        .title(Span::styled(
            " @projects — ↑/↓ · Tab completes ",
            Style::default().fg(Color::LightGreen),
        ));
    f.render_widget(Clear, area);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, inner, &mut state);
}

/// Centered popup overlay showing context-aware keyboard shortcuts.
pub(super) fn draw_help_overlay(f: &mut Frame, app: &App) {
    let shortcut_lines: Vec<Line<'static>> = match app.zone {
        Zone::Roster => vec![
            Line::from("  ↑/↓           navigate agents"),
            Line::from("  →             open agent (zoom in)"),
            Line::from("  ←             provider selector"),
            Line::from("  @project Tab  dispatch from project alias"),
            Line::from("  Ctrl+R        rename agent"),
            Line::from("  Ctrl+X        stop / delete agent"),
            Line::from("  Ctrl+Q        quit"),
        ],
        Zone::SingleAgent => vec![
            Line::from("  ←             back to roster"),
            Line::from("  Esc           interrupt running turn"),
            Line::from("  Ctrl+X        stop / delete agent"),
            Line::from("  ↑/↓           recall input history"),
            Line::from("  PgUp/PgDn     scroll transcript"),
            Line::from("  mouse drag    select/copy transcript or composer text"),
            Line::from("  Ctrl+Q        quit"),
        ],
        Zone::Config => vec![
            Line::from("  ↑/↓           navigate config fields"),
            Line::from("  ←/→           change selected option"),
            Line::from("  Space         toggle / advance option"),
            Line::from("  Enter         save and return"),
            Line::from("  Esc           return"),
        ],
        Zone::ProviderSelector => vec![
            Line::from("  ↑/↓           cycle providers"),
            Line::from("  Enter         confirm + home"),
            Line::from("  →             confirm + home"),
            Line::from("  ←             back (model selector)"),
        ],
        Zone::ModelSelector => vec![
            Line::from("  ↑/↓           cycle models"),
            Line::from("  Enter         confirm + home"),
            Line::from("  →             confirm + home"),
            Line::from("  ←             effort selector"),
        ],
        Zone::EffortSelector => vec![
            Line::from("  ↑/↓           cycle efforts"),
            Line::from("  Enter         confirm + home"),
            Line::from("  →             back"),
        ],
    };
    let h = (shortcut_lines.len() as u16 + 2).min(f.area().height);
    let w = 42u16.min(f.area().width);
    let area = centered_rect(w, h, f.area());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " shortcuts — Esc to dismiss ",
            Style::default().fg(Color::Cyan),
        ));
    f.render_widget(Clear, area);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let para = Paragraph::new(shortcut_lines).style(Style::default().fg(Color::White));
    f.render_widget(para, inner);
}

pub(super) fn draw_roster_body(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    views: &[AgentView],
    order: &[usize],
) {
    // The roster is the focus — full width, no transcript here (that lives in
    // the single-agent view, `→`). In sub-selector zones a slim selector panel
    // sits to the left of the roster.
    let sub_zone = matches!(
        app.zone,
        Zone::ProviderSelector | Zone::ModelSelector | Zone::EffortSelector
    );
    if sub_zone {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(PROVIDER_SEL_WIDTH), Constraint::Min(0)])
            .split(area);
        match app.zone {
            Zone::EffortSelector => draw_effort_selector(f, split[0], app),
            Zone::ModelSelector => draw_model_selector(f, split[0], app),
            Zone::ProviderSelector => draw_provider_selector(f, split[0], app),
            _ => unreachable!(),
        }
        draw_roster(f, split[1], app, views, order);
    } else {
        draw_roster(f, area, app, views, order);
    }
}

pub(super) fn draw_config_body(f: &mut Frame, area: Rect, app: &App) {
    let path = FleetConfig::path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "fleet.json path unavailable".to_string());
    let enabled = app
        .config
        .classifier
        .as_ref()
        .is_some_and(ClassifierConfig::enabled_resolved);
    let title_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled(" fleet config", title_style),
            Span::styled("  ", Style::default()),
            Span::styled(path_tail(&path), Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(""),
    ];

    for (idx, field) in ConfigField::ALL.iter().copied().enumerate() {
        let selected = idx == app.config_cursor;
        let marker = if selected { "▶ " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else if !enabled && !matches!(field, ConfigField::ClassifierEnabled) {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{:<20}", field.label()), style),
            Span::styled(" ", Style::default()),
            Span::styled(
                field.value(&app.config),
                if selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::LightYellow)
                },
            ),
        ]));
        if selected {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(field.hint(), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑/↓ fields   ←/→ options   Space toggles   Enter saves + returns   Esc returns",
        Style::default().fg(Color::DarkGray),
    )));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

pub(super) fn draw_roster(f: &mut Frame, area: Rect, app: &mut App, views: &[AgentView], order: &[usize]) {
    // Full-width, borderless — the roster is the focus (the title bar and
    // composer frame it). In provider-selector mode the selector to the left
    // carries its own separator.
    let inner = area;

    if app.agents.is_empty() {
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no agents yet — type a prompt below + Enter to dispatch one",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        f.render_widget(hint, inner);
        return;
    }

    // Fixed-width columns: glyph · provider · name · model · report (flex) ·
    // started · last. `started` = session age; `last` = time since the
    // last stream event.
    let widths = [
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Length(30),
        Constraint::Length(13),
        Constraint::Min(18),
        Constraint::Length(7),
        Constraint::Length(7),
    ];
    let header = Row::new(["", "prov", "agent", "model", "report", "started", "last"]).style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut flat_selected: Option<usize> = None;
    let mut first_bucket = true;

    for bucket in FleetState::BUCKETS {
        let in_bucket: Vec<usize> = order
            .iter()
            .copied()
            .filter(|&i| views[i].state == bucket)
            .collect();
        if in_bucket.is_empty() {
            continue;
        }
        // One blank row between buckets.
        if !first_bucket {
            rows.push(Row::new(Vec::<Cell>::new()));
        }
        first_bucket = false;

        let (_, color) = bucket.glyph();
        let collapsed = app.collapsed.contains(&bucket);
        let caret = if collapsed { "▸" } else { "▾" };
        // Section header — the bucket label sits in the (wide) name column.
        rows.push(Row::new(vec![
            Cell::from(""),
            Cell::from(""),
            Cell::from(format!("{caret} {} ({})", bucket.label(), in_bucket.len()))
                .style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
        ]));

        if collapsed {
            continue;
        }
        for i in in_bucket {
            let sel_idx = order.iter().position(|&o| o == i).unwrap_or(0);
            if sel_idx == app.roster_selected {
                flat_selected = Some(rows.len());
            }
            let v = &views[i];
            let a = &app.agents[i];
            let (glyph, gcolor) = v.state.glyph();
            let model = a
                .selected_model
                .clone()
                .or_else(|| v.model.clone())
                .unwrap_or_else(|| "—".into());
            let report = v
                .report_message
                .as_deref()
                .map(|m| truncate(m, 54))
                .unwrap_or_else(|| "—".into());
            let started = age(v.started_at);
            let last = v
                .last_activity_ms
                .map(age)
                .unwrap_or_else(|| started.clone());
            rows.push(Row::new(vec![
                Cell::from(glyph).style(Style::default().fg(gcolor)),
                Cell::from(provider_tag(a.provider))
                    .style(Style::default().fg(provider_color(a.provider))),
                Cell::from(truncate(&a.name, 30))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Cell::from(truncate(&model, 13)).style(Style::default().fg(Color::Gray)),
                Cell::from(report).style(Style::default().fg(Color::LightYellow)),
                Cell::from(started).style(Style::default().fg(Color::DarkGray)),
                Cell::from(last).style(Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    let mut state = TableState::default();
    state.select(flat_selected);
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .row_highlight_style(Style::default().bg(ROSTER_SELECTED_BG))
        .highlight_symbol(Text::styled(
            ROSTER_SELECTED_MARKER,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(table, inner, &mut state);
}

pub(super) fn draw_provider_selector(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::RIGHT | Borders::TOP)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " provider · model · effort ",
            Style::default().fg(Color::Cyan),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    for (i, p) in FLEET_PROVIDERS.iter().enumerate() {
        let selected = i == app.provider_cursor;
        let marker = if selected { "▶ " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(provider_color(*p))
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(provider_color(*p))
        };
        let model = if *p == app.next_provider {
            app.next_model
                .as_deref()
                .or_else(|| default_model_for(*p))
                .unwrap_or("—")
        } else {
            default_model_for(*p).unwrap_or("—")
        };
        let effort = if *p == app.next_provider {
            app.next_effort
                .as_deref()
                .or_else(|| default_effort_for(*p))
        } else {
            default_effort_for(*p)
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{:<8}", p.as_str()), style),
            Span::styled(truncate(model, 18), Style::default().fg(Color::Gray)),
            Span::styled(" ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                effort.unwrap_or("—").to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn draw_model_selector(f: &mut Frame, area: Rect, app: &App) {
    let provider = FLEET_PROVIDERS[app.provider_cursor];
    let models = provider.models();
    let title = format!(" model · {} ", provider.as_str());
    let block = Block::default()
        .borders(Borders::RIGHT | Borders::TOP)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(title, Style::default().fg(Color::Cyan)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    for (i, m) in models.iter().enumerate() {
        let selected = i == app.model_cursor;
        let marker = if selected { "▶ " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(Color::Gray)
        };
        let default_marker = if m.default { " ★" } else { "" };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(truncate(m.id, 24), style),
            Span::styled(default_marker, Style::default().fg(Color::Yellow)),
        ]));
        if selected {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    truncate(m.description, PROVIDER_SEL_WIDTH as usize - 6),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    // Hint line
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter: confirm  ←: effort  →: back",
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn draw_effort_selector(f: &mut Frame, area: Rect, app: &App) {
    let provider = FLEET_PROVIDERS[app.provider_cursor];
    let models = provider.models();
    let model_id = models.get(app.model_cursor).map(|m| m.id).unwrap_or("—");
    let efforts = provider.efforts();
    let title = format!(" effort · {} · {} ", provider.as_str(), model_id);
    let block = Block::default()
        .borders(Borders::RIGHT | Borders::TOP)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(title, Style::default().fg(Color::Cyan)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    if efforts.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no effort levels for this provider",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (i, e) in efforts.iter().enumerate() {
            let selected = i == app.effort_cursor;
            let marker = if selected { "▶ " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default().fg(Color::Gray)
            };
            let default_marker = if e.default { " ★" } else { "" };
            lines.push(Line::from(vec![
                Span::raw(marker),
                Span::styled(format!("{:<10}", e.id), style),
                Span::styled(default_marker, Style::default().fg(Color::Yellow)),
            ]));
            if selected {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        truncate(e.description, PROVIDER_SEL_WIDTH as usize - 6),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }
    }
    // Hint line
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter: confirm  →: back",
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn draw_single_agent(f: &mut Frame, area: Rect, app: &mut App, views: &[AgentView]) {
    let Some(idx) = app.selected_agent() else {
        app.transcript_y_range = Some((area.y, area.y.saturating_add(area.height)));
        app.last_transcript_height = area.height;
        if app.mode.is_standalone() {
            let target = app
                .mode
                .pending_resume()
                .map(|id| format!("resume session {id}"))
                .unwrap_or_else(|| "start a fresh session".to_string());
            let provider = next_tuple(app);
            let lines = vec![
                Line::from(Span::styled(
                    "bro agent",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(format!("Type a prompt and press Enter to {target}.")),
                Line::from(format!("Next: {provider}")),
                Line::from(""),
                Line::from(Span::styled(
                    "Slash commands: /config, /model, /effort, /resume <session_id> [turn], /clear",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
            return;
        }
        app.transcript_y_range = None;
        app.last_transcript_height = 0;
        app.focused_agent_id = None;
        app.zone = Zone::Roster;
        return;
    };
    let v = &views[idx];
    let transcript = app.agents[idx].task.transcript();
    let latest_todo = latest_todo_state(&transcript);

    let mut transcript_area = area;
    if let Some(todo) = latest_todo
        .as_ref()
        .filter(|todo| !todo.items.is_empty() && area.height >= 8)
    {
        let todo_h = todo_panel_height(todo, area.height);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(todo_h)])
            .split(area);
        transcript_area = chunks[0];
        draw_todo_panel(f, chunks[1], todo);
    }

    // The single-agent transcript is intentionally bare: no border and no
    // header/title line. Identity and status live on the composer chrome so the
    // transcript keeps every available row.
    let width = transcript_area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(err) = &v.stderr_tail {
        lines.push(Line::from(Span::styled(
            format!("✗ {}", truncate(err, 100)),
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::from(""));
    }
    let initial = initial_prompt(&app.agents[idx]).to_string();
    let queued = queued_user_turns(&mut app.agents[idx], &transcript);
    let queued: Vec<&str> = queued.iter().map(String::as_str).collect();
    lines.extend(render_transcript(&transcript, &initial, &queued, width));

    let para = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
    let total = para.line_count(transcript_area.width.max(1));
    if app.scroll_from_bottom > 0 && total > app.cached_total_lines {
        let delta = total - app.cached_total_lines;
        app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(delta);
    }
    app.cached_total_lines = total;
    let body_h = transcript_area.height.max(1) as usize;
    let max_scroll = total.saturating_sub(body_h);
    let from_bottom = app.scroll_from_bottom.min(max_scroll);
    let scroll_y = max_scroll.saturating_sub(from_bottom) as u16;

    app.transcript_y_range = Some((
        transcript_area.y,
        transcript_area.y.saturating_add(transcript_area.height),
    ));
    app.last_transcript_height = transcript_area.height;
    // The transcript is intentionally borderless, and Paragraph rendering can
    // leave stale terminal cells visible when a scrolled viewport lands on blank
    // rows. Clear the full transcript rect before painting the current slice so
    // whitespace is real whitespace, not diff-buffer leftovers from a previous
    // scroll position.
    f.render_widget(Clear, transcript_area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll_y, 0)),
        transcript_area,
    );
}

pub(super) fn draw_todo_panel(f: &mut Frame, area: Rect, todo: &TodoState) {
    let title = format!(" todo {} / {} ", todo.completed, todo.total);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    let lines: Vec<Line<'static>> = todo
        .items
        .iter()
        .take(inner.height as usize)
        .map(|item| {
            let (mark, style) = match item.status {
                TodoItemStatus::Pending => ("[ ]", Style::default().fg(Color::Gray)),
                TodoItemStatus::InProgress => ("[~]", Style::default().fg(Color::Yellow)),
                TodoItemStatus::Completed => ("[x]", Style::default().fg(Color::DarkGray)),
            };
            Line::from(vec![
                Span::styled(mark.to_string(), style),
                Span::raw(" "),
                Span::styled(
                    truncate(&item.text, inner.width.saturating_sub(5) as usize),
                    style,
                ),
            ])
        })
        .collect();
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn draw_composer(
    f: &mut Frame,
    area: Rect,
    app: &App,
    top_titles: Option<Vec<Line<'static>>>,
    bottom_title: Option<Line<'static>>,
) {
    let (title, color) = if app.rename_target.is_some() {
        (" rename (Enter=save · Esc=cancel) ", Color::Magenta)
    } else {
        match app.zone {
            Zone::SingleAgent => ("", COMPOSER_CHROME_COLOR),
            Zone::Config => (
                " config (↑/↓ fields · ←/→ options · Enter=save · Esc=back) ",
                Color::Cyan,
            ),
            _ => (
                " dispatch (Enter=spawn · Shift+Enter=newline · Tab=provider · Ctrl+R=rename) ",
                COMPOSER_CHROME_COLOR,
            ),
        }
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color));
    if let Some(top_titles) = top_titles {
        for top_title in top_titles {
            block = block.title_top(top_title);
        }
    } else if !title.is_empty() {
        block = block.title(Span::styled(title, Style::default().fg(color)));
    }
    if let Some(bottom_title) = bottom_title {
        block = block.title_bottom(bottom_title);
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    let padded = Rect {
        x: inner.x.saturating_add(1),
        y: inner.y.saturating_add(1),
        width: inner.width.saturating_sub(2).max(1),
        height: inner.height.saturating_sub(2).max(1),
    };

    let buf = composer_display_text(&app.input, app.cursor_pos);
    let lines = Paragraph::new(buf.clone())
        .wrap(Wrap { trim: false })
        .line_count(padded.width.max(1));
    let scroll_y = lines.saturating_sub(padded.height as usize) as u16;
    f.render_widget(
        Paragraph::new(buf)
            .wrap(Wrap { trim: false })
            .scroll((scroll_y, 0)),
        padded,
    );
}

/// Return a `Rect` centered in `r` with the given width and height.
pub(super) fn centered_rect(width: u16, height: u16, r: Rect) -> Rect {
    let x = r.x + (r.width.saturating_sub(width)) / 2;
    let y = r.y + (r.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}
