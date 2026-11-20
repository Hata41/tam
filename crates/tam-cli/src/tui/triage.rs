//! What the pane shows for a member without an agent, so without a screen:
//! how its last run ended or what is wrong with its worktree, its branch
//! and its latest messages (see `ui::render_pane`).

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tam_proto::{MemberHealth, MessageInfo, RunInfo};

use crate::task::{Member, Task, TaskStatus};

use super::app::TaskDetail;
use super::branch::{BranchState, Observation};
use super::theme;
use super::ui::status_display;

/// Latest messages shown.
const MESSAGES: usize = 3;
/// Body rows kept before the branch and messages when the pane is short.
const BODY_MIN: usize = 2;

/// The view's parts, before they are fitted to the pane.
pub struct Triage {
    /// The member's status, and how its last run ended.
    pub headline: Line<'static>,
    /// What is wrong with its worktree: wrapped to the pane's width when
    /// fitted.
    pub body: Vec<(String, Style)>,
    /// Its branch and changes.
    pub branch: Option<Line<'static>>,
    /// Its latest messages, oldest first.
    pub messages: Vec<Line<'static>>,
}

/// The view of `member` of `task`, which has no agent. `detail` is the
/// task's runs and messages, `branch` what git said of the member's
/// worktree; `now` is a Unix timestamp (seconds).
pub fn triage(
    task: &Task,
    member: &Member,
    detail: Option<&TaskDetail>,
    branch: Option<&Observation>,
    now: u64,
) -> Triage {
    let runs = detail.map_or(&[][..], |d| &d.runs[..]);
    let (headline, body) = idle_lines(task, member, runs, now);
    let messages = detail
        .map(|d| message_lines(member.id(), &d.messages, now))
        .unwrap_or_default();
    Triage {
        headline,
        body,
        branch: branch_line(member, branch),
        messages,
    }
}

/// The view in at most `rows` rows of `width` columns. The headline
/// comes first, then the first rows of the body, the branch, the
/// messages, and the rest of the body.
pub fn fit(triage: Triage, width: u16, rows: u16) -> Vec<Line<'static>> {
    let rows = rows as usize;
    let indent = 2;
    let body: Vec<(String, Style)> = triage
        .body
        .iter()
        .flat_map(|(text, style)| {
            wrap(text, (width as usize).saturating_sub(indent).max(1))
                .into_iter()
                .map(move |line| (line, *style))
        })
        .collect();

    let headline = rows.min(1);
    let mut left = rows - headline;
    let body_min = body.len().min(BODY_MIN).min(left);
    left -= body_min;
    let branch = usize::from(triage.branch.is_some()).min(left);
    left -= branch;
    let messages = triage.messages.len().min(MESSAGES).min(left);
    left -= messages;
    let body_rows = body_min + (body.len() - body_min).min(left);

    let mut lines: Vec<Line> = Vec::new();
    if headline > 0 {
        lines.push(triage.headline);
    }
    let cut = body_rows < body.len();
    for (i, (text, style)) in body.into_iter().take(body_rows).enumerate() {
        let text = if cut && i + 1 == body_rows {
            ellipsize(&text, (width as usize).saturating_sub(indent))
        } else {
            text
        };
        lines.push(Line::styled(format!("{:indent$}{text}", ""), style));
    }
    lines.extend(triage.branch.into_iter().take(branch));
    let skip = triage.messages.len() - messages;
    lines.extend(triage.messages.into_iter().skip(skip));
    lines
}

