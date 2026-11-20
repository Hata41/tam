//! The task list: a row per task, under a header row per repository (or
//! the most urgent first), and a row per member below a task with several.
//! Its columns fit the width: the least useful go first when it is narrow.

use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, HighlightSpacing, Row as TableRow, Table, TableState};
use ratatui::Frame;
use tam_proto::AgentState;

use crate::task::{Member, Task, TaskStatus};

use super::app::{self, App, Order, Row};
use super::branch::Observation;
use super::fuzzy;
use super::theme;
use super::triage::{age, one_line};
use super::ui::{agent_detail_line, context_display, first_line, status_display, CONTEXT_GAUGE};

/// From this width on, a column says what each member waits on.
const NOTE_FROM: u16 = 100;
/// The note column's narrowest.
const NOTE_MIN: u16 = 20;
/// Task names this long are not cut while other columns can make room.
const TASK_WANTED: u16 = 20;
/// Between columns.
const SPACING: u16 = 1;
/// Left of the rows: the selected one's mark.
const MARK: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Status,
    /// Time in state.
    Age,
    Task,
    /// Shown when the list is not grouped by repository.
    Repo,
    /// Ahead, behind, uncommitted.
    Branch,
    Agent,
    Ctx,
    /// What the member waits on.
    Note,
}

/// Columns dropped first when the list is too narrow for them all.
const DROP_ORDER: [Col; 6] = [
    Col::Note,
    Col::Agent,
    Col::Ctx,
    Col::Branch,
    Col::Repo,
    Col::Age,
];

impl Col {
    fn header(self) -> &'static str {
        match self {
            Col::Status => "STATUS",
            Col::Age | Col::Note => "",
            Col::Task => "TASK",
            Col::Repo => "REPO",
            Col::Branch => "BRANCH",
            Col::Agent => "AGENT",
            Col::Ctx => "CTX",
        }
    }

    /// Fixed widths, so that the columns do not move as states change.
    fn fixed_width(self) -> Option<u16> {
        match self {
            Col::Status => Some(10),                    // "? question"
            Col::Age => Some(4),                        // "999d"
            Col::Ctx => Some(5 + CONTEXT_GAUGE as u16), // "100% " and the gauge
            _ => None,
        }
    }
}

