//! Rendering helpers for the dashboard, editor panes, and Codex UI.

use crate::*;

#[derive(Clone)]
struct StyledSegment {
    text: String,
    style: Style,
}

/// Renders the full application frame.
pub(crate) fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(app.ui.bg)), area);

    let [left, right] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .areas(area);

    let [left_top, left_bottom] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .areas(left);

    let [terminal_area, performance_area] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(63), Constraint::Percentage(37)])
        .areas(left_top);

    let [editor_area, codex_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .areas(right);

    render_pty_pane(
        frame,
        terminal_area,
        app.ui,
        app.focus == Focus::Terminal,
        &mut app.terminal,
    );
    performance_block(frame, performance_area, app);
    project_tree(frame, left_bottom, app);
    render_editor(frame, editor_area, app);
    codex_block(frame, codex_area, app);
}

fn render_editor(frame: &mut Frame, area: Rect, app: &mut App) {
    if let Some(preview) = &mut app.editor_preview {
        match preview {
            EditorPreview::Image(preview) => {
                render_image_preview(frame, area, app.ui, app.focus == Focus::Editor, preview);
            }
            EditorPreview::Document(preview) => {
                render_document_preview(frame, area, app.ui, app.focus == Focus::Editor, preview);
            }
        }
    } else {
        render_pty_pane(
            frame,
            area,
            app.ui,
            app.focus == Focus::Editor,
            &mut app.nvim,
        );
    }
}

fn render_pty_pane(frame: &mut Frame, area: Rect, ui: UiTheme, focused: bool, pane: &mut PtyPane) {
    let inner = render_panel(frame, area, pane.title, ui, focused, ui.panel);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    pane.resize(inner);
    let snapshot = pane.snapshot(ui);
    let widget = Paragraph::new(snapshot.lines).style(Style::default().bg(ui.panel));
    frame.render_widget(widget, inner);

    if focused {
        if let Some((row, col)) = snapshot.cursor {
            let cursor_x = inner.x.saturating_add(col);
            let cursor_y = inner.y.saturating_add(row);
            if cursor_x < inner.right() && cursor_y < inner.bottom() {
                frame.set_cursor_position((cursor_x, cursor_y));
            }
        }
    }
}

