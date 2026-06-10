use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, List, ListItem, Padding, Paragraph, Row, Table,
    TableState,
};
use ratatui::Frame;

use crate::task::{Task, TaskStatus};

use super::app::{App, Mode, PickerState};

pub fn render(frame: &mut Frame, app: &mut App) {
    let show_filter = app.filter_active || !app.filter.is_empty();
    let filter_height = if show_filter { 1 } else { 0 };

    if app.peek.is_some() {
        let [header_area, filter_area, table_area, preview_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(filter_height),
            Constraint::Percentage(35),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        render_header(frame, header_area, app);
        if show_filter {
            render_filter(frame, filter_area, app);
        }
        render_tasks(frame, table_area, app);

        let task_name = app.selected_task().map(|t| t.name.as_str()).unwrap_or("—");
        let content = app.peek.as_deref().unwrap_or("");
        render_preview(frame, preview_area, task_name, content);

        render_footer(frame, footer_area, app);
    } else {
        let [header_area, filter_area, table_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(filter_height),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        render_header(frame, header_area, app);
        if show_filter {
            render_filter(frame, filter_area, app);
        }
        render_tasks(frame, table_area, app);

        render_footer(frame, footer_area, app);
    }

    // Overlay popups
    match &app.mode {
        Mode::Normal => {}
        Mode::NewTaskPickProject(picker) | Mode::RunPickSession { picker, .. } => {
            render_picker_popup(frame, picker);
        }
        Mode::NewTaskEnterName {
            name,
            create_worktree,
            ..
        } => {
            render_new_task_popup(frame, name, *create_worktree);
        }
        Mode::SpawnEnterPath(path) => {
            render_enter_path_popup(frame, path);
        }
        Mode::ConfirmDropTask { name } => {
            render_confirm_drop_popup(frame, name);
        }
    }
}

fn render_filter(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(" /", Style::new().fg(Color::Cyan))];
    spans.push(Span::raw(&app.filter));
    if app.filter_active {
        spans.push(Span::styled("█", Style::new().fg(Color::DarkGray)));
    }
    frame.render_widget(Line::from(spans), area);
}

fn render_tasks(frame: &mut Frame, area: Rect, app: &App) {
    let visible = app.visible_tasks();
    if visible.is_empty() {
        if app.filter.is_empty() {
            let text = Paragraph::new("No tasks. Press n to create one.")
                .alignment(Alignment::Center)
                .fg(Color::DarkGray);
            let y = area.y + area.height / 2;
            frame.render_widget(text, Rect::new(area.x, y, area.width, 1));
        } else {
            let text = Paragraph::new("No matching tasks.")
                .alignment(Alignment::Center)
                .fg(Color::DarkGray);
            let y = area.y + area.height / 2;
            frame.render_widget(text, Rect::new(area.x, y, area.width, 1));
        }
    } else {
        render_table(frame, area, &visible, app.selected);
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let total = app.tasks.len();
    let needs = app.needs_attention_count();

    let mut spans = vec![Span::styled(" tam", Style::new().bold()), Span::raw(" — ")];

    if total == 0 {
        spans.push(Span::raw("no tasks"));
    } else {
        spans.push(Span::raw(format!(
            "{total} task{}",
            if total == 1 { "" } else { "s" }
        )));
        if needs > 0 {
            spans.push(Span::styled(
                format!(" ({needs} needs input)"),
                Style::new().fg(Color::Yellow),
            ));
        }
    }

    frame.render_widget(Line::from(spans), area);
}

fn render_table(frame: &mut Frame, area: Rect, tasks: &[&Task], selected: usize) {
    let header = Row::new([
        Cell::from("STATUS"),
        Cell::from("REPO"),
        Cell::from("TASK"),
        Cell::from("AGENT"),
        Cell::from("OWN"),
        Cell::from("DIR"),
        Cell::from("CTX"),
    ])
    .style(Style::new().fg(Color::DarkGray))
    .bottom_margin(0);

    let rows: Vec<Row> = tasks
        .iter()
        .map(|task| {
            let status = task.status();
            let (icon, color) = status_display(&status);
            let dir = shorten_home(&task.dir.display().to_string());
            let agent = task
                .agent_info
                .as_ref()
                .map(|a| a.provider.as_str())
                .unwrap_or("-");
            let ctx = task
                .agent_info
                .as_ref()
                .and_then(|a| a.context_percent)
                .map(|p| context_display(p))
                .unwrap_or_else(|| Span::raw(""));
            let owned = if task.owned {
                Span::styled("✔", Style::new().fg(Color::Green))
            } else {
                Span::styled("✘", Style::new().fg(Color::DarkGray))
            };
            // mark running agents whose Slack notifications are muted
            let task_label = match task.agent_info.as_ref() {
                Some(a) if !a.notify => format!("{} 🔕", task.name),
                _ => task.name.clone(),
            };

            Row::new([
                Cell::from(Span::styled(icon, Style::new().fg(color))),
                Cell::from(task.repo_name.as_str()),
                Cell::from(task_label),
                Cell::from(agent),
                Cell::from(owned),
                Cell::from(dir),
                Cell::from(ctx),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(10), // STATUS
        Constraint::Length(15), // REPO
        Constraint::Length(15), // TASK
        Constraint::Length(10), // AGENT
        Constraint::Length(3),  // OWN
        Constraint::Fill(1),    // DIR
        Constraint::Length(6),  // CTX
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::NONE))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    let mut table_state = TableState::default();
    if !tasks.is_empty() {
        table_state.select(Some(selected));
    }

    frame.render_stateful_widget(table, area, &mut table_state);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &mut App) {
    if let Some(msg) = app.status_message() {
        let line = Line::from(format!(" {msg}")).fg(Color::Yellow);
        frame.render_widget(line, area);
    } else {
        let hints = match app.mode {
            Mode::Normal => {
                let selected = app.selected_task();
                let has_agent = selected.map(|t| t.agent_info.is_some()).unwrap_or(false);

                let mut hints = Vec::new();
                if has_agent {
                    hints.extend([
                        Span::styled(" enter", Style::new().bold()),
                        Span::raw(":attach  "),
                        Span::styled("s", Style::new().bold()),
                        Span::raw(":stop  "),
                        Span::styled("b", Style::new().bold()),
                        Span::raw(":notify  "),
                    ]);
                } else if selected.is_some() {
                    hints.extend([Span::styled(" r", Style::new().bold()), Span::raw(":run  ")]);
                }
                hints.extend([
                    Span::styled("n", Style::new().bold()),
                    Span::raw(":new  "),
                    Span::styled("/", Style::new().bold()),
                    Span::raw(":filter  "),
                    Span::styled("p", Style::new().bold()),
                    Span::raw(":peek  "),
                ]);
                for cmd in &app.commands {
                    hints.push(Span::styled(cmd.key.clone(), Style::new().bold()));
                    hints.push(Span::raw(format!(":{} ", cmd.name)));
                }
                hints.extend([
                    Span::styled("d", Style::new().bold()),
                    Span::raw(":drop  "),
                    Span::styled("q", Style::new().bold()),
                    Span::raw(":quit"),
                ]);
                hints
            }
            Mode::NewTaskEnterName { .. } => vec![
                Span::styled(" enter", Style::new().bold()),
                Span::raw(":create  "),
                Span::styled("tab", Style::new().bold()),
                Span::raw(":toggle  "),
                Span::styled("esc", Style::new().bold()),
                Span::raw(":cancel"),
            ],
            Mode::ConfirmDropTask { .. } => vec![
                Span::styled(" y", Style::new().bold()),
                Span::raw("/"),
                Span::styled("enter", Style::new().bold()),
                Span::raw(":confirm  "),
                Span::styled("any", Style::new().bold()),
                Span::raw(":cancel"),
            ],
            _ => vec![
                Span::styled(" enter", Style::new().bold()),
                Span::raw(":select  "),
                Span::styled("esc", Style::new().bold()),
                Span::raw(":cancel"),
            ],
        };
        frame.render_widget(Line::from(hints).fg(Color::DarkGray), area);
    }
}

fn render_preview(frame: &mut Frame, area: Rect, task_name: &str, raw_content: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(format!(" {task_name} "));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let parse_rows = 500_u16;
    let mut parser = vt100::Parser::new(parse_rows, inner.width, 0);
    parser.process(raw_content.as_bytes());

    if parser.screen().alternate_screen() {
        parser.process(b"\x1b[?1049l");
    }
    let screen = parser.screen();

    let mut all_lines: Vec<Line> = (0..parse_rows)
        .map(|row| {
            let mut spans = Vec::new();
            for col in 0..inner.width {
                let cell = screen.cell(row, col).unwrap();
                if cell.is_wide_continuation() {
                    continue;
                }
                let contents = cell.contents();
                let style = vt100_style_to_ratatui(cell);
                if contents.is_empty() {
                    spans.push(Span::styled(" ".to_string(), style));
                } else {
                    spans.push(Span::styled(contents.to_string(), style));
                }
            }
            Line::from(spans)
        })
        .collect();

    while all_lines.last().is_some_and(|l| line_is_blank(l)) {
        all_lines.pop();
    }

    let display_lines: Vec<Line> = if all_lines.len() > inner.height as usize {
        all_lines.split_off(all_lines.len() - inner.height as usize)
    } else {
        all_lines
    };

    frame.render_widget(Paragraph::new(display_lines), inner);
}

fn line_is_blank(line: &Line) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

fn vt100_style_to_ratatui(cell: &vt100::Cell) -> Style {
    let mut style = Style::new();
    style = style.fg(vt100_color_to_ratatui(cell.fgcolor()));
    style = style.bg(vt100_color_to_ratatui(cell.bgcolor()));
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn render_picker_popup(frame: &mut Frame, picker: &PickerState) {
    let area = centered_rect(60, 60, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan))
        .title(format!(" {} ", picker.title))
        .title_style(Style::new().bold().fg(Color::Cyan))
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);

    let filter_line = Line::from(vec![
        Span::styled("> ", Style::new().fg(Color::Cyan)),
        Span::raw(&picker.filter),
        Span::styled("█", Style::new().fg(Color::DarkGray)),
    ]);
    frame.render_widget(filter_line, filter_area);

    let filtered = picker.filtered_items();
    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == picker.selected {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            ListItem::new(item.display.as_str()).style(style)
        })
        .collect();

    frame.render_widget(List::new(items), list_area);
}

fn render_new_task_popup(frame: &mut Frame, name: &str, create_worktree: bool) {
    // 3 content rows + 2 border + 2 vertical padding = 7
    let area = centered_fixed_rect(50, 7, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan))
        .title(" New Task ")
        .title_style(Style::new().bold().fg(Color::Cyan))
        .padding(Padding::new(2, 2, 1, 1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [name_area, _, wt_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let name_line = Line::from(vec![
        Span::raw("Task name: "),
        Span::styled(name, Style::new().bold()),
        Span::styled("█", Style::new().fg(Color::DarkGray)),
    ]);
    frame.render_widget(name_line, name_area);

    let wt_check = if create_worktree { "x" } else { " " };
    let wt_line = Line::from(vec![
        Span::raw("["),
        Span::styled(
            wt_check,
            Style::new().fg(if create_worktree {
                Color::Green
            } else {
                Color::DarkGray
            }),
        ),
        Span::raw("] Create worktree  "),
        Span::styled("tab", Style::new().fg(Color::DarkGray)),
        Span::styled(" toggle", Style::new().fg(Color::DarkGray)),
    ]);
    frame.render_widget(wt_line, wt_area);
}

fn render_enter_path_popup(frame: &mut Frame, path: &str) {
    // 1 content row + 2 border + 2 vertical padding = 5
    let area = centered_fixed_rect(60, 5, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan))
        .title(" Enter Path ")
        .title_style(Style::new().bold().fg(Color::Cyan))
        .padding(Padding::new(2, 2, 1, 1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let input_line = Line::from(vec![
        Span::styled("> ", Style::new().fg(Color::Cyan)),
        Span::raw(path),
        Span::styled("█", Style::new().fg(Color::DarkGray)),
    ]);
    frame.render_widget(input_line, inner);
}

fn render_confirm_drop_popup(frame: &mut Frame, name: &str) {
    // Size to content: "Drop task {name}? y/n" + padding + border
    let content_width = "Drop task ".len() + name.len() + "? y/n".len();
    // +8 for padding + border, 1 content row + 2 border + 2 vertical padding = 5
    let width = (content_width as u16 + 8).max(30);
    let area = centered_fixed_rect(width, 5, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Red))
        .title(" Confirm Drop ")
        .title_style(Style::new().bold().fg(Color::Red))
        .padding(Padding::new(2, 2, 1, 1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let msg = Line::from(vec![
        Span::raw("Drop task "),
        Span::styled(name, Style::new().bold()),
        Span::raw("? "),
        Span::styled("y", Style::new().bold().fg(Color::Red)),
        Span::raw("/"),
        Span::styled("n", Style::new().bold()),
    ]);
    frame.render_widget(msg, inner);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let [_, center_v, _] = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .areas(area);
    let [_, center, _] = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .areas(center_v);
    center
}

fn centered_fixed_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

fn status_display(status: &TaskStatus) -> (&'static str, Color) {
    match status {
        TaskStatus::Run => ("● run", Color::Blue),
        TaskStatus::Input => ("▲ input", Color::Yellow),
        TaskStatus::Block => ("▲ block", Color::Red),
        TaskStatus::Idle => ("○ idle", Color::DarkGray),
        TaskStatus::Stale => ("◌ stale", Color::DarkGray),
        TaskStatus::Gone => ("✗ gone", Color::Red),
    }
}

fn context_display(pct: u8) -> Span<'static> {
    let color = if pct >= 90 {
        Color::Red
    } else if pct >= 70 {
        Color::Yellow
    } else {
        Color::Green
    };
    Span::styled(format!("{pct}%"), Style::new().fg(color))
}

pub fn shorten_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}