/// Draw the rows of `app` (at least one) in `area`, keeping the selected
/// task's rows (its group's header when it is the first in it, its
/// members') in view.
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let t = theme::get();
    // The first column marks the selected row.
    let width = area.width.saturating_sub(MARK);
    let now = crate::task::now();
    let rows = app.rows();
    let focused = app.focused_member().map(|(_, m)| m.id().to_string());
    let selected = app.selected_task().map(|t| t.name.clone());

    let mut cols = vec![Col::Status, Col::Age, Col::Task];
    if app.order == Order::Attention {
        cols.push(Col::Repo);
    }
    cols.extend([Col::Branch, Col::Agent, Col::Ctx]);
    if width >= NOTE_FROM {
        cols.push(Col::Note);
    }
    let cell = |row: &Row, col| match *row {
        Row::Group(_) => Line::default(),
        Row::Task(task) => task_cell(col, task, app, now),
        Row::Member { task, member, last } => {
            let marked = focused.as_deref() == Some(member.id());
            member_cell(col, task, member, last, marked, app, now)
        }
    };
    let mut cells: Vec<Vec<Line<'static>>> = rows
        .iter()
        .map(|row| cols.iter().map(|&col| cell(row, col)).collect())
        .collect();

    let widths = fit(&mut cols, &mut cells, width);
    let task_width = cols
        .iter()
        .position(|&c| c == Col::Task)
        .map_or(0, |i| widths[i]);
    let note_width = width.saturating_sub(
        widths.iter().sum::<u16>() + SPACING * (cols.len() as u16).saturating_sub(1),
    );

    let header =
        TableRow::new(cols.iter().map(|c| Cell::from(c.header()))).style(Style::new().fg(t.muted));
    let table_rows: Vec<TableRow> = rows
        .iter()
        .zip(cells)
        .map(|(row, cells)| match row {
            Row::Group(task) => {
                // The repositories, and a rule to the list's edge.
                let name = app::repo_group(task);
                let rule = (width as usize).saturating_sub(Span::raw(name.as_str()).width() + 1);
                let mut spans = vec![
                    Span::styled(name, Style::new().fg(t.accent).add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                ];
                if t.rainbow {
                    spans.extend(theme::rainbow_spans(&"─".repeat(rule), 0, Style::new()));
                } else {
                    spans.push(Span::styled("─".repeat(rule), Style::new().fg(t.subtle)));
                }
                TableRow::new([Cell::from(Line::from(spans)).column_span(cols.len() as u16)])
            }
            _ => TableRow::new(cols.iter().zip(cells).map(|(&col, line)| {
                Cell::from(match col {
                    Col::Task => ellipsize(line, task_width),
                    Col::Note => ellipsize(line, note_width),
                    _ => line,
                })
            })),
        })
        .collect();

    let constraints: Vec<Constraint> = cols
        .iter()
        .zip(&widths)
        .map(|(&col, &w)| match col {
            Col::Note => Constraint::Fill(1),
            _ => Constraint::Length(w),
        })
        .collect();

    // The selected task's rows, from its group's header when it is the
    // first in it.
    let selected_row = rows
        .iter()
        .position(|r| matches!(r, Row::Task(t) if Some(&t.name) == selected.as_ref()));
    let block = selected_row.map(|i| {
        let start = if i > 0 && matches!(rows[i - 1], Row::Group(_)) {
            i - 1
        } else {
            i
        };
        let members = rows[i + 1..]
            .iter()
            .take_while(|r| matches!(r, Row::Member { .. }))
            .count();
        (start, i, i + 1 + members)
    });
    let height = area.height.saturating_sub(1) as usize;
    let mut offset = app.table_offset.min(rows.len().saturating_sub(height));
    if let Some((start, row, end)) = block {
        if start < offset {
            offset = start;
        } else if end > offset + height {
            // Taller than the list: from the task's row.
            offset = (end - height).min(row);
        }
    }
    // For the mouse: the task (its index among the visible ones) and the
    // member each row is about.
    let mut task_index = 0;
    let row_tasks: Vec<Option<(usize, Option<String>)>> = rows
        .iter()
        .map(|row| match row {
            Row::Group(_) => None,
            Row::Task(_) => {
                task_index += 1;
                Some((task_index - 1, None))
            }
            Row::Member { member, .. } => Some((task_index - 1, Some(member.id().to_string()))),
        })
        .collect();
    drop(rows);

    let table = Table::new(table_rows, constraints)
        .header(header)
        .column_spacing(SPACING)
        .row_highlight_style(t.selection)
        .highlight_symbol(Line::styled(
            "▌",
            Style::new().fg(if t.rainbow {
                theme::rainbow(0, app.spin.unwrap_or(0) * 2)
            } else {
                t.accent
            }),
        ))
        .highlight_spacing(HighlightSpacing::Always);
    let mut state = TableState::new()
        .with_offset(offset)
        .with_selected(selected_row);
    frame.render_stateful_widget(table, area, &mut state);
    app.table_offset = state.offset();
    app.hits.list = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    app.hits.rows = row_tasks
        .into_iter()
        .skip(app.table_offset)
        .take(height)
        .collect();
}

/// A task's or member's name, the characters the filter matched
/// highlighted.
fn name_line(name: &str, query: &str, style: Style) -> Line<'static> {
    match fuzzy::fuzzy(query, name).filter(|m| !m.positions.is_empty()) {
        Some(m) => fuzzy::highlight(
            name,
            &m.positions,
            style,
            style.fg(theme::get().accent).add_modifier(Modifier::BOLD),
        ),
        None => Line::styled(name.to_string(), style),
    }
}