/// Headline and body of a member without an agent: its last run, or what
/// is wrong with its worktree.
fn idle_lines(
    task: &Task,
    member: &Member,
    runs: &[RunInfo],
    now: u64,
) -> (Line<'static>, Vec<(String, Style)>) {
    let status = task.member_status(member);
    let (_, color) = status_display(&status);
    let mut spans = vec![Span::styled(
        format!("{} {status}", glyph(status)),
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    )];
    let dim = Style::new().fg(theme::get().muted);
    let mut body = Vec::new();
    if let Some(problem) = crate::health::problem(&member.info) {
        body.push((problem, Style::new().fg(theme::get().bad)));
        let fix = if crate::health::repairable(&member.info) {
            "d: recreate the worktree, or drop the task"
        } else {
            "d: drop the task"
        };
        body.push((fix.into(), dim));
    }
    let last = runs
        .iter()
        .filter(|r| r.member == member.id())
        .max_by_key(|r| r.started_at);
    let about = match last {
        Some(run) => match run.ended_at {
            Some(ended) => {
                let how = match run.exit_code {
                    Some(0) => String::new(),
                    Some(-1) => ", stopped".into(),
                    Some(code) => format!(", exit {code}"),
                    None => String::new(),
                };
                format!(
                    "no agent · {} ended {} ago{how}",
                    run.provider,
                    age(now.saturating_sub(ended))
                )
            }
            None => format!(
                "no agent · {} started {} ago",
                run.provider,
                age(now.saturating_sub(run.started_at))
            ),
        },
        None if task.run_count == 0 => "no agent · never ran".into(),
        None => "no agent".into(),
    };
    spans.push(Span::raw(format!(" · {about}")));
    (Line::from(spans), body)
}

/// The member's branch, how it compares with the default branch and what it
/// changed; `None` when its worktree is gone (the body says so).
fn branch_line(member: &Member, observation: Option<&Observation>) -> Option<Line<'static>> {
    if member.is_unhealthy() {
        return None;
    }
    let dim = Style::new().fg(theme::get().muted);
    if member.info.borrowed {
        // What git says of it, if it is in git; no branch of its own.
        let mut spans = vec![Span::styled("borrowed folder, left as it is", dim)];
        if let Some(Ok(state)) = observation.and_then(|o| o.state.as_ref()) {
            for part in branch_facts(state) {
                spans.push(Span::styled(" · ", dim));
                spans.extend(part);
            }
        }
        return Some(Line::from(spans));
    }
    let branch = &member.info.branch;
    let head = match &member.info.health {
        Some(MemberHealth::Ok { branch: Some(b) }) if b != branch => Span::styled(
            format!("on {b}, not {branch}"),
            Style::new().fg(theme::get().warn),
        ),
        Some(MemberHealth::Ok { branch: None }) => Span::styled(
            format!("detached HEAD (branch {branch})"),
            Style::new().fg(theme::get().warn),
        ),
        _ => Span::styled(branch.clone(), Style::new().add_modifier(Modifier::BOLD)),
    };
    let mut spans = vec![Span::styled("branch ", dim), head];
    match observation.and_then(|o| o.state.as_ref()) {
        Some(Ok(state)) => {
            for part in branch_facts(state) {
                spans.push(Span::styled(" · ", dim));
                spans.extend(part);
            }
        }
        Some(Err(why)) => spans.push(Span::styled(format!(" · {}", clean(why)), dim)),
        None => spans.push(Span::styled(" · …", dim)),
    }
    Some(Line::from(spans))
}

/// What git said, a group of spans per fact.
fn branch_facts(state: &BranchState) -> Vec<Vec<Span<'static>>> {
    let t = theme::get();
    let mut parts = Vec::new();
    if let Some(base) = &state.base {
        parts.push(vec![Span::raw(match (state.ahead, state.behind) {
            (0, 0) => format!("even with {base}"),
            (ahead, 0) => format!("{ahead} ahead of {base}"),
            (0, behind) => format!("{behind} behind {base}"),
            (ahead, behind) => format!("{ahead} ahead, {behind} behind {base}"),
        })]);
    }
    if state.files > 0 {
        let files = if state.files == 1 { "file" } else { "files" };
        parts.push(vec![
            Span::raw(format!("{} {files} ", state.files)),
            Span::styled(format!("+{}", state.insertions), Style::new().fg(t.ok)),
            Span::raw(" "),
            Span::styled(format!("-{}", state.deletions), Style::new().fg(t.bad)),
        ]);
    }
    parts.push(vec![match state.uncommitted {
        0 => Span::raw("clean"),
        n => Span::styled(format!("{n} uncommitted"), Style::new().fg(t.warn)),
    }]);
    parts
}

