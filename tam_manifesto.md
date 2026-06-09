# TAM — Terminal Agent Multiplexer

> Manage units of work, not just processes.

## Philosophy

### The problem

Modern AI coding agents (Claude Code, Codex, etc.) run as interactive terminal processes. A developer working on multiple tasks needs multiple agents, each in its own working directory, each with its own conversation history. The tools for managing this are ad-hoc: tmux panes, background jobs, manual worktree creation, remembering which session to resume. The cognitive overhead scales with the number of tasks.

Two concerns are tangled together:

- **State management**: creating working directories (worktrees), tracking branches, discovering projects, cleaning up when done.
- **Process management**: spawning agents, tracking what they're doing, attaching and detaching, session resume.

These are distinct but deeply coupled. An agent needs a directory to work in. A worktree is useless without something working in it. The interesting operations are the ones that bridge both: "create a worktree and start an agent" or "kill the agent and delete the worktree."

### The task model

TAM introduces the **task** as the central abstraction. A task is a named unit of work that binds a directory to a series of agent runs.

```
TASK
  ├── name: a short identifier ("feat", "fix-auth", "refactor-nav")
  ├── context: a directory, either owned or borrowed
  └── history: a list of agent runs, each tied to a provider session
```

**Owned context**: TAM created the worktree and can destroy it. The task name becomes the branch name and the worktree suffix. `tam new feat -w` creates branch `feat` and worktree `project--feat`. One name, everywhere.

**Borrowed context**: the directory already existed (the main repo, a manually created worktree, etc.). TAM manages agents in it but does not touch the filesystem. Cleanup removes the task from TAM's records, nothing more.

A task is not a process. An agent can start, stop, and restart within a single task. A task is not a session — multiple sessions can be used within the same task (fresh starts, different providers). A task is the persistent spine that ties directory, agent runs, and sessions together over time.

### No lifecycle assertions

TAM does not maintain a lifecycle state machine (created → running → done → archived). Such states would inevitably diverge from reality because TAM cannot intercept every git operation, PR merge, or external file change.

Instead, TAM **derives status from observable facts** every time you look:

| Status | How it's determined |
|---|---|
| `run` | Agent process is alive, producing output |
| `input` | Agent alive, waiting for user prompt |
| `block` | Agent alive, waiting for permission |
| `idle` | No agent running, task exists |
| `merged` | No agent running, branch is merged into default branch |
| `orphan` | Worktree exists but branch was deleted |
| `gone` | Worktree was deleted externally |

Status is always computed, never stored. The task ledger records **events** (run started, run ended, worktree created) but never states. This keeps TAM honest — it reports what it can see, not what it hopes is true.

### The ledger

TAM persists task metadata across daemon restarts in a lightweight append-only ledger (`~/.local/share/tam/ledger.jsonl`). The ledger records:

- Task creation (name, directory, owned/borrowed).
- Agent runs (start, stop, provider, session ID, exit code).
- Worktree deletion.

The ledger serves three purposes:

1. **Session resume**: when `tam run feat` starts, the ledger knows which provider sessions were used previously for that task, enabling the session picker without filesystem scraping.
2. **History display**: `tam ps` shows run count, turn count, and last activity without querying providers.
3. **Daemon independence**: the daemon is ephemeral (auto-starts, auto-shuts-down after 30s idle). The ledger survives daemon restarts, giving continuity.

The ledger is not a database. It is not queried in complex ways. It is a sequential log of facts that TAM reads on startup and appends to during operation.

### Directory uniqueness

At most one active task can be bound to a directory. Two agents in the same directory would conflict on files. For owned contexts, this is enforced by construction (each task creates its own worktree). For borrowed contexts, `tam new` refuses if there's already an active task in that directory.

### Naming

The task name is mandatory. It serves as the universal handle: the argument to `run`, `stop`, `attach`, `drop`. For owned tasks, it also becomes the git branch name and the worktree directory suffix, so it must be a valid branch name.

Making the name mandatory has a subtle benefit: it forces the developer to name what they're doing. `tam new fix-typo` is three seconds of thought that pays off in a readable `tam ps` output.

---

## Architecture

### Workspace structure