fn render_image_preview(
    frame: &mut Frame,
    area: Rect,
    ui: UiTheme,
    focused: bool,
    preview: &mut ImagePreview,
) {
    let title = format!(
        " image  {} ",
        preview
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("preview")
    );
    let inner = render_panel(frame, area, &title, ui, focused, ui.panel);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let [meta_area, preview_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .areas(inner);

    let (width, height) = preview.image.dimensions();
    let meta = Paragraph::new(vec![
        Line::styled(
            preview.path.display().to_string(),
            Style::default().fg(ui.accent).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!("{width}x{height}  static preview"),
            Style::default().fg(ui.muted),
        ),
    ])
    .style(Style::default().bg(ui.panel));
    frame.render_widget(meta, meta_area);

    let widget = Paragraph::new(
        preview
            .lines(preview_area.width, preview_area.height, ui)
            .to_vec(),
    )
    .style(Style::default().bg(ui.panel));
    frame.render_widget(widget, preview_area);
}

fn render_document_preview(
    frame: &mut Frame,
    area: Rect,
    ui: UiTheme,
    focused: bool,
    preview: &DocumentPreview,
) {
    let title = format!(
        " {}  {} ",
        preview.kind,
        preview
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("preview")
    );
    let inner = render_panel(frame, area, &title, ui, focused, ui.panel);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let [meta_area, preview_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .areas(inner);

    let meta = Paragraph::new(vec![
        Line::styled(
            preview.path.display().to_string(),
            Style::default().fg(ui.accent).add_modifier(Modifier::BOLD),
        ),
        Line::styled(preview.summary.clone(), Style::default().fg(ui.muted)),
    ])
    .style(Style::default().bg(ui.panel));
    frame.render_widget(meta, meta_area);

    let widget = Paragraph::new(preview.lines.clone())
        .style(Style::default().bg(ui.panel).fg(ui.text))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, preview_area);
}

fn project_tree(frame: &mut Frame, area: Rect, app: &App) {
    let title = if let Some(picker) = &app.project_picker {
        if picker.search_active() {
            format!(
                " open project  {}  search: {} ",
                picker.current_dir.display(),
                picker.search_query
            )
        } else {
            format!(" open project  {} ", picker.current_dir.display())
        }
    } else if app.project_tree.search_active() {
        format!("project tree  search: {}", app.project_tree.search_query)
    } else {
        "project tree".to_string()
    };

    let inner = render_panel(
        frame,
        area,
        &title,
        app.ui,
        app.focus == Focus::ProjectTree,
        app.ui.panel,
    );

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let footer_height = command_footer_height(app);
    let (list_area, footer_area) = if footer_height > 0 {
        let [list_area, footer_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(footer_height)])
            .areas(inner);
        (list_area, Some(footer_area))
    } else {
        (inner, None)
    };

    if let Some(picker) = &app.project_picker {
        let items = picker
            .entries
            .iter()
            .map(|entry| {
                ListItem::new(Line::from(vec![
                    Span::styled("▸", Style::default().fg(app.ui.accent)),
                    Span::raw("  "),
                    Span::styled("󰉋", Style::default().fg(app.ui.accent)),
                    Span::raw("  "),
                    Span::styled(entry.label.clone(), Style::default().fg(app.ui.text)),
                ]))
            })
            .collect::<Vec<_>>();

        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(list_selection_fg(app.ui))
                    .bg(list_selection_bg(app.ui))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(" ");
        let mut state = ListState::default();
        if !picker.entries.is_empty() {
            state.select(Some(picker.selected));
        }
        frame.render_stateful_widget(list, list_area, &mut state);
    } else {
        let items = app
            .project_tree
            .visible
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let label = entry
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_else(|| entry.path.to_str().unwrap_or_default())
                    .to_string();
                let indent = "   ".repeat(entry.depth);
                let is_selected = index == app.project_tree.selected;
                let is_startup = entry.path == app.project_tree.root.join(STARTUP_FILE);
                let row_style = if is_selected {
                    Style::default()
                        .fg(list_selection_fg(app.ui))
                        .bg(list_selection_bg(app.ui))
                        .add_modifier(Modifier::BOLD)
                } else if is_startup {
                    Style::default()
                        .fg(app.ui.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(app.ui.text)
                };
                let accent_style = if is_selected {
                    row_style
                } else {
                    row_style.fg(app.ui.accent)
                };
                let toggle = if entry.is_dir {
                    if app.project_tree.expanded.contains(&entry.path) {
                        "▾"
                    } else {
                        "▸"
                    }
                } else {
                    " "
                };
                let icon = if entry.is_dir { "󰉋" } else { "󰈔" };

                ListItem::new(Line::from(vec![
                    Span::styled(indent, row_style),
                    Span::styled(toggle, accent_style),
                    Span::styled("  ", row_style),
                    Span::styled(icon, accent_style),
                    Span::styled("  ", row_style),
                    Span::styled(label, row_style),
                ]))
            })
            .collect::<Vec<_>>();

        let tree = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(list_selection_fg(app.ui))
                    .bg(list_selection_bg(app.ui))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(" ");
        let mut state = ListState::default();
        if !app.project_tree.visible.is_empty() {
            state.select(Some(app.project_tree.selected));
        }
        frame.render_stateful_widget(tree, list_area, &mut state);
    }

    if let Some(footer_area) = footer_area {
        render_project_tree_footer(frame, footer_area, app);
    }
}

fn command_footer_height(app: &App) -> u16 {
    let mut height = 0;
    if let Some(output) = &app.command_output {
        let output_lines = output.lines.len().min(6) as u16;
        height = height.max(output_lines.saturating_add(2));
    }
    if app.command_prompt.is_some() {
        height = height.saturating_add(4);
    }
    height
}