/// The member's latest messages, oldest first: `← sender` for one it got,
/// `→ recipient` for one it sent.
fn message_lines(id: &str, messages: &[MessageInfo], now: u64) -> Vec<Line<'static>> {
    let dim = Style::new().fg(theme::get().muted);
    let mut ours: Vec<&MessageInfo> = messages
        .iter()
        .filter(|m| m.to == id || m.from.as_deref() == Some(id))
        .collect();
    ours.sort_by_key(|m| m.sent_at);
    let skip = ours.len().saturating_sub(MESSAGES);
    ours.into_iter()
        .skip(skip)
        .map(|m| {
            let (arrow, who) = if m.to == id {
                ("←", &m.from_label)
            } else {
                ("→", &m.to_label)
            };
            let mut spans = vec![
                Span::styled(format!("{:>4} ", age(now.saturating_sub(m.sent_at))), dim),
                Span::styled(
                    format!("{arrow} {who}: "),
                    Style::new().fg(theme::get().accent),
                ),
            ];
            if m.delivered_at.is_none() {
                spans.push(Span::styled(
                    "(queued) ",
                    Style::new().fg(theme::get().warn),
                ));
            }
            spans.push(Span::raw(one_line(&m.text)));
            Line::from(spans)
        })
        .collect()
}

/// A status's glyph (as in the task list).
fn glyph(status: TaskStatus) -> &'static str {
    status.indicator().split(' ').next().unwrap_or("")
}

/// How long `secs` is, in its largest unit: `45s`, `3m`, `2h`, `5d`.
pub fn age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// Control characters (from an agent's text) as spaces: a terminal would
/// act on them.
pub(super) fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect()
}

/// The text on one line, spaces collapsed.
pub(super) fn one_line(text: &str) -> String {
    clean(text).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Columns `text` takes on a terminal.
pub(super) fn width_of(text: &str) -> usize {
    Span::raw(text).width()
}

/// `text` in rows of at most `width` columns, cut between words (inside a
/// word longer than a row). Blank lines are kept, one at most, and not at
/// either end.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for line in clean(text).lines() {
        if line.trim().is_empty() {
            if rows.last().is_some_and(|r| !r.is_empty()) {
                rows.push(String::new());
            }
            continue;
        }
        let mut row = String::new();
        for word in line.split_whitespace() {
            let room = if row.is_empty() {
                width
            } else {
                width.saturating_sub(width_of(&row) + 1)
            };
            if width_of(word) <= room {
                if !row.is_empty() {
                    row.push(' ');
                }
                row.push_str(word);
                continue;
            }
            if !row.is_empty() {
                rows.push(std::mem::take(&mut row));
            }
            // A word longer than a row: cut it.
            for c in word.chars() {
                let mut buf = [0; 4];
                if width_of(&row) + width_of(c.encode_utf8(&mut buf)) > width && !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
                row.push(c);
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows
}

/// `text` marked as followed by more, within `width` columns: ` …` after
/// it, or cut to end with `…`.
fn ellipsize(text: &str, width: usize) -> String {
    if width_of(text) + 2 <= width {
        return format!("{text} …");
    }
    let mut out = String::new();
    for c in text.chars() {
        let mut buf = [0; 4];
        if width_of(&out) + width_of(c.encode_utf8(&mut buf)) + 1 > width {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("\n\na\n\n\n\nb\n\n", 10), ["a", "", "b"]);
        assert_eq!(wrap("x 漢字漢字", 4), ["x", "漢字", "漢字"]);
        assert_eq!(wrap("bell\x07 esc\x1b[31m", 20), ["bell esc [31m"]);
    }

    #[test]
    fn ellipsizing() {
        assert_eq!(ellipsize("abcdefgh", 5), "abcd…");
        assert_eq!(ellipsize("abc", 5), "abc …");
        assert_eq!(ellipsize("abcd", 5), "abcd…");
    }

    #[test]
    fn ages() {
        assert_eq!(age(0), "0s");
        assert_eq!(age(59), "59s");
        assert_eq!(age(60), "1m");
        assert_eq!(age(7200), "2h");
        assert_eq!(age(86400 * 3), "3d");
    }
}