```
tam/
  Cargo.toml                   # workspace root
  crates/
    tam-cli/                   # binary: the tam command
    tam-daemon/                # library: daemon, agent, provider, scrollback
    tam-proto/                 # library: wire protocol (Request, Response, Event)
    tam-worktree/              # library: git, worktree, discovery, pretty, init
```

**`tam-proto`** defines the shared types and wire format for daemon-client communication. All messages are newline-delimited JSON over a Unix domain socket. The protocol is versioned with a handshake on connect. Message types are internally tagged serde enums.

**`tam-daemon`** owns agent processes. Each agent runs in a PTY. A background thread drains PTY output into a scrollback ring buffer and broadcasts to attached viewers. A state monitor polls for exits and state changes every second. The daemon auto-starts on first `tam` command and auto-shuts-down after 30 seconds with no agents and no clients.

**`tam-worktree`** is the library extracted from yawn. It handles project discovery, worktree creation/deletion, pretty naming, and worktree initialization. It knows nothing about agents or the daemon. Its public API is:

```rust
// Worktree lifecycle
tam_worktree::worktree::create(name, source, config, cwd) -> Result<PathBuf>
tam_worktree::worktree::delete(name, delete_branch, force, config, cwd) -> Result<()>

// Project discovery
tam_worktree::discovery::discover(root, ignore_set, max_depth) -> Result<Vec<PathBuf>>

// Pretty naming
tam_worktree::pretty::prettify(dir, paths) -> Result<String>
tam_worktree::pretty::resolve(name, paths) -> Result<PathBuf>
tam_worktree::pretty::build_pretty_names(paths) -> Vec<PrettyEntry>

// Configuration
tam_worktree::config::load_config() -> Result<Config>

// Initialization
tam_worktree::init::run(dir) -> Result<()>
```

**`tam-cli`** is the binary that wires everything together. It owns the CLI parser, the TUI, the task ledger, the client connection to the daemon, and the logic that bridges worktrees and agents (e.g., `tam new feat -w` calls `tam-worktree` to create the worktree, then sends a spawn request to the daemon).

### Daemon-client protocol

Communication happens over a Unix socket at `$XDG_RUNTIME_DIR/tam/sock`. The client connects, performs a version handshake, then sends JSON-line requests and receives JSON-line responses. Events (state changes, agent exits) are pushed from the daemon as unsolicited messages on the same connection.

When the client sends an `Attach` request, the connection transitions from JSON-line mode to raw byte mode — bidirectional relay between the client's terminal and the agent's PTY. Detach (`ctrl-a` then `b`) returns the client to the TUI or exits.

The daemon writes a PID file next to the socket for health checks. The client auto-starts the daemon as a detached background process if the socket is absent.

### State detection

State detection uses two strategies, selected per provider:

**Hook-based** (Claude Code): the agent process receives `TAM_AGENT_ID` and `TAM_SOCKET` environment variables at spawn. `tam init --agent claude` configures Claude Code hooks (in `~/.claude/settings.json`) that call `tam hook-notify` on events like `stop`, `user_prompt_submit`, `notification:permission_prompt`. The hook-notify command sends a best-effort message to the daemon, which maps the event to an agent state. This is immediate and accurate.

**PTY heuristic** (generic/codex): for agents without hooks, the daemon tracks time since last PTY output. Activity within 5 seconds means "working," silence beyond that means "idle." This cannot distinguish "waiting for input" from "thinking quietly," so it's a fallback.

**Context usage**: for Claude Code, the daemon periodically reads the agent's JSONL session file to extract token usage (input + cache tokens) and computes a percentage against the model's context limit. This is done in a two-phase pattern: collect metadata under the daemon lock, release the lock, do file IO, re-acquire and write back. This avoids holding the lock during disk access.

### Configuration

TAM reads configuration from `~/.config/tam/config.toml`. The worktree-related settings (worktree root, auto-init, discovery ignore patterns, max depth) mirror what was previously in yawn's config and serve as overrides — if `tam-worktree` also has its own config file at `~/.config/yawn/config.toml`, TAM's values take precedence. Users who only know TAM configure everything in one place; users migrating from yawn get their existing config as a fallback.