/// Column widths for `width` columns: drops the columns that do not fit
/// (from `cols` and each row of `cells`). The task column is as wide as
/// its longest name while there is room, and takes the room left when no
/// note column does.
fn fit(cols: &mut Vec<Col>, cells: &mut [Vec<Line<'static>>], width: u16) -> Vec<u16> {
    let natural = |cols: &[Col], cells: &[Vec<Line>], i: usize| {
        cols[i].fixed_width().unwrap_or_else(|| {
            cells
                .iter()
                .map(|row| row[i].width() as u16)
                .chain([cols[i].header().len() as u16])
                .max()
                .unwrap_or(0)
        })
    };
    let others = |cols: &[Col], cells: &[Vec<Line>]| {
        let mut sum = SPACING * (cols.len() as u16).saturating_sub(1);
        for (i, col) in cols.iter().enumerate() {
            sum += match col {
                Col::Task => 0,
                Col::Note => NOTE_MIN,
                _ => natural(cols, cells, i),
            };
        }
        sum
    };
    let task = cols
        .iter()
        .position(|&c| c == Col::Task)
        .expect("task column");
    let wanted = natural(cols, cells, task).min(TASK_WANTED);
    for drop in DROP_ORDER {
        if others(cols, cells) + wanted <= width {
            break;
        }
        if let Some(i) = cols.iter().position(|&c| c == drop) {
            cols.remove(i);
            for row in cells.iter_mut() {
                row.remove(i);
            }
        }
    }
    let room = width.saturating_sub(others(cols, cells));
    // The list fills its width: the note column takes what is left, else
    // the task column does (the columns after it sit at the right edge).
    let task_width = if cols.contains(&Col::Note) {
        natural(
            cols,
            cells,
            cols.iter().position(|&c| c == Col::Task).unwrap(),
        )
        .min(room)
    } else {
        room
    };
    (0..cols.len())
        .map(|i| match cols[i] {
            Col::Task => task_width,
            Col::Note => 0,
            _ => natural(cols, cells, i),
        })
        .collect()
}

/// A task's row: its only member's, or for a task with several, its name
/// alone: a status belongs to an agent, and the members' rows below have
/// theirs.
fn task_cell(col: Col, task: &Task, app: &App, now: u64) -> Line<'static> {
    if let [member] = &task.members[..] {
        return match col {
            Col::Task => name_line(&task.name, app.filter.value(), Style::new()),
            Col::Repo => Line::raw(member.info.repo.clone()),
            _ => member_cell(col, task, member, false, false, app, now),
        };
    }
    match col {
        Col::Task => name_line(
            &task.name,
            app.filter.value(),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Col::Repo => Line::raw(app::repo_group(task)),
        _ => Line::default(),
    }
}

/// A member's cells. `last`: the last of its task's members' rows;
/// `marked`: the member the keys act on.
fn member_cell(
    col: Col,
    task: &Task,
    member: &Member,
    last: bool,
    marked: bool,
    app: &App,
    now: u64,
) -> Line<'static> {
    let dim = Style::new().fg(theme::get().muted);
    let agent = member.agent_info.as_ref();
    match col {
        Col::Status => status_cell(task.member_status(member), app.spin),
        Col::Age => age_cell(task, member, now),
        Col::Task => {
            let tree = if last { "└" } else { "├" };
            let (mark, style) = if marked {
                ("▸", Style::new().add_modifier(Modifier::BOLD))
            } else {
                (" ", Style::new())
            };
            let mut line = name_line(&member.info.name, app.filter.value(), style);
            line.spans.splice(
                0..0,
                [
                    Span::styled(format!(" {tree}"), Style::new().fg(theme::get().subtle)),
                    Span::styled(mark, style),
                ],
            );
            line
        }
        Col::Repo => Line::default(),
        Col::Branch => branch_cell(member, app.branches.get(member.id())),
        Col::Agent => match agent {
            Some(a) => Line::raw(a.provider.clone()),
            None => Line::styled("-", dim),
        },
        Col::Ctx => agent
            .and_then(|a| a.context_percent)
            .map(context_display)
            .unwrap_or_default(),
        Col::Note => note_cell(member),
    }
}

/// A status: its mark and name; a working agent's mark turns (`spin`: the
/// frame).
fn status_cell(status: TaskStatus, spin: Option<usize>) -> Line<'static> {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let (text, color) = status_display(&status);
    if let (TaskStatus::Run, Some(frame)) = (status, spin) {
        let name = text.split_once(' ').map_or(text, |(_, name)| name);
        return Line::styled(
            format!("{} {name}", FRAMES[frame % FRAMES.len()]),
            Style::new().fg(if theme::get().rainbow {
                theme::rainbow(0, frame * 3)
            } else {
                color
            }),
        );
    }
    let style = Style::new().fg(color);
    match status {
        // What needs the user stands out.
        TaskStatus::Block | TaskStatus::Question | TaskStatus::Error | TaskStatus::Ready => {
            Line::styled(text, style.add_modifier(Modifier::BOLD))
        }
        _ => Line::styled(text, style),
    }
}

