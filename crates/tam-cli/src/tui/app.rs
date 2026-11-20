use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::KeyEvent;

use tam_proto::{AgentInfo, MessageInfo, RunInfo, TaskInfo};

use crate::config::CustomCommand;
use crate::task::{Member, Task};

use ratatui::layout::Rect;

use super::answer::AnswerDialog;
use super::branch::{self, BranchState, Observation};
use super::fuzzy::fuzzy;
use super::input::TextInput;
use super::peek::Peek;

pub enum Mode {
    Normal,
    NewTaskPickProject(PickerState),
    /// The task's name, for worktrees in these repositories (main
    /// checkouts), or for the folder picked as it is (`borrow`). The name of
    /// an existing task adds them to it.
    NewTaskEnterName {
        project_dirs: Vec<PathBuf>,
        name: TextInput,
        /// The one directory picked, which the task can borrow: work in it
        /// as it is, no worktree. `None` when several were picked.
        folder: Option<PathBuf>,
        /// Borrow `folder`: always for a folder outside git (no
        /// `project_dirs`), else tab switches.
        borrow: bool,
    },
    SpawnEnterPath(TextInput),
    ConfirmDropTask {
        name: String,
        /// The focused member, when the task has others: it can be dropped
        /// alone.
        member: Option<MemberChoice>,
    },
    /// The daemon refused the drop: `report` tells why (work it would lose).
    ConfirmForceDrop {
        /// What is dropped, as shown: the task name or a member label.
        name: String,
        /// What `DropTask` names: the task, or the member's id.
        target: String,
        delete_branch: bool,
        report: Vec<String>,
    },
    /// A message too long for the status line (e.g. a failed drop); any key
    /// closes it.
    Notice {
        title: String,
        lines: Vec<String>,
    },
    /// A task with a worktree gone or unusable (`lines` say what): recreate
    /// the worktrees that can be (`repairable`), or drop the task. Deleting
    /// branches is offered only when some worktree is `in_place`: a drop keeps
    /// the branch of a gone one.
    ConfirmRepairOrDrop {
        name: String,
        lines: Vec<String>,
        repairable: bool,
        in_place: bool,
        /// The focused member, when the task has others: it can be dropped
        /// alone.
        member: Option<MemberChoice>,
    },
    /// The keys, custom commands included; any key closes it.
    Help,
    /// The request a member's agent waits on, to answer (`a`).
    Answer(AnswerDialog),
    /// The pane has the keyboard: what is typed goes to this agent, until
    /// ctrl-] (see `keys`). The dashboard is drawn as in `Normal`.
    Terminal {
        agent: String,
    },
}

/// How the task list is ordered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Order {
    /// Under a header row per repository (the set of them for a task in
    /// several), the most urgent first in each.
    #[default]
    Repo,
    /// The most urgent first, all repositories together.
    Attention,
}

/// A row of the task list.
#[derive(Debug, Clone, Copy)]
pub enum Row<'a> {
    /// The repositories of the tasks below it.
    Group(&'a Task),
    /// A task: the row of its only member, or the head of its members'
    /// rows. The only rows that can be selected.
    Task(&'a Task),
    /// A member of a task with several, below the task's row.
    Member {
        task: &'a Task,
        member: &'a Member,
        last: bool,
    },
}

/// A member a drop dialog offers to drop alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberChoice {
    pub id: String,
    /// How to name it (`<task>/<member>`).
    pub label: String,
}

/// The member of the selected task that the detail pane shows and member
/// actions (attach, stop, run) apply to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    pub task: String,
    pub member: String,
}

/// What the detail pane shows of the selected task beyond the task list,
/// fetched when the task is selected.
#[derive(Debug, Default)]
pub struct TaskDetail {
    pub task: String,
    /// The task's agent runs, oldest first.
    pub runs: Vec<RunInfo>,
    /// Messages to or from its members, oldest first.
    pub messages: Vec<MessageInfo>,
    /// To fetch again (e.g. a run started or ended).
    pub stale: bool,
}