```toml
[spawn]
default_agent = "claude"                    # default provider

[worktree]
root = "~/worktrees"                        # where owned worktrees are created
auto_init = true                            # run .worktree-init.toml after creation

[discovery]
max_depth = 5
ignore = [".*", "node_modules", "target"]

[daemon]
scrollback = 1048576                        # bytes per agent

[notify]
command = "notify-send 'tam: {task}' '{status}'"
on_states = ["input", "blocked"]

[session]
finder = "fzf"                              # for tam pick

[[tui.commands]]
name = "open in editor"
key = "o"
command = "code {dir}"
```

### Worktree initialization

When TAM creates a worktree (via `tam new NAME -w`), it runs the initialization defined in `.worktree-init.toml` at the project root. This file is tool-agnostic — it describes what needs to happen for any new worktree, regardless of whether TAM or yawn created it.

```toml
# .worktree-init.toml
[init]
include = [".env", ".claude/**"]
commands = ["npm install"]
```

`include` copies files/globs from the main checkout that aren't tracked by git. `commands` runs shell commands in the new worktree (dependency installation, build setup, etc.).

The file is named `.worktree-init.toml`, not `.tam.toml` or `.yawn.toml`, because its semantics are generic. Any tool that creates worktrees can read it.

---

## CLI reference

### Task lifecycle

```
tam new NAME                        # create task bound to cwd (borrowed)
tam new NAME -w                     # create task with owned worktree
tam new NAME -w -s REF              # worktree branched from specific ref

tam run NAME                        # start/resume agent in existing task
tam run NAME --new-session          # start a new session, ignoring history

tam stop NAME                       # kill agent, task persists
tam stop                            # resolve from cwd

tam attach NAME                     # full-screen attach to running agent
tam attach                          # resolve from cwd

tam drop NAME                       # kill agent + remove task (+ delete worktree if owned)
tam drop NAME -b                    # also delete the git branch

tam gc                              # drop all tasks whose branch is merged
tam gc --dry-run                    # show what would be dropped
```

### Observing

```
tam ps                              # task table with computed status
tam ps --json                       # machine-readable output

tam ls                              # discover projects and worktrees
tam ls PATH                         # discover under specific directory
tam ls --json                       # machine-readable
tam ls --raw                        # paths only, one per line

tam pick                            # fuzzy project picker, prints selected path
```

### Setup and administration

```
tam init --agent claude             # configure agent hooks for state detection
tam shutdown                        # stop all agents, kill daemon
tam status                          # check if daemon is running
```

### Internal (not user-facing)

```
tam daemon                          # run the daemon process (auto-started by client)
tam hook-notify                     # hook callback from agent process
```

### Resolution rules

- `tam new NAME`: name is always mandatory and positional.
- `tam new NAME` errors if a task with that name already exists.
- `tam new NAME` (without `-w`) errors if there's already an active task in cwd.
- `tam run NAME` errors if no task with that name exists.
- `tam run NAME` errors if the task already has a running agent.
- `tam stop` and `tam attach` without a name resolve from cwd: find the task bound to the current directory. Error if zero or multiple matches.
- `tam new NAME -w`: name must be a valid git branch name.
- `tam drop NAME` on a borrowed task: kills agent, removes from ledger. Does not touch directory.
- `tam drop NAME` on an owned task: kills agent, deletes worktree, removes from ledger.
- `tam drop NAME -b`: also deletes the local git branch. Only valid for owned tasks.
- `tam gc` only operates on owned tasks with merged branches.

### Session resume flow

When `tam run NAME` is invoked and the task has previous agent runs recorded in the ledger:

1. If stdin is not a terminal: always start a new session.
2. If `--new-session` is passed: start a new session.
3. Otherwise: show a session picker.

The session picker presents:

```
  Resume: "fix the auth tests" (4 turns, 2h ago)
  Resume: "refactor auth module" (12 turns, 1d ago)
  New session
```

Session metadata (ID, first user message as summary, turn count, last modified) comes from the ledger cross-referenced with the provider's session files.

---

## TUI

### Default view