/// How long the member has been in its state: its agent's, else since the
/// task's last activity (idle for so long).
fn age_cell(task: &Task, member: &Member, now: u64) -> Line<'static> {
    let since = match &member.agent_info {
        Some(agent) => agent.state_since,
        None => task.last_activity,
    };
    match since {
        Some(since) => Line::styled(
            age(now.saturating_sub(since)),
            Style::new().fg(theme::get().muted),
        )
        .right_aligned(),
        None => Line::default(),
    }
}

/// The member's branch against the default branch: `↑` commits ahead, `↓`
/// behind, `*` paths with uncommitted changes, `=` none of them; `…` until
/// git was asked, `?` when it could not tell. Nothing for a gone worktree
/// (its status says so).
fn branch_cell(member: &Member, observation: Option<&Observation>) -> Line<'static> {
    let dim = Style::new().fg(theme::get().muted);
    if member.is_unhealthy() {
        return Line::default();
    }
    let state = match observation.and_then(|o| o.state.as_ref()) {
        None => return Line::styled("…", dim),
        // A borrowed folder outside git: nothing to say.
        Some(Err(_)) if member.info.borrowed => return Line::styled("-", dim),
        Some(Err(_)) => return Line::styled("?", dim),
        Some(Ok(state)) => state,
    };
    let mut parts = Vec::new();
    if state.ahead > 0 {
        parts.push(Span::styled(
            format!("↑{}", state.ahead),
            Style::new().fg(theme::get().ok),
        ));
    }
    if state.behind > 0 {
        parts.push(Span::styled(format!("↓{}", state.behind), dim));
    }
    if state.uncommitted > 0 {
        parts.push(Span::styled(
            format!("*{}", state.uncommitted),
            Style::new().fg(theme::get().warn),
        ));
    }
    if parts.is_empty() {
        return Line::styled("=", dim);
    }
    let mut spans = Vec::new();
    for part in parts {
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans.push(part);
    }
    Line::from(spans)
}

/// What the member waits on, on one line: the permission or question, why
/// its turn failed or what auto mode refused, the start of its last
/// message; what is wrong with its worktree.
fn note_cell(member: &Member) -> Line<'static> {
    let dim = Style::new().fg(theme::get().muted);
    match &member.agent_info {
        Some(agent) => {
            if let Some((text, color)) = agent_detail_line(agent.state, &agent.detail) {
                return Line::styled(one_line(&text), Style::new().fg(color));
            }
            match (&agent.detail.last_message, agent.state) {
                (Some(message), AgentState::Ready) => {
                    Line::styled(one_line(first_line(message)), dim)
                }
                _ => Line::default(),
            }
        }
        None => match crate::health::problem(&member.info) {
            Some(problem) => Line::styled(problem, Style::new().fg(theme::get().bad)),
            None => Line::default(),
        },
    }
}

/// `line` cut to `width` columns, ending with `…` when cut.
fn ellipsize(line: Line<'static>, width: u16) -> Line<'static> {
    let width = width as usize;
    if line.width() <= width {
        return line;
    }
    let mut left = width.saturating_sub(1);
    let mut spans = Vec::new();
    let mut last_style = Style::new();
    for span in line.spans {
        last_style = span.style;
        let mut text = String::new();
        for c in span.content.chars() {
            let w = Span::raw(c.to_string()).width();
            if w > left {
                left = 0;
                break;
            }
            left -= w;
            text.push(c);
        }
        spans.push(Span::styled(text, span.style));
        if left == 0 {
            break;
        }
    }
    if width > 0 {
        spans.push(Span::styled("…", last_style));
    }
    Line::from(spans).style(line.style)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ellipsizing_keeps_styles_and_width() {
        let line = Line::from(vec![
            Span::raw("ab"),
            Span::styled("cdef", Style::new().fg(theme::get().bad)),
        ]);
        let cut = ellipsize(line.clone(), 4);
        assert_eq!(cut.width(), 4);
        assert_eq!(cut.to_string(), "abc…");
        assert_eq!(cut.spans.last().unwrap().style.fg, Some(theme::get().bad));
        assert_eq!(ellipsize(line.clone(), 6).to_string(), "abcdef");
        assert_eq!(ellipsize(Line::raw("漢字漢字"), 5).to_string(), "漢字…");
    }
}