fn render_project_tree_footer(frame: &mut Frame, area: Rect, app: &App) {
    let output_height = app
        .command_output
        .as_ref()
        .map(|output| (output.lines.len().min(6) as u16).saturating_add(2))
        .unwrap_or(0);
    let prompt_height = if app.command_prompt.is_some() { 4 } else { 0 };

    let (output_area, prompt_area) = match (output_height > 0, prompt_height > 0) {
        (true, true) => {
            let [output_area, prompt_area] = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(output_height),
                    Constraint::Length(prompt_height),
                ])
                .areas(area);
            (Some(output_area), Some(prompt_area))
        }
        (true, false) => (Some(area), None),
        (false, true) => (None, Some(area)),
        (false, false) => (None, None),
    };

    if let (Some(output), Some(output_area)) = (&app.command_output, output_area) {
        let block = framed_block(&output.title, app.ui, false, app.ui.panel_alt);
        let widget = Paragraph::new(output.lines.iter().take(6).cloned().collect::<Vec<_>>())
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, output_area);
    }

    if let (Some(command), Some(prompt_area)) = (&app.command_prompt, prompt_area) {
        let command_block = framed_block(
            "command  enter run  esc cancel",
            app.ui,
            true,
            app.ui.panel_alt,
        );
        let command_inner = command_block.inner(prompt_area);
        let command_widget = Paragraph::new(Line::from(vec![
            Span::styled(
                ":",
                Style::default()
                    .fg(app.ui.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                command
                    .strip_prefix(':')
                    .map(str::to_string)
                    .unwrap_or_else(|| command.clone()),
                Style::default().fg(app.ui.text),
            ),
        ]))
        .block(command_block)
        .wrap(Wrap { trim: false });
        frame.render_widget(command_widget, prompt_area);

        let cursor_x = command_inner
            .x
            .saturating_add(command.chars().count() as u16);
        let cursor_y = command_inner.y;
        if cursor_x < command_inner.right()
            && cursor_y < command_inner.bottom()
            && app.focus == Focus::ProjectTree
        {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

fn performance_block(frame: &mut Frame, area: Rect, app: &App) {
    let [gpu_area, cpu_area, mem_area, graph_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
        ])
        .areas(area);

    let gauge_style = Style::default()
        .fg(if is_glow_mood(app.ui) {
            app.ui.glow_hot
        } else {
            app.ui.accent
        })
        .bg(if is_glow_mood(app.ui) {
            app.ui.glow_fill
        } else {
            app.ui.panel_alt
        });
    let focus = false;
    let metrics = &app.terminal_metrics;
    let gpu_percent = metrics.gpu_percent.unwrap_or(0.0).clamp(0.0, 100.0) as u16;
    let cpu_percent = metrics.cpu_percent.clamp(0.0, 100.0) as u16;
    let mem_percent = metrics.memory_percent().clamp(0.0, 100.0) as u16;

    frame.render_widget(
        Gauge::default()
            .block(framed_block(
                "gpu / terminal",
                app.ui,
                focus,
                app.ui.panel_alt,
            ))
            .gauge_style(gauge_style)
            .label(
                metrics
                    .gpu_percent
                    .map(|value| format!("{value:.1}%"))
                    .unwrap_or_else(|| "unavailable".to_string()),
            )
            .percent(gpu_percent),
        gpu_area,
    );
    frame.render_widget(
        Gauge::default()
            .block(framed_block(
                "cpu / terminal",
                app.ui,
                false,
                app.ui.panel_alt,
            ))
            .gauge_style(gauge_style)
            .label(format!(
                "{:.1}% {}",
                metrics.cpu_percent, metrics.active_process
            ))
            .percent(cpu_percent),
        cpu_area,
    );
    frame.render_widget(
        Gauge::default()
            .block(framed_block(
                "mem / terminal",
                app.ui,
                false,
                app.ui.panel_alt,
            ))
            .gauge_style(gauge_style)
            .label(metrics.memory_label())
            .percent(mem_percent),
        mem_area,
    );

    let [status_area, spark_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .areas(graph_area);

    let detail = Paragraph::new(vec![
        Line::styled(
            format!(
                "pid {}  processes {}",
                metrics.shell_pid.unwrap_or(0),
                metrics.process_count
            ),
            Style::default().fg(app.ui.text),
        ),
        Line::styled(metrics.note.clone(), Style::default().fg(app.ui.muted)),
    ])
    .block(framed_block(
        "terminal job",
        app.ui,
        false,
        app.ui.panel_alt,
    ))
    .wrap(Wrap { trim: true });
    frame.render_widget(detail, status_area);

    let spark = Sparkline::default()
        .block(framed_block(
            "cpu / history",
            app.ui,
            false,
            app.ui.panel_alt,
        ))
        .data(&metrics.cpu_history)
        .style(Style::default().fg(if is_glow_mood(app.ui) {
            app.ui.glow_hot
        } else {
            app.ui.accent
        }))
        .max(
            metrics
                .cpu_history
                .iter()
                .copied()
                .max()
                .unwrap_or(100)
                .max(100),
        );

    frame.render_widget(spark, spark_area);
}

fn codex_block(frame: &mut Frame, area: Rect, app: &mut App) {
    let inner = render_panel(
        frame,
        area,
        "codex",
        app.ui,
        app.focus == Focus::Codex,
        app.ui.panel,
    );
    app.codex_history_area = None;
    app.codex_change_list_area = None;

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let change_height = app
        .codex_chat
        .last_change_set
        .as_ref()
        .map(|change_set| codex_change_block_height(change_set, inner.height));
    let (change_area, history_area, input_area) = if let Some(change_height) = change_height {
        let [change_area, history_area, input_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(change_height),
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .areas(inner);
        (Some(change_area), Some(history_area), input_area)
    } else if inner.height >= 6 {
        let [history_area, input_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .areas(inner);
        (None, Some(history_area), input_area)
    } else {
        (None, None, inner)
    };

    if let (Some(change_set), Some(change_area)) = (&app.codex_chat.last_change_set, change_area) {
        app.codex_change_list_area = render_codex_change_block(frame, change_area, app, change_set);
    }

    if let Some(history_area) = history_area {
        let history_lines = codex_history_lines(app, history_area.width);
        let scroll = clamp_scroll(
            app.codex_chat.history_scroll,
            history_lines.len(),
            history_area.height,
        );
        app.codex_chat.history_scroll = scroll;
        app.codex_history_area = Some(history_area);
        let history = Paragraph::new(history_lines)
            .style(Style::default().bg(app.ui.panel).fg(app.ui.text))
            .scroll((scroll as u16, 0));
        frame.render_widget(history, history_area);
    }

    let input_lines = if let Some(prompt) = &app.create_prompt {
        vec![
            Line::from(vec![
                Span::styled(
                    match prompt.kind {
                        CreateKind::File => "create file",
                        CreateKind::Directory => "create directory",
                    },
                    Style::default()
                        .fg(app.ui.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(prompt.input.clone(), Style::default().fg(app.ui.text)),
            ]),
            Line::styled(
                "enter confirm  esc cancel",
                Style::default().fg(app.ui.muted),
            ),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::styled(
                    "you",
                    Style::default()
                        .fg(app.ui.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    app.codex_chat.input.clone(),
                    Style::default().fg(app.ui.text),
                ),
            ]),
            Line::styled("enter send", Style::default().fg(app.ui.muted)),
        ]
    };

    let input_block = framed_block("chat", app.ui, true, app.ui.panel_alt);
    let input_inner = input_block.inner(input_area);
    let input = Paragraph::new(input_lines)
        .block(input_block)
        .wrap(Wrap { trim: false });
    frame.render_widget(input, input_area);

    let (cursor_prefix, cursor_len) = if let Some(prompt) = &app.create_prompt {
        (
            match prompt.kind {
                CreateKind::File => "create file  ",
                CreateKind::Directory => "create directory  ",
            },
            prompt.input.chars().count(),
        )
    } else {
        ("you  ", app.codex_chat.input.chars().count())
    };
    let cursor_x = input_inner
        .x
        .saturating_add((cursor_prefix.chars().count() + cursor_len) as u16);
    let cursor_y = input_inner.y;

    if cursor_x < input_inner.right() && cursor_y < input_inner.bottom() {
        if app.create_prompt.is_some() || app.focus == Focus::Codex {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

fn codex_history_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }

    let mut lines = Vec::new();
    for (index, message) in app.codex_chat.messages.iter().enumerate() {
        let label = match message.role {
            ChatRole::User => "you",
            ChatRole::Assistant => "codex",
        };
        lines.extend(render_codex_message(label, &message.content, width, app.ui));

        if index + 1 < app.codex_chat.messages.len() {
            lines.push(Line::default());
        }
    }
    lines
}

fn render_codex_message(label: &str, content: &str, width: u16, ui: UiTheme) -> Vec<Line<'static>> {
    let label_style = Style::default().fg(ui.accent).add_modifier(Modifier::BOLD);
    let prefix = format!("{label}  ");
    let continuation = " ".repeat(prefix.chars().count());
    let prefix_width = prefix.chars().count();
    let body_width = (width as usize).saturating_sub(prefix_width).max(1);
    let mut lines = Vec::new();
    let mut code_block = false;
    let mut emitted_any = false;

    for raw_line in content.split('\n') {
        let raw_line = raw_line.trim_end_matches('\r');
        if raw_line.trim_start().starts_with("```") {
            code_block = !code_block;
            continue;
        }

        let body_segments = if code_block {
            render_code_block_line(raw_line, ui)
        } else {
            render_markdown_block_line(raw_line, ui)
        };

        let wrapped = wrap_segments(&body_segments, body_width);
        if wrapped.is_empty() {
            let leader = if emitted_any {
                continuation.clone()
            } else {
                prefix.clone()
            };
            let leader_style = if emitted_any {
                Style::default().fg(ui.muted)
            } else {
                label_style
            };
            lines.push(Line::from(vec![
                Span::styled(leader, leader_style),
                Span::raw(""),
            ]));
        } else {
            for (index, chunk) in wrapped.into_iter().enumerate() {
                let leader = if emitted_any || index > 0 {
                    continuation.clone()
                } else {
                    prefix.clone()
                };
                let leader_style = if emitted_any || index > 0 {
                    Style::default().fg(ui.muted)
                } else {
                    label_style
                };
                let mut spans = vec![Span::styled(leader, leader_style)];
                spans.extend(
                    chunk
                        .into_iter()
                        .map(|segment| Span::styled(segment.text, segment.style)),
                );
                lines.push(Line::from(spans));
            }
        }

        emitted_any = true;
    }

    if lines.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(prefix, label_style),
            Span::raw(""),
        ]));
    }

    lines
}

fn render_code_block_line(line: &str, ui: UiTheme) -> Vec<StyledSegment> {
    let mut segments = Vec::new();
    segments.push(StyledSegment {
        text: "  ".to_string(),
        style: Style::default().fg(ui.muted),
    });
    if !line.is_empty() {
        segments.push(StyledSegment {
            text: line.to_string(),
            style: Style::default()
                .fg(ui.special)
                .bg(ui.panel_alt)
                .add_modifier(Modifier::DIM),
        });
    }
    segments
}

fn render_markdown_block_line(line: &str, ui: UiTheme) -> Vec<StyledSegment> {
    let base_style = Style::default().fg(ui.text);
    let trimmed = line.trim_start();
    let indent_width = line.len().saturating_sub(trimmed.len());
    let indent = " ".repeat(indent_width);
    let mut segments = Vec::new();

    if !indent.is_empty() {
        segments.push(StyledSegment {
            text: indent,
            style: Style::default().fg(ui.muted),
        });
    }

    if trimmed.is_empty() {
        return segments;
    }

    if is_markdown_rule(trimmed) {
        segments.push(StyledSegment {
            text: "─".repeat(12),
            style: Style::default().fg(ui.border).add_modifier(Modifier::DIM),
        });
        return segments;
    }

    if let Some((level, rest)) = markdown_heading(trimmed) {
        let style = Style::default().fg(ui.accent).add_modifier(Modifier::BOLD);
        segments.push(StyledSegment {
            text: "#".repeat(level),
            style: Style::default().fg(ui.accent_dim),
        });
        segments.push(StyledSegment {
            text: " ".to_string(),
            style,
        });
        segments.extend(parse_markdown_inline(rest, style, ui));
        return segments;
    }

    if let Some(rest) = markdown_unordered_list(trimmed) {
        segments.push(StyledSegment {
            text: "• ".to_string(),
            style: Style::default().fg(ui.accent).add_modifier(Modifier::BOLD),
        });
        segments.extend(parse_markdown_inline(rest, base_style, ui));
        return segments;
    }

    if let Some((marker, rest)) = markdown_ordered_list(trimmed) {
        segments.push(StyledSegment {
            text: format!("{marker} "),
            style: Style::default().fg(ui.accent).add_modifier(Modifier::BOLD),
        });
        segments.extend(parse_markdown_inline(rest, base_style, ui));
        return segments;
    }

    if let Some(rest) = trimmed.strip_prefix("> ") {
        segments.push(StyledSegment {
            text: "│ ".to_string(),
            style: Style::default().fg(ui.special),
        });
        segments.extend(parse_markdown_inline(
            rest,
            Style::default().fg(ui.text).add_modifier(Modifier::ITALIC),
            ui,
        ));
        return segments;
    }

    segments.extend(parse_markdown_inline(trimmed, base_style, ui));
    segments
}

fn markdown_heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }

    let rest = line.get(hashes..)?;
    let rest = rest.strip_prefix(' ')?;
    Some((hashes, rest))
}