Running `tam` with no arguments opens the TUI. It connects to the daemon (starting it if needed), fetches the task list from the ledger, and subscribes to daemon events for real-time updates.

```
┌──────────────────────────────────────────────────────────────────┐
│  tam ─ 4 tasks (1 needs input)                                   │
├──────────────────────────────────────────────────────────────────┤
│  STATUS   TASK          AGENT    DIR                      CTX    │
│  ● run    feat          claude   ~/wt/myapp--feat         34%    │
│▸ ▲ input  fix-nav       claude   ~/wt/myapp--fix-nav      67%    │
│  ○ idle   refactor      -        ~/wt/myapp--refac        -      │
│  ✓ merged old-thing     -        ~/wt/myapp--old          -      │
├──────────────────────────────────────────────────────────────────┤
│  enter:attach  n:new  r:run  s:stop  d:drop  p:peek  q:quit     │
└──────────────────────────────────────────────────────────────────┘
```

The header shows total task count and how many need attention (input or blocked status). The table shows one row per task, sorted by priority: blocked first, then input, then running, then idle, then merged. The footer shows available keys, updated based on the selected task's state.

### Task table columns

| Column | Content |
|---|---|
| STATUS | Derived status indicator (see status table) |
| TASK | Task name |
| AGENT | Provider name if agent is running, `-` otherwise |
| DIR | Working directory (with `~` shortening) |
| CTX | Context window usage percentage (agent running only) |

### Status indicators

| Indicator | Status | Meaning |
|---|---|---|
| `● run` | Running | Agent producing output |
| `▲ input` | Input | Agent waiting for prompt |
| `▲ block` | Blocked | Agent waiting for permission |
| `○ idle` | Idle | No agent running |
| `✓ merged` | Merged | Branch merged, candidate for cleanup |
| `? orphan` | Orphan | Worktree exists, branch deleted |
| `✗ gone` | Gone | Worktree deleted externally |

### Keybindings — normal mode

| Key | Action | Available when |
|---|---|---|
| `j` / `k` / `↑` / `↓` | Navigate task list | Always |
| `enter` | Attach to agent | Agent is running |
| `n` | New task flow | Always |
| `r` | Run/resume agent flow | Task idle (no agent running) |
| `s` | Stop agent | Agent is running |
| `d` | Drop task (confirms for owned) | Always |
| `p` | Toggle peek panel | Always |
| `/` | Enter filter mode | Always |
| `q` | Quit TUI | Always |
| Config keys | Custom commands | Always |

The footer only shows keys applicable to the currently selected task. If the selected task has a running agent: `enter:attach  s:stop  p:peek  d:drop  q:quit`. If idle: `r:run  d:drop  p:peek  q:quit`.

### Peek mode

Pressing `p` splits the view. The left panel shows a compressed task list (fewer columns). The right panel renders the selected task's agent scrollback through a `vt100` terminal parser. As the user navigates with `j`/`k`, the right panel updates to show the selected task's output.

```
┌──────────────────────────────────┬───────────────────────────────┐
│  STATUS  TASK         CTX        │  fix-nav (claude, 8m)         │
│  ● run   feat         34%        │                               │
│▸ ▲ input fix-nav      67%        │  I've updated the navigation  │
│  ○ idle  refactor     -          │  component. Should I also     │
│  ✓ mrgd  old-thing    -          │  update the tests? [y/n]      │
├──────────────────────────────────┴───────────────────────────────┤
│  enter:attach  p:close peek  q:quit                              │
└──────────────────────────────────────────────────────────────────┘
```

Peek is read-only. To interact with the agent, press `enter` to attach.

### Filter mode

Pressing `/` enters filter mode. The header shows the filter text. Typing filters the task list by name, provider, or directory (case-insensitive substring match). `esc` clears the filter and returns to normal mode. Navigating and pressing `enter` on a result selects it and exits filter mode.

### New task flow

Pressing `n` in the TUI starts a multi-step flow:

**Step 1 — Select project**: a picker showing discovered projects (via `tam-worktree` discovery). "Current directory" is always an option. The user navigates and selects.