impl Mode {
    /// Name a new task: worktrees in the repositories `project_dirs`, or the
    /// one directory picked, `folder`, borrowed (the only way outside git).
    pub fn new_task(project_dirs: Vec<PathBuf>, folder: Option<PathBuf>) -> Self {
        Mode::NewTaskEnterName {
            borrow: project_dirs.is_empty(),
            project_dirs,
            name: TextInput::default(),
            folder,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PickerItem {
    pub display: String,
    pub id: String,
}

pub struct PickerState {
    pub title: String,
    pub items: Vec<PickerItem>,
    pub filter: TextInput,
    /// Index in the filtered items.
    pub selected: usize,
    /// Ids of the items picked with tab, in the order picked.
    pub marked: Vec<String>,
    /// More items are on their way (projects being discovered).
    pub loading: bool,
    /// The first item on screen (kept between draws).
    pub offset: usize,
}

impl PickerState {
    pub fn new(title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        Self {
            title: title.into(),
            items,
            filter: TextInput::default(),
            selected: 0,
            marked: Vec::new(),
            loading: false,
            offset: 0,
        }
    }

    /// Replace the items, keeping the highlighted one (by id) when it is
    /// still there, and the filter and picks.
    pub fn set_items(&mut self, items: Vec<PickerItem>) {
        let selected = self.selected_item().map(|item| item.id.clone());
        self.items = items;
        self.selected = selected
            .and_then(|id| self.filtered_items().iter().position(|item| item.id == id))
            .unwrap_or(0);
    }

    /// Apply an editing key to the filter: `false` when it is not one.
    pub fn edit(&mut self, key: KeyEvent) -> bool {
        let before = self.filter.value().to_string();
        if !self.filter.handle(key) {
            return false;
        }
        if self.filter.value() != before {
            self.selected = 0;
        }
        true
    }

    /// Pick the highlighted item (or unpick it), then move down. Items whose
    /// id starts with `__` other than `__cwd__` are actions: not picked.
    pub fn toggle_mark(&mut self) {
        let Some(id) = self.selected_item().map(|item| item.id.clone()) else {
            return;
        };
        if id.starts_with("__") && id != "__cwd__" {
            return;
        }
        match self.marked.iter().position(|m| *m == id) {
            Some(pos) => {
                self.marked.remove(pos);
            }
            None => self.marked.push(id),
        }
        self.select_next();
    }

    /// The items the filter matches, the best matches first (in their
    /// order when they match as well), with the characters matched.
    pub fn filtered(&self) -> Vec<(&PickerItem, Vec<usize>)> {
        if self.filter.is_empty() {
            return self.items.iter().map(|item| (item, Vec::new())).collect();
        }
        let mut matched: Vec<(&PickerItem, i32, Vec<usize>)> = self
            .items
            .iter()
            .filter_map(|item| {
                fuzzy(self.filter.value(), &item.display).map(|m| (item, m.score, m.positions))
            })
            .collect();
        // Stable: equal scores keep the items' order.
        matched.sort_by_key(|(_, score, _)| std::cmp::Reverse(*score));
        matched
            .into_iter()
            .map(|(item, _, positions)| (item, positions))
            .collect()
    }

    pub fn filtered_items(&self) -> Vec<&PickerItem> {
        self.filtered().into_iter().map(|(item, _)| item).collect()
    }

    pub fn selected_item(&self) -> Option<&PickerItem> {
        let filtered = self.filtered_items();
        filtered.get(self.selected).copied()
    }

    pub fn select_next(&mut self) {
        let count = self.filtered_items().len();
        if count > 0 {
            self.selected = (self.selected + 1).min(count - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

/// Where the last draw put what the mouse can act on.
#[derive(Debug, Default)]
pub struct Hits {
    /// The task list's rows (below its column names).
    pub list: Rect,
    /// For each row of `list` from its top: the visible task it belongs
    /// to (its index), and the member it is about.
    pub rows: Vec<Option<(usize, Option<String>)>>,
    /// The detail pane.
    pub pane: Option<Rect>,
    /// The project picker's items, and the index of the first one shown.
    pub picker: Option<(Rect, usize)>,
}

/// How long without a key before the dashboard takes keys again after the
/// pane lost the keyboard (see `App::stop_typing`).
const TYPING_PAUSE: std::time::Duration = std::time::Duration::from_millis(700);

pub struct App {
    pub tasks: Vec<Task>,
    pub selected: usize,
    pub status: Option<(String, Instant)>,
    pub mode: Mode,
    /// The preview pane, when shown.
    pub peek: Peek,
    /// The running agents' spinner: its frame, `None` for a still mark
    /// (tests).
    pub spin: Option<usize>,
    /// The pane lost the keyboard while one was typing (its agent exited,
    /// say): keys are dropped until typing pauses, so that what was meant
    /// for the agent does not run as the dashboard's own keys.
    pub dropping_keys_until: Option<std::time::Instant>,
    pub filter: TextInput,
    pub filter_active: bool,
    pub commands: Vec<CustomCommand>,
    /// See [`App::focused_member`].
    pub focus: Option<Focus>,
    /// A first `g` was pressed: a second one goes to the first task.
    pub pending_g: bool,
    /// The selected task's runs and messages, while the pane is shown.
    pub detail: Option<TaskDetail>,
    /// Git's view of members' worktrees, by member id.
    pub branches: HashMap<String, Observation>,
    pub order: Order,
    /// The first row of the task list on screen (kept between draws, so
    /// the list scrolls only when the selection would leave it).
    pub table_offset: usize,
    /// The projects found under `$HOME` last time (for the picker, while
    /// they are looked for again).
    pub projects: Option<Vec<PickerItem>>,
    /// Projects are being looked for.
    pub discovering: bool,
    pub hits: Hits,
}

impl App {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            selected: 0,
            status: None,
            mode: Mode::Normal,
            peek: Peek::default(),
            spin: None,
            dropping_keys_until: None,
            filter: TextInput::default(),
            filter_active: false,
            commands: Vec::new(),
            focus: None,
            pending_g: false,
            detail: None,
            branches: HashMap::new(),
            order: Order::default(),
            table_offset: 0,
            projects: None,
            discovering: false,
            hits: Hits::default(),
        }
    }

    /// Replace the task list, preserving selection.
    pub fn set_tasks(&mut self, tasks: Vec<Task>) {
        let prev_name = self.selected_task().map(|t| t.name.clone());
        self.tasks = tasks;
        self.sort_tasks();
        self.reselect(prev_name);
    }

    fn sort_tasks(&mut self) {
        match self.order {
            Order::Repo => self.tasks.sort_by_key(|t| {
                (
                    repo_group(t).to_lowercase(),
                    t.status().sort_priority(),
                    t.name.to_lowercase(),
                )
            }),
            Order::Attention => self
                .tasks
                .sort_by_key(|t| (t.status().sort_priority(), t.name.to_lowercase())),
        }
    }

    /// Group the list by repository, or show the most urgent first.
    pub fn toggle_order(&mut self) {
        let prev = self.selected_task().map(|t| t.name.clone());
        self.order = match self.order {
            Order::Repo => Order::Attention,
            Order::Attention => Order::Repo,
        };
        self.sort_tasks();
        self.reselect(prev);
    }

    /// The task list's rows: the visible tasks in order, a header row
    /// before each repository's tasks (when grouped), a row per member
    /// below a task with several.
    pub fn rows(&self) -> Vec<Row<'_>> {
        let mut rows = Vec::new();
        let mut group: Option<String> = None;
        for task in self.visible_tasks() {
            if self.order == Order::Repo {
                let name = repo_group(task);
                if group.as_ref() != Some(&name) {
                    rows.push(Row::Group(task));
                    group = Some(name);
                }
            }
            rows.push(Row::Task(task));
            if task.members.len() > 1 {
                let count = task.members.len();
                rows.extend(
                    task.members
                        .iter()
                        .enumerate()
                        .map(|(i, member)| Row::Member {
                            task,
                            member,
                            last: i + 1 == count,
                        }),
                );
            }
        }
        rows
    }

    /// The tasks the filter matches (by name, or a member's name,
    /// repository or branch), in the list's order.
    pub fn visible_tasks(&self) -> Vec<&Task> {
        let query = self.filter.value();
        if query.is_empty() {
            return self.tasks.iter().collect();
        }
        let matches = |text: &str| fuzzy(query, text).is_some();
        self.tasks
            .iter()
            .filter(|t| {
                matches(&t.name)
                    || t.members.iter().any(|m| {
                        matches(&m.info.name) || matches(&m.info.repo) || matches(&m.info.branch)
                    })
            })
            .collect()
    }

    /// Empty the filter, the task found with it still selected.
    pub fn clear_filter(&mut self) {
        let prev = self.selected_task().map(|t| t.name.clone());
        self.filter.clear();
        self.reselect(prev);
    }

    /// The project list was looked for: keep it, and show it in the picker
    /// if it is open.
    pub fn set_projects(&mut self, projects: Vec<PickerItem>) {
        self.discovering = false;
        if let Mode::NewTaskPickProject(picker) = &mut self.mode {
            let mut items: Vec<PickerItem> = picker
                .items
                .iter()
                .filter(|item| item.id.starts_with("__"))
                .cloned()
                .collect();
            items.extend(projects.iter().cloned());
            picker.set_items(items);
            picker.loading = false;
        }
        self.projects = Some(projects);
    }

    /// Select the visible task at `index` (e.g. clicked), focusing
    /// `member` when the pane shows one.
    pub fn select_at(&mut self, index: usize, member: Option<String>) {
        if index >= self.visible_tasks().len() {
            return;
        }
        self.selected = index;
        self.sync_focus();
        let Some(member) = member else {
            return;
        };
        if let Some(task) = self.selected_task().map(|t| t.name.clone()) {
            self.focus = Some(Focus { task, member });
        }
    }

    pub fn select_next(&mut self) {
        let count = self.visible_tasks().len();
        if count > 0 {
            self.selected = (self.selected + 1).min(count - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.visible_tasks().len().saturating_sub(1);
    }

    pub fn selected_task(&self) -> Option<&Task> {
        self.visible_tasks().get(self.selected).copied()
    }

    fn clamp_selection(&mut self) {
        let count = self.visible_tasks().len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(count - 1);
        }
    }

    /// Add a task, or replace the one with the same name.
    pub fn upsert_task(&mut self, task: Task) {
        let prev = self.selected_task().map(|t| t.name.clone());
        match self.tasks.iter_mut().find(|t| t.name == task.name) {
            Some(existing) => *existing = task,
            None => self.tasks.push(task),
        }
        self.sort_tasks();
        self.reselect(prev);
    }

    /// Apply the daemon's latest summary of a task.
    pub fn update_task_info(&mut self, info: TaskInfo) {
        let prev = self.selected_task().map(|t| t.name.clone());
        if let Some(detail) = self.detail.as_mut().filter(|d| d.task == info.name) {
            detail.stale = true;
        }
        if let Some(task) = self.tasks.iter_mut().find(|t| t.name == info.name) {
            task.update_info(info);
            self.sort_tasks();
            self.reselect(prev);
        }
    }

    pub fn remove_task(&mut self, name: &str) {
        let prev = self.selected_task().map(|t| t.name.clone());
        self.tasks.retain(|t| t.name != name);
        self.reselect(prev);
    }

    /// Keep the selection on the named task if it is still visible.
    fn reselect(&mut self, name: Option<String>) {
        if let Some(name) = name {
            if let Some(pos) = self.visible_tasks().iter().position(|t| t.name == name) {
                self.selected = pos;
                return;
            }
        }
        self.clamp_selection();
    }

    /// The answer dialog's request is no longer the one its agent waits on
    /// (answered or changed meanwhile, or the agent is gone): it can take
    /// no answer.
    pub fn answer_stale(&self) -> bool {
        let Mode::Answer(dialog) = &self.mode else {
            return false;
        };
        let current = self
            .find_member(&dialog.agent)
            .and_then(|(_, m)| m.agent_info.as_ref())
            .and_then(|a| a.detail.ask.as_ref());
        current.is_none_or(|ask| ask.id != dialog.ask.id)
    }

    /// The member an agent runs for (agent ids are member ids).
    fn member_mut(&mut self, id: &str) -> Option<&mut Member> {
        self.tasks.iter_mut().find_map(|t| t.member_mut(id))
    }

    /// A member and its task, by member id.
    pub fn find_member(&self, id: &str) -> Option<(&Task, &Member)> {
        self.tasks.iter().find_map(|t| t.member(id).map(|m| (t, m)))
    }

    /// Update the agent info of the member it runs for.
    pub fn update_agent(&mut self, info: AgentInfo) {
        let selected = self.selected_task().map(|t| t.name.clone());
        if let Some(member) = self.member_mut(&info.id) {
            member.agent_info = Some(info);
            self.sort_tasks();
            // The selection stays on its task, wherever the order puts it.
            self.reselect(selected);
        }
    }

    pub fn update_state(&mut self, id: &str, new_state: tam_proto::AgentState) {
        let selected = self.selected_task().map(|t| t.name.clone());
        if let Some(member) = self.member_mut(id) {
            if let Some(ref mut info) = member.agent_info {
                // Already taken from the daemon's info (with its time)
                // when a hook changed it.
                if info.state != new_state {
                    info.state = new_state;
                    info.state_since = Some(crate::task::now());
                }
            }
            self.sort_tasks();
            self.reselect(selected);
        }
        // A turn ended or began: its changes are worth a new look.
        self.branch_changed(id);
    }

    pub fn update_context(&mut self, id: &str, context_percent: u8) {
        if let Some(member) = self.member_mut(id) {
            if let Some(ref mut info) = member.agent_info {
                info.context_percent = Some(context_percent);
            }
        }
    }

    /// Remove agent info from a member (agent exited, member becomes idle).
    pub fn remove_agent(&mut self, id: &str) {
        let selected = self.selected_task().map(|t| t.name.clone());
        if let Some(member) = self.member_mut(id) {
            member.agent_info = None;
            self.sort_tasks();
            self.reselect(selected);
        }
        self.clamp_selection();
        self.branch_changed(id);
    }

    pub fn set_status(&mut self, msg: String, duration: std::time::Duration) {
        self.status = Some((msg, Instant::now() + duration));
    }

    pub fn status_message(&mut self) -> Option<&str> {
        if let Some((_, expiry)) = &self.status {
            if Instant::now() >= *expiry {
                self.status = None;
                return None;
            }
        }
        self.status.as_ref().map(|(msg, _)| msg.as_str())
    }

    /// The agent the preview pane should show: the focused member's.
    pub fn peek_agent(&self) -> Option<String> {
        self.focused_member()
            .and_then(|(_, m)| m.agent_info.as_ref())
            .map(|a| a.id.clone())
    }

    /// The preview pane shows another agent than the one it should.
    pub fn peek_stale(&self) -> bool {
        self.peek.agent != self.peek_agent()
    }

    /// The pane gives the keyboard back without having been asked to
    /// (`why`): said in the status line, and the keys still coming are
    /// dropped (see `dropping_keys_until`).
    pub fn stop_typing(&mut self, why: &str) {
        if matches!(self.mode, Mode::Terminal { .. }) {
            self.mode = Mode::Normal;
            self.dropping_keys_until = Some(std::time::Instant::now() + TYPING_PAUSE);
            self.set_status(
                format!("Back to the list: {why}"),
                std::time::Duration::from_secs(4),
            );
        }
    }

    /// Whether a key typed now is to be dropped (see `stop_typing`); each
    /// one pushes the pause back.
    pub fn drops_key(&mut self) -> bool {
        let now = std::time::Instant::now();
        match self.dropping_keys_until {
            Some(until) if now < until => {
                self.dropping_keys_until = Some(now + TYPING_PAUSE);
                true
            }
            _ => {
                self.dropping_keys_until = None;
                false
            }
        }
    }

    /// An agent is working: its spinner turns.
    pub fn any_running(&self) -> bool {
        self.tasks.iter().any(|t| {
            t.members
                .iter()
                .any(|m| matches!(t.member_status(m), crate::task::TaskStatus::Run))
        })
    }

    /// Count tasks needing attention (input, blocked, question or error).
    pub fn needs_attention_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.status().needs_attention())
            .count()
    }

    /// The member the detail pane shows and member actions apply to: the
    /// one focused in the selected task, else the one that most needs the
    /// user.
    pub fn focused_member(&self) -> Option<(&Task, &Member)> {
        let task = self.selected_task()?;
        let focused = self
            .focus
            .as_ref()
            .filter(|f| f.task == task.name)
            .and_then(|f| task.member(&f.member));
        Some((task, focused.or_else(|| most_urgent(task))?))
    }

    /// Focus the selected task's most urgent
    /// member when the selection is on another task than the focus (or the
    /// focused member left it). The focus then stays while the task is
    /// selected, whatever becomes more urgent: a key never acts on a member
    /// the pane did not show.
    pub fn sync_focus(&mut self) {
        let Some(task) = self.selected_task() else {
            self.focus = None;
            return;
        };
        let current = self
            .focus
            .as_ref()
            .is_some_and(|f| f.task == task.name && task.member(&f.member).is_some());
        if !current {
            self.focus = most_urgent(task).map(|m| Focus {
                task: task.name.clone(),
                member: m.id().to_string(),
            });
        }
    }

    /// Focus the next (`step` 1) or previous (-1) member of the selected
    /// task, wrapping around.
    pub fn cycle_member(&mut self, step: isize) {
        let Some((task, member)) = self.focused_member() else {
            return;
        };
        let count = task.members.len() as isize;
        let Some(pos) = task.members.iter().position(|m| m.id() == member.id()) else {
            return;
        };
        let next = (pos as isize + step).rem_euclid(count) as usize;
        self.focus = Some(Focus {
            task: task.name.clone(),
            member: task.members[next].id().to_string(),
        });
    }

    /// The pane shows another task's runs and messages than the selected
    /// one's, or they changed.
    pub fn detail_stale(&self) -> bool {
        let selected = self.selected_task().map(|t| t.name.as_str());
        match &self.detail {
            Some(detail) => detail.stale || Some(detail.task.as_str()) != selected,
            None => selected.is_some(),
        }
    }

    /// A message was sent or delivered: keep it if it is to or from a
    /// member of the task shown.
    pub fn note_message(&mut self, message: MessageInfo) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        let Some(task) = self.tasks.iter().find(|t| t.name == detail.task) else {
            return;
        };
        let ours = |id: &str| task.member(id).is_some();
        if !ours(&message.to) && !message.from.as_deref().is_some_and(ours) {
            return;
        }
        match detail.messages.iter_mut().find(|m| m.id == message.id) {
            Some(known) => *known = message,
            None => detail.messages.push(message),
        }
    }

    /// The next member worktree git should be asked about (marked as being
    /// looked at), one at a time: `(member id, dir)`. The focused member
    /// while the pane shows it, every [`branch::REFRESH`]; else the member
    /// looked at longest ago (never, or before its agent's state changed,
    /// first), every [`branch::BACKGROUND`]. Gone worktrees are left out.
    pub fn branch_due(&mut self) -> Option<(String, PathBuf)> {
        if self.branches.values().any(|o| o.running) {
            return None;
        }
        let due = |member: &Member, every| {
            !member.is_unhealthy() && self.branches.get(member.id()).is_none_or(|o| o.due(every))
        };
        let focused = self
            .focused_member()
            .map(|(_, m)| m)
            .filter(|m| due(m, branch::REFRESH));
        let member = focused.or_else(|| {
            self.tasks
                .iter()
                .flat_map(|t| &t.members)
                .filter(|m| due(m, branch::BACKGROUND))
                .min_by_key(|m| self.branches.get(m.id()).and_then(|o| o.at))
        })?;
        let (id, dir) = (member.id().to_string(), member.info.dir.clone());
        self.branches.entry(id.clone()).or_default().running = true;
        Some((id, dir))
    }

    /// What git said of a member's worktree.
    pub fn set_branch(&mut self, id: String, state: Result<BranchState, String>) {
        let observation = self.branches.entry(id).or_default();
        observation.state = Some(state);
        observation.at = Some(Instant::now());
        observation.running = false;
    }

    /// Look at a member's worktree again when it is next shown.
    fn branch_changed(&mut self, id: &str) {
        if let Some(observation) = self.branches.get_mut(id) {
            observation.at = None;
        }
    }
}

/// The member that most needs the user (the first one, in join order, of
/// the most urgent status): the one the task's status is about.
pub fn most_urgent(task: &Task) -> Option<&Member> {
    task.members
        .iter()
        .min_by_key(|m| task.member_status(m).sort_priority())
}

/// The repositories of a task's members, as its group in the task list
/// names them: `api`, or `api + web` for a task in both.
pub fn repo_group(task: &Task) -> String {
    let mut repos: Vec<&str> = task.members.iter().map(|m| m.info.repo.as_str()).collect();
    repos.sort_unstable();
    repos.dedup();
    repos.join(" + ")
}