fn markdown_unordered_list(line: &str) -> Option<&str> {
    ["- ", "* ", "+ "]
        .into_iter()
        .find_map(|marker| line.strip_prefix(marker))
}

fn markdown_ordered_list(line: &str) -> Option<(String, &str)> {
    let digit_count = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_count == 0 {
        return None;
    }

    let marker_end = digit_count + 1;
    let marker = line.get(..marker_end)?;
    if !marker.ends_with(['.', ')']) {
        return None;
    }

    let rest = line.get(marker_end..)?.strip_prefix(' ')?;
    Some((marker.to_string(), rest))
}

fn is_markdown_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 3 && trimmed.chars().all(|ch| matches!(ch, '-' | '*' | '_'))
}

fn parse_markdown_inline(text: &str, base_style: Style, ui: UiTheme) -> Vec<StyledSegment> {
    let mut segments = Vec::new();
    let mut plain = String::new();
    let chars = text.char_indices().collect::<Vec<_>>();
    let mut index = 0;

    while index < chars.len() {
        let (byte_index, ch) = chars[index];

        if ch == '`' {
            if let Some((next_index, code)) = consume_inline_marker(text, &chars, index, "`", "`") {
                flush_plain_segment(&mut plain, &mut segments, base_style);
                segments.push(StyledSegment {
                    text: code.to_string(),
                    style: Style::default()
                        .fg(ui.special)
                        .bg(ui.panel_alt)
                        .add_modifier(Modifier::BOLD),
                });
                index = next_index;
                continue;
            }
        }

        if let Some((next_index, label)) = consume_markdown_link(text, &chars, index) {
            flush_plain_segment(&mut plain, &mut segments, base_style);
            segments.push(StyledSegment {
                text: label.to_string(),
                style: Style::default()
                    .fg(ui.accent)
                    .add_modifier(Modifier::UNDERLINED),
            });
            index = next_index;
            continue;
        }

        if let Some((next_index, strong)) = consume_inline_marker(text, &chars, index, "**", "**")
            .or_else(|| consume_inline_marker(text, &chars, index, "__", "__"))
        {
            flush_plain_segment(&mut plain, &mut segments, base_style);
            segments.extend(parse_markdown_inline(
                strong,
                base_style.add_modifier(Modifier::BOLD),
                ui,
            ));
            index = next_index;
            continue;
        }

        if let Some((next_index, emphasis)) = consume_inline_marker(text, &chars, index, "*", "*")
            .or_else(|| consume_inline_marker(text, &chars, index, "_", "_"))
        {
            flush_plain_segment(&mut plain, &mut segments, base_style);
            segments.extend(parse_markdown_inline(
                emphasis,
                base_style.add_modifier(Modifier::ITALIC),
                ui,
            ));
            index = next_index;
            continue;
        }

        plain.push(ch);
        let next_byte = chars
            .get(index + 1)
            .map(|(offset, _)| *offset)
            .unwrap_or(text.len());
        if next_byte <= byte_index {
            break;
        }
        index += 1;
    }

    flush_plain_segment(&mut plain, &mut segments, base_style);
    segments
}