```
┌──────────────────────────────────────────────────────────────────┐
│  new task ─ select project                                       │
├──────────────────────────────────────────────────────────────────┤
│▸   myapp                  ~/projects/myapp                       │
│    other-project          ~/projects/other                       │
│    ── or ──                                                      │
│    [current directory]    ~/projects/myapp                        │
├──────────────────────────────────────────────────────────────────┤
│  enter:select  esc:cancel                                        │
└──────────────────────────────────────────────────────────────────┘
```

**Step 2 — Name and options**: a text input for the task name with toggles for worktree creation and immediate agent start.

```
┌──────────────────────────────────────────────────────────────────┐
│  new task in myapp                                               │
├──────────────────────────────────────────────────────────────────┤
│  Task name: feat█                                                │
│                                                                  │
│  [x] Create worktree                                             │
│  [ ] Start agent immediately                                     │
├──────────────────────────────────────────────────────────────────┤
│  enter:create  tab:toggle  esc:cancel                            │
└──────────────────────────────────────────────────────────────────┘
```

After confirmation, TAM creates the task (and worktree if selected), optionally spawns an agent (and attaches if "start agent" was checked), then returns to normal mode.

### Run/resume flow

Pressing `r` on an idle task that has previous sessions triggers the session picker:

```
┌──────────────────────────────────────────────────────────────────┐
│  run agent in feat                                               │
├──────────────────────────────────────────────────────────────────┤
│▸   Resume: "fix the auth tests" (4 turns, 2h ago)               │
│    Resume: "refactor auth module" (12 turns, 1d ago)             │
│    New session                                                   │
├──────────────────────────────────────────────────────────────────┤
│  enter:select  esc:cancel                                        │
└──────────────────────────────────────────────────────────────────┘
```

If the task has no session history, `r` spawns a new agent directly.

### Attached mode

Pressing `enter` on a running task enters attached mode. The full screen is handed to the agent's PTY, with a thin status bar:

```
│  tam ▸ feat (claude, 34% ctx, 12m) ─ C-a b:detach              │
```

`ctrl-a` then `b` detaches and returns to the task list. On detach, terminal state is fully reset (alternate screen, mouse modes, keyboard protocols, colors, cursor visibility).

Output from the agent is filtered to strip keyboard protocol escape sequences (Kitty keyboard protocol, xterm modifyOtherKeys). This ensures the `ctrl-a` leader is always recognized as the raw byte `0x01` regardless of what TUI the agent runs internally.

### Real-time updates

The TUI maintains a persistent daemon connection and receives pushed events:

- `StateChange`: status column updates, rows re-sort (blocked/input bubble to top).
- `AgentExited`: task status recomputed from git state (may become idle or merged).
- `AgentSpawned`: row updates to show running agent.
- `ContextUpdate`: CTX column updates.

Git branch state (merged status) is refreshed periodically (every ~30 seconds) since it changes externally.

---

## Buildup: from zinc and yawn to TAM

### What comes from zinc

The daemon-client architecture moves almost entirely intact:

| zinc component | TAM destination | Changes |
|---|---|---|
| `zinc-proto` | `tam-proto` | Rename. Add task-related event types (`TaskCreated`, `TaskDropped`). Wire format unchanged. |
| `zinc-daemon/daemon.rs` | `tam-daemon/daemon.rs` | Rename. Daemon gains awareness of task names (agents are associated with tasks, not just IDs). Auto-shutdown logic unchanged. |
| `zinc-daemon/agent.rs` | `tam-daemon/agent.rs` | Unchanged. PTY management, scrollback, broadcast, viewer tracking. |
| `zinc-daemon/provider.rs` | `tam-daemon/provider.rs` | Unchanged. `ClaudeProvider`, `CodexProvider`, `GenericProvider`. Hook mapping, context usage parsing. |
| `zinc-daemon/scrollback.rs` | `tam-daemon/scrollback.rs` | Unchanged. Ring buffer. |
| `zinc-daemon/notify.rs` | `tam-daemon/notify.rs` | Template variables rename (`{id}` → `{task}`). |
| `zinc-cli/client.rs` | `tam-cli/client.rs` | Unchanged. Socket connection, raw mode, attach relay, keyboard protocol filter, terminal reset. |
| `zinc-cli/tui/` | `tam-cli/tui/` | Rework. Task-centric table instead of agent-centric. New modes: new-task flow, session picker, filter mode. Peek panel. |
| `zinc-cli/sessions.rs` | `tam-cli/sessions.rs` | Refactor. Session discovery supplements ledger data rather than being the sole source. |
| `zinc-cli/config.rs` | `tam-cli/config.rs` | Merge with yawn config concerns. Single config file at `~/.config/tam/config.toml`. |
| `zinc init --agent` | `tam init --agent` | Unchanged. Writes hooks to `~/.claude/settings.json` referencing `tam hook-notify`. |