fn consume_inline_marker<'a>(
    text: &'a str,
    chars: &[(usize, char)],
    index: usize,
    open: &str,
    close: &str,
) -> Option<(usize, &'a str)> {
    let start = chars.get(index)?.0;
    let remainder = text.get(start..)?;
    if !remainder.starts_with(open) {
        return None;
    }

    let content_start = start + open.len();
    let tail = text.get(content_start..)?;
    let close_offset = tail.find(close)?;
    let content_end = content_start + close_offset;
    let content = text.get(content_start..content_end)?;
    if content.is_empty() {
        return None;
    }

    let consumed_bytes = open.len() + close_offset + close.len();
    let end = start + consumed_bytes;
    let next_index = chars.partition_point(|(offset, _)| *offset < end);
    Some((next_index, content))
}

fn consume_markdown_link<'a>(
    text: &'a str,
    chars: &[(usize, char)],
    index: usize,
) -> Option<(usize, &'a str)> {
    let start = chars.get(index)?.0;
    let remainder = text.get(start..)?;
    if !remainder.starts_with('[') {
        return None;
    }

    let label_end = remainder.find(']')?;
    let after_label = remainder.get(label_end + 1..)?;
    if !after_label.starts_with('(') {
        return None;
    }
    let url_end = after_label.find(')')?;
    let label = remainder.get(1..label_end)?;
    if label.is_empty() {
        return None;
    }

    let end = start + label_end + 1 + url_end + 1;
    let next_index = chars.partition_point(|(offset, _)| *offset < end);
    Some((next_index, label))
}

fn flush_plain_segment(plain: &mut String, segments: &mut Vec<StyledSegment>, style: Style) {
    if plain.is_empty() {
        return;
    }

    segments.push(StyledSegment {
        text: std::mem::take(plain),
        style,
    });
}

fn wrap_segments(segments: &[StyledSegment], width: usize) -> Vec<Vec<StyledSegment>> {
    if width == 0 {
        return Vec::new();
    }

    let mut wrapped = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0;

    for segment in segments {
        let mut chunk = String::new();
        let mut chunk_width = 0;

        for ch in segment.text.chars() {
            if current_width + chunk_width >= width {
                if !chunk.is_empty() {
                    current.push(StyledSegment {
                        text: std::mem::take(&mut chunk),
                        style: segment.style,
                    });
                    chunk_width = 0;
                }
                wrapped.push(std::mem::take(&mut current));
                current_width = 0;
            }

            chunk.push(ch);
            chunk_width += 1;

            if current_width + chunk_width >= width {
                current.push(StyledSegment {
                    text: std::mem::take(&mut chunk),
                    style: segment.style,
                });
                chunk_width = 0;
                wrapped.push(std::mem::take(&mut current));
                current_width = 0;
            }
        }

        if !chunk.is_empty() {
            current_width += chunk_width;
            current.push(StyledSegment {
                text: chunk,
                style: segment.style,
            });
        }
    }

    if !current.is_empty() {
        wrapped.push(current);
    }

    wrapped
}