### What comes from yawn

The library modules move into `tam-worktree` with minimal changes:

| yawn component | TAM destination | Changes |
|---|---|---|
| `git.rs` | `tam-worktree/git.rs` | Unchanged. All functions take `&Path`, return `Result`. No TAM-specific logic. |
| `worktree.rs` | `tam-worktree/worktree.rs` | Unchanged. `create()` and `delete()` are pure git operations. |
| `discovery.rs` | `tam-worktree/discovery.rs` | Unchanged. `discover()` scans for git projects. |
| `pretty.rs` | `tam-worktree/pretty.rs` | Split. Core logic (`build_pretty_names`, `prettify`, `resolve`) goes to library. Colored tree output stays in CLI (or is feature-gated behind a `cli` feature). |
| `config.rs` | `tam-worktree/config.rs` | Unchanged as library. TAM-CLI loads it as fallback behind TAM's own config. |
| `init.rs` | `tam-worktree/init.rs` | Rename config file lookup: `.worktree-init.toml` primary, `.yawn.toml` fallback. |
| `cli.rs` | dropped | TAM-CLI defines its own CLI structure. |
| `session.rs` | dropped | Session opener (`session::open`) is yawn-specific (launch terminal). TAM doesn't need it. |
| `main.rs` | dropped | Replaced by TAM-CLI's main. |

### What's new in TAM

| Component | Location | Purpose |
|---|---|---|
| Task ledger | `tam-cli/ledger.rs` | Append-only JSONL at `~/.local/share/tam/ledger.jsonl`. Records task creation, agent runs, session IDs. Read on startup, appended during operation. |
| Task model | `tam-cli/task.rs` | In-memory representation of tasks. Computed status from ledger + daemon state + git state. |
| `tam new` logic | `tam-cli/main.rs` | Bridges `tam-worktree::create()` and daemon spawn. Creates ledger entry. |
| `tam run` logic | `tam-cli/main.rs` | Session picker using ledger history. Sends spawn request with resume session ID. |
| `tam gc` logic | `tam-cli/main.rs` | Queries git merged status for all owned tasks, drops those that are merged. |
| `tam pick` | `tam-cli/main.rs` | Uses `tam-worktree::discovery` + `tam-worktree::pretty`, pipes through configured finder, prints selected path. |
| `tam ls` | `tam-cli/main.rs` | Uses `tam-worktree::discovery` + `tam-worktree::pretty` for project listing. Annotates with git and task status. |
| TUI new-task flow | `tam-cli/tui/` | Multi-step modal: project picker → name input → confirmation. |
| TUI session picker | `tam-cli/tui/` | Modal shown on `r` key for idle tasks with session history. |

### What can be published separately

`tam-worktree` has no dependency on agents, daemons, or TAM-specific concepts. It could be published as a standalone crate for any tool that needs git worktree management and project discovery. The `git-yawn` binary could be maintained as a thin CLI wrapper over `tam-worktree` for users who want worktree navigation without agent management, though this is low priority given current usage.

### Migration path

1. Create the `tam` workspace with four crates.
2. Move `zinc-proto` → `tam-proto`, `zinc-daemon` → `tam-daemon` with renames.
3. Extract yawn library modules into `tam-worktree`, add `[lib]` with public API.
4. Build `tam-cli` by refactoring `zinc-cli`: new CLI parser, task ledger, `tam-worktree` integration.
5. Publish final versions of `git-yawn` and `zinc-cli` pointing users to `tam-cli`.
6. Archive the yawn and zinc repositories.