fn codex_change_block_height(change_set: &CodexChangeSet, available_height: u16) -> u16 {
    let desired = (change_set.files.len().min(4) as u16).saturating_add(3);
    desired.min(available_height.saturating_sub(3)).max(3)
}

fn render_codex_change_block(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    change_set: &CodexChangeSet,
) -> Option<Rect> {
    let block = framed_block("changes", app.ui, false, app.ui.panel_alt);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    let count = change_set.files.len();
    let summary = match count {
        0 => "no files changed".to_string(),
        1 => "1 file changed".to_string(),
        _ => format!("{count} files changed"),
    };
    let (header_area, list_area) = if inner.height > 2 {
        let [header_area, list_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .areas(inner);
        (header_area, Some(list_area))
    } else {
        (inner, None)
    };

    let undo_label = if change_set.reverse_patch.is_some() {
        "ctrl+z undo"
    } else {
        "undo unavailable"
    };
    let header = Paragraph::new(codex_change_header_line(
        &summary,
        undo_label,
        header_area.width,
        app.ui,
    ))
    .style(Style::default().bg(app.ui.panel_alt));
    frame.render_widget(header, header_area);

    let Some(list_area) = list_area else {
        return None;
    };
    if list_area.width == 0 || list_area.height == 0 {
        return None;
    }

    let file_lines = change_set
        .files
        .iter()
        .map(|file| codex_changed_file_line(app, file, list_area.width))
        .collect::<Vec<_>>();
    let scroll = clamp_scroll(
        app.codex_chat.change_scroll,
        file_lines.len(),
        list_area.height,
    );
    let list = Paragraph::new(file_lines)
        .style(Style::default().bg(app.ui.panel_alt))
        .scroll((scroll as u16, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(list, list_area);
    Some(list_area)
}

fn codex_change_header_line(summary: &str, action: &str, width: u16, ui: UiTheme) -> Line<'static> {
    let summary_len = summary.chars().count();
    let action_len = action.chars().count();
    let spacing = (width as usize)
        .saturating_sub(summary_len)
        .saturating_sub(action_len)
        .max(2);
    Line::from(vec![
        Span::styled(
            summary.to_string(),
            Style::default().fg(ui.text).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(spacing)),
        Span::styled(
            action.to_string(),
            Style::default()
                .fg(if is_glow_mood(ui) {
                    ui.glow_hot
                } else {
                    ui.accent
                })
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn codex_changed_file_line(app: &App, file: &CodexChangedFile, width: u16) -> Line<'static> {
    let relative = relative_to_root(&app.project_tree.root, &file.path);
    let mut stats = String::new();
    if file.additions > 0 {
        stats.push_str(&format!("+{}", file.additions));
    }
    if file.deletions > 0 {
        if !stats.is_empty() {
            stats.push(' ');
        }
        stats.push_str(&format!("-{}", file.deletions));
    }
    let max_path = (width as usize)
        .saturating_sub(stats.chars().count())
        .saturating_sub(6)
        .max(8);
    let truncated = truncate_with_ellipsis(&relative, max_path);
    let spacing = (width as usize)
        .saturating_sub(truncated.chars().count())
        .saturating_sub(stats.chars().count())
        .saturating_sub(2)
        .max(1);

    let mut spans = vec![
        Span::styled("󰈔 ", Style::default().fg(app.ui.accent)),
        Span::styled(truncated, Style::default().fg(app.ui.text)),
    ];
    if !stats.is_empty() {
        spans.push(Span::raw(" ".repeat(spacing)));
        if file.additions > 0 {
            spans.push(Span::styled(
                format!("+{}", file.additions),
                Style::default()
                    .fg(app.ui.ansi[2])
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if file.deletions > 0 {
            if file.additions > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                format!("-{}", file.deletions),
                Style::default()
                    .fg(app.ui.ansi[1])
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    Line::from(spans)
}

fn render_panel(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    ui: UiTheme,
    focused: bool,
    fill: Color,
) -> Rect {
    let block = framed_block(title, ui, focused, fill);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if !is_glow_mood(ui) || inner.width < 3 || inner.height < 3 {
        return inner;
    }

    let top = Rect::new(inner.x, inner.y, inner.width, 1);
    let bottom = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let left = Rect::new(inner.x, inner.y, 1, inner.height);
    let right = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
    let spill = Block::default().style(Style::default().bg(ui.glow_fill));
    let edge = Block::default().style(Style::default().bg(ui.glow_outer));
    frame.render_widget(&spill, top);
    frame.render_widget(&spill, bottom);
    frame.render_widget(&edge, left);
    frame.render_widget(&edge, right);

    inset_rect(inner, 1)
}

fn framed_block<'a>(title: &'a str, ui: UiTheme, focused: bool, fill: Color) -> Block<'a> {
    let border_style = if focused {
        Style::default()
            .fg(if is_glow_mood(ui) {
                ui.glow_hot
            } else {
                ui.accent
            })
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(if is_glow_mood(ui) {
            ui.glow_inner
        } else {
            ui.border
        })
    };
    let title_style = if is_glow_mood(ui) {
        Style::default()
            .fg(ui.bg)
            .bg(if focused { ui.glow_hot } else { ui.glow_inner })
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ui.accent).add_modifier(Modifier::BOLD)
    };

    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(Style::default().bg(fill))
        .title(Line::from(vec![Span::styled(
            format!(" {} ", title),
            title_style,
        )]))
}

fn inset_rect(area: Rect, inset: u16) -> Rect {
    if area.width <= inset.saturating_mul(2) || area.height <= inset.saturating_mul(2) {
        return area;
    }

    Rect::new(
        area.x.saturating_add(inset),
        area.y.saturating_add(inset),
        area.width.saturating_sub(inset.saturating_mul(2)),
        area.height.saturating_sub(inset.saturating_mul(2)),
    )
}

fn is_glow_mood(ui: UiTheme) -> bool {
    matches!(ui.mood, ThemeMood::Synthwave84)
}

fn list_selection_bg(ui: UiTheme) -> Color {
    if is_glow_mood(ui) {
        ui.glow_hot
    } else {
        ui.accent
    }
}

fn list_selection_fg(ui: UiTheme) -> Color {
    if is_glow_mood(ui) { ui.bg } else { ui.bg }
}

fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut output = text.chars().take(keep).collect::<String>();
    output.push('…');
    output
}
