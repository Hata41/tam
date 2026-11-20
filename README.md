<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/logo.svg">
    <img alt="TAM" src="assets/logo.svg" width="243">
  </picture>
  <br>
  <strong>Terminal Agent Multiplexer</strong>
</p>

<p align="center">
  <a href="https://github.com/ComeBertrand/tam/actions/workflows/ci.yml"><img src="https://github.com/ComeBertrand/tam/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://crates.io/crates/tam-cli"><img src="https://img.shields.io/crates/v/tam-cli.svg" alt="crates.io"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
</p>

<p align="center">
  <a href="#features">Features</a> ·
  <a href="#install">Install</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#commands">Commands</a> ·
  <a href="#configuration">Configuration</a> ·
  <a href="#agent-providers">Providers</a>
</p>

> Manage units of work, not just processes.

TAM manages **tasks** — named units of work, each with its own git worktree and a series of AI agent runs. It unifies git worktree management and agent process supervision into a single tool.

Running multiple AI coding agents means juggling tmux panes, manual worktree creation, and remembering which session to resume. TAM replaces that with a single abstraction: create a task, and it gets a worktree, a branch, and an agent — all under one name. Status is always derived from reality (daemon, git, filesystem), never stored, so it can't drift.

## Quick start

```bash
# Create a task — this creates branch "fix-auth" and worktree "myapp--fix-auth"
tam new fix-auth

# Start an agent and attach to it (full-screen, like tmux)
tam run fix-auth

# Detach with ctrl-] — the agent keeps running in the background
# Check all tasks at a glance
tam ps

# Or open the interactive TUI dashboard
tam
```

## Key concepts

**Task** — a named unit of work. TAM creates a git worktree for it, on a branch named after the task, and `tam drop` cleans it up.

**Member** — a task's worktree, with at most one agent session. A task can hold several members, one per repository, for work spanning e.g. a backend and a frontend: `tam new feat -r api -r web` creates both worktrees, on branch `feat`, and starts an agent in each. Members have a stable id, which is also their agent's id (`$TAM_AGENT_ID`), and a short name within the task: the repository name. Name one as `TASK/MEMBER`; a bare task name means its only member.

**Status is always derived**, never stored. TAM checks the daemon (is an agent running?), the filesystem (does the worktree exist?), git (does the branch exist?), and activity timestamps (is the task stale?) every time you look.

| Status | Meaning |
|---|---|
| `● run` | Agent producing output |
| `◆ ready` | Agent waiting for user prompt |
| `▲ block` | Agent waiting for permission (or plan approval) |
| `? question` | Agent asked the user a question |
| `✗ error` | Agent's last turn failed (API error, rate limit…) |
| `○ idle` | No agent running, task exists |
| `◌ stale` | No activity for 30 days |
| `∅ gone` | Worktree deleted outside tam, or no longer a worktree |

## Features

- **Task = worktree + agent** — `tam new feat` creates a branch, a worktree, and optionally starts an agent, all under one name
- **Derived status** — never stores lifecycle state; computes it from the daemon, git, and filesystem every time you look
- **TUI dashboard** — real-time task table with peek mode (the agent's live screen and its history, without attaching)
- **Session resume** — a task picks up its current session with no question asked, or starts a fresh one on request; session history is tracked in an append-only ledger
- **Auto-daemon** — the daemon starts on first command and shuts down after 30s idle; no manual management
- **Per-repo init** — `.tam.toml` copies untracked files and runs setup commands in new worktrees
- **Desktop notifications** — configurable alerts when agents need input or hit permission prompts
- **Messages** — `tam msg` sends a message to a running session without attaching, from you or from another agent, and can wait for the reply
- **Peers** — an agent knows the other members of its task (their names, repositories and worktrees) and reaches them with `tam msg`; `tam peers` shows what their agents are doing
- **Merge and split** — `tam merge` makes tasks started separately one task, so their running agents become peers; `tam split` moves a member back out. Nothing stops or moves on disk
- **Custom TUI commands** — bind keys to shell commands (open editor, run tests, etc.)

## Commands

### Task lifecycle

```
tam new NAME                   Create task: branch NAME and its worktree, in the current repo
tam new NAME -s REF            Branch from a specific ref
tam new NAME -p TEXT           Start its agent on a prompt
tam new NAME -d                Don't attach to the agent (--json: print a receipt)
tam new NAME -r REPO -r REPO   ... in each of these repositories instead (one member each)
tam new NAME -r REPO --join    Add a worktree in another repository to the task NAME
tam new NAME --brief REF       Give the task a brief: a URL, a ticket id or a path (see Brief)
tam new NAME -b [DIR]          Work in a folder as it is (default: the current one), git or not

tam run NAME                   Start the task's agent, resuming its current session
tam run NAME --new-session     Start a new session instead
tam run NAME --session ID      Resume a given agent session
tam run NAME -p TEXT -d        On a prompt, without attaching (--json: print a receipt)

tam stop NAME                  Kill the task's agents (task persists)
tam stop                       Resolve the member from the current directory

tam attach NAME                Full-screen attach to running agent
tam attach                     Resolve from current directory

tam drop NAME                  Stop agents + remove worktrees + forget task
tam drop NAME -b               Also delete the git branches
tam drop NAME -f               Drop even if work would be lost
tam drop TASK/MEMBER           Drop one member only (its agent, its worktree)

tam adopt [PATH]               Make a worktree created outside tam a task
tam adopt PATH -n NAME         ... named NAME (default: its branch)
tam adopt PATH --join TASK     ... or a new member of an existing task
tam repair NAME                Recreate a worktree deleted outside tam

tam merge INTO FROM...         Move the members of tasks FROM into task INTO (FROM tasks go)
tam split TASK/MEMBER [NAME]   Move one member out into a task of its own (default: its branch)

NAME is a task name, or TASK/MEMBER for one member of the task.
```

`tam drop` checks first: it refuses when uncommitted changes would be lost, or, with `-b`, commits that no other branch, tag or remote branch holds. It lists them and, on a terminal, asks before going on. If a worktree cannot be removed, the task is kept.

The daemon checks every task's worktree against git (`git worktree list`) when it starts and every 10 seconds. A worktree deleted outside tam (`rm -rf`, `git worktree remove`) shows as `gone` in `tam ps` and the dashboard, with what can be done. `tam repair` recreates it on its branch: only if the branch still exists, is checked out nowhere else, and git's leftover registration of the directory (if any) has no staged changes. `tam drop` forgets the task. A worktree switched to another branch is not gone; `tam ps` notes the switch. TAM only looks: nothing is recreated or removed unless you ask.

`tam new` names a repository by a path to it (or to any directory in it), or by its project name as `tam ls` and `tam pick` show it (e.g. `api`, or `mnemo (work)` when two projects share a name). It is all or nothing: every repository is checked first (branch free, worktree directory free, `-s REF` present), and if a worktree still cannot be created, the ones created before it are removed again, with the branches created for them. What cannot be removed without losing work is kept, and the error says where. With several repositories, `tam new` starts an agent in each (`-p`: each gets the prompt) and attaches to the first one's (with `-d`, or without a terminal, it attaches to none and prints how to); `tam run`, `tam attach` and the preview act on one member (`NAME/REPO`); `tam stop` and `tam drop` act on the whole task, or on one member given as `NAME/REPO`. Git runs in the environment of the `tam` command that asked (checkout hooks and filters such as git-lfs see your `PATH`), for `tam repair` too.

`tam new NAME -b` borrows a folder instead of creating a worktree: the current one, or DIR (`--join` adds it to the task NAME). In git or not, tam works in it as it is: no branch, no checks, and `tam drop` (`-b` too) stops its agent and forgets it, leaving the folder and whatever is in it alone. A borrowed folder is `gone` while it is missing, and cannot be repaired; a task's worktree cannot be borrowed, but several tasks can borrow the same folder.

`tam adopt` takes over a worktree made by hand (`git worktree add`), from the worktree or any directory in it: it must be a linked worktree (not the main checkout) on a branch. From then on it is a task like any other: `tam drop` removes it.

`tam merge` and `tam split` change which task a member belongs to, and nothing else: worktrees, branches, sessions, run history, messages and running agents stay as they are. Started as two tasks, a backend and a frontend become one with `tam merge backend frontend`: the members of `frontend` join `backend` after its own, and `frontend` is gone. A member keeps its short name (its repository) unless the task has one by that name already; it is then `<repo>-<branch>`. Names through a task merged away (`frontend`, `frontend/web`) no longer resolve; member ids still do. `tam split backend/web` is the inverse: the member goes to a new task named after its branch, or NAME when a task has that name already. The only member of a task cannot be split out of it. Neither runs while one of the tasks is being dropped. Running Claude agents of the members involved learn of it at their next prompt or tool call: what changed, then their task and peers as they are now (an idle agent is not woken for it). Other agents are not told; `tam peers` shows their peers. The merged task keeps its brief (see Brief), and a task split off starts with the local `brief.md`; the directories of the tasks merged away stay in `~/.local/share/tam/tasks/`.

### Observing

```
tam ps                         Task table with computed status, a row per member
                               of a task with several
tam ps --json                  Machine-readable output

tam ls                         Discover projects and worktrees
tam ls PATH                    Discover under a specific directory

tam pick                       Fuzzy project picker (prints selected path)
tam pick -F "dmenu -l 20"     Use a custom finder instead of fzf
```

### Messages

```
tam msg NAME TEXT              Send a message to the member's agent (TEXT or stdin)
tam msg NAME TEXT --wait       ...and print its reply (--timeout SECS, default 90)

tam history NAME               The task's agent runs and messages
tam peers                      The other members of your task, and what their agents do
tam peers --all                ...of every task (--json: machine-readable)
```

A Claude agent gets a message right away: when idle it wakes up, when busy it reads it at its next step (after a pending permission prompt is answered). Other agents get it typed in once idle. A member with no running agent gets it when its next agent starts. Sent from inside an agent TAM started, a message comes from that agent, which the recipient can answer with `tam msg`; otherwise it comes from you. The reply `--wait` prints is the recipient's next message back, or its response at the end of the turn that got the message. Two agents answering each other are stopped after 6 messages, until you prompt one of them; an agent sends at most 20 messages per 10 minutes.

An agent TAM starts knows its task. A Claude agent is told when its session starts (again on resume, `/clear` and compaction): its task, its name, the other members with their repositories and worktrees, and the task's brief and scratch directory (see Brief), so a backend and a frontend session can agree on an API without you relaying. After `tam merge` or `tam split` changed its task, it is told again at its next prompt or tool call. Other agents have their member id in `$TAM_AGENT_ID` and can run `tam peers`. From an agent, `tam msg`, `tam run`, `tam stop`, `tam attach` and `tam split` take the short name of another member of its task (`tam msg frontend ...`), unless a task has that name; `tam peers` prints the name that works from where you run it (`TASK/MEMBER` from outside the task).

### Brief

```
tam brief [NAME]               The task's brief and scratch directory
tam brief NAME --set REF       Make REF the brief: a URL, a ticket id, a path
tam brief NAME --local         Make the local file brief.md the brief again
```

The members of a task work from one brief: the reference you give with `tam new --brief REF` or `tam brief --set REF` (a URL, a ticket id, a path; TAM neither fetches nor reads it), else the file `brief.md` in the task's directory (`~/.local/share/tam/tasks/NAME/`), which TAM starts with a heading per member. Next to it, `scratch/` is a directory the members share for transient notes. A Claude agent is told both when its session starts, and how to write in the brief: under its own heading, and what the members agree on (an API shape, a name) under `Shared`, so that two agents do not overwrite each other's part. A changed brief reaches running agents at their next session start (resume, `/clear`, compaction); `tam peers` shows the current one. A pattern in the configuration can turn a ticket id into its URL and into the agents' start prompt (see Configuration). Dropping the task removes its directory: a `brief.md` the members wrote in is kept in `~/.local/share/tam/tasks/.dropped/`, and `tam drop` says where.

A Claude agent can also read the other members' worktrees without asking, but not edit them: TAM adds them to the session's directories with a rule denying edits there, which covers Claude's file tools and the file commands it recognises in shell commands, not every program a command may run. A member that joined after the agent started is readable from the agent's next start. The task's directory is open to the agent as its worktree is: edits there follow its permission mode.

With several repositories, every agent starts on the same prompt (`-p`, or the pattern's): each one is told its own repository and its peers, and takes its part of the brief. To tell one member more, `tam msg TASK/MEMBER`.

### Scripting

For agents and scripts: nothing waits on a terminal, exit statuses tell what happened, and `--json` output has a versioned shape.

```
tam new NAME -p TEXT -d --json Create a task, start its agent on a prompt, print a receipt
tam run NAME -p TEXT -d --json Start the agent on a prompt, print a receipt
tam wait NAME                  Wait until the task's agents stop working; print their states
tam wait NAME -u ready -t 30m  ... until they wait for input, for at most 30 minutes
tam result NAME                Print the last response of the task's agents
tam ps --json                  Every task, as JSON
tam merge INTO FROM --json     Merge tasks; print who moved where
tam split TASK/MEMBER --json   Split a member out; print who moved where
```

`tam new` and `tam run` attach only when stdin and stdout are terminals: otherwise they start the agent and say so. `tam wait` returns once each agent is in one of the `--until` states, comma-separated: `working`, `blocked`, `ready`, `idle`, `question`, `error`, `exit` (no agent runs). The default is any state but `working`. `ready` (`input`, its former name, still works) takes in `idle`, since agents without hooks never report `ready`. It gives up at once when an agent exits or its turn fails, unless `--until` takes `exit` or `error`. `tam result` prints the response that ended the agent's last turn, in full; it is still there once the agent exited, or the daemon restarted. There is none while a turn runs, before the first turn of a run ends, after a failed or interrupted turn, and for agents other than Claude. For `wait` and `result`, NAME is a task (all its members), TASK/MEMBER or a member id. Both report what they find: right after `tam msg`, an agent that has not picked the message up yet is still `ready`, with its previous response. `tam msg --wait` waits for the reply instead, and `response_at` tells when a response came.

Claude asks whether to trust a folder it does not know yet, and runs no hook until it is answered: a detached agent in a new worktree waits there (`tam wait` sees `ready` after 10s, and `tam result` has nothing). Attach once to answer it.

| Exit status | Meaning | `error.kind` |
|---|---|---|
| 0 | Done | |
| 1 | Failed; `tam wait` gave up; `tam result` has no response; a merge or split refused | `failed`, `unreachable`, `no_result` |
| 2 | Invalid arguments (reported as text by the argument parser) | |
| 3 | No such task or member, or a task name where one member is needed | `not_found` |
| 4 | `tam wait` timed out | `timeout` |
| 5 | The daemon cannot be reached, or broke off | `daemon` |

With `--json`, stdout holds one JSON document and nothing else, also when the command fails: its `error` is then `{"kind": …, "message": …}`, next to what could still be reported (a command with nothing to report prints only `schema_version` and `error`). Every document has `"schema_version": 1`, raised only when a key is removed, renamed or retyped: new keys may come. Every key is always present, `null` when it has no value.

`tam ps --json`, tasks most urgent first:

```json
{
  "schema_version": 1,
  "tasks": [
    {
      "name": "feat",
      "status": "ready",
      "dir": "/home/me/worktrees/app--feat",
      "agent": "claude",
      "context_percent": 34,
      "run_count": 2,
      "last_activity": 1763400000,
      "created_at": 1763300000,
      "members": [
        {
          "id": "m-1a2b3c4d",
          "name": "app",
          "repo": "app",
          "branch": "feat",
          "dir": "/home/me/worktrees/app--feat",
          "status": "ready",
          "agent": "claude",
          "health": { "state": "ok", "branch": "feat" },
          "label": "feat",
          "repo_root": "/home/me/src/app",
          "state": "ready",
          "pid": 4242,
          "viewers": 0,
          "context_percent": 34,
          "session_id": "8c1f2a90-…",
          "pending": null,
          "turn_error": null,
          "denied": null,
          "last_message": "Fixed the bug: …",
          "last_run": {
            "provider": "claude",
            "session_id": "8c1f2a90-…",
            "started_at": 1763390000,
            "ended_at": null,
            "exit_code": null
          },
          "borrowed": false
        }
      ]
    }
  ]
}
```

- `status` (of a task: its most urgent member's) is `run`, `ready`, `block`, `question`, `error`, `idle`, `stale` or `gone`. `state` is the running agent's: `working`, `blocked`, `ready`, `idle`, `question` or `error`; `null` without one. A task's `dir`, `agent` and `context_percent` are its first member's.
- `pending` (while `blocked` or `question`) is `{"tool", "summary"}`, `turn_error` (while `error`) `{"kind", "message"}`, `denied` (the last tool call auto mode refused in this turn) `{"tool", "summary", "reason"}`.
- `session_id` is the running agent's session, else the last run's. `last_message` is the start of the agent's last response: `tam result` has all of it. A run's `exit_code` is -1 when it was stopped.
- `health` is what the daemon saw in git: `{"state": "ok", "branch"}`, `{"state": "gone", "cannot_repair"}` or `{"state": "broken", "problem"}`; `null` until checked.
- `borrowed` is `true` for a folder tam works in as it is (`tam new -b`): its `branch` is `""`, its `health` `null`, or `gone` while it is missing.
- Before version 1, `tam ps --json` printed the `tasks` array alone, with the keys up to `last_activity` (tasks) and `health` (members).

`tam new --json` and `tam run --json`: the task's members (`new`) or the one started (`run`). `agent` is `null` when none started, `resumed_session` is `null` for a new session.

```json
{
  "schema_version": 1,
  "task": "feat",
  "members": [
    {
      "id": "m-1a2b3c4d",
      "name": "app",
      "label": "feat",
      "repo": "app",
      "branch": "feat",
      "dir": "/home/me/worktrees/app--feat",
      "agent": "claude",
      "resumed_session": null,
      "borrowed": false
    }
  ],
  "error": null
}
```

`tam brief --json`: `file` is the local brief while there is no `reference`. `tam peers --json` has the same document, without `schema_version`, under `brief` (`null` outside a task).

```json
{
  "schema_version": 1,
  "task": "feat",
  "reference": "https://tickets.example.com/CS-42",
  "file": null,
  "scratch": "/home/me/.local/share/tam/tasks/feat/scratch"
}
```

`tam wait --json` and `tam result --json`: `state` is the agent's, or `exit` when none runs; `exit_code` then tells how its last run ended. `response` is the full text that ended the last turn, `response_at` when.

```json
{
  "schema_version": 1,
  "task": "feat",
  "members": [
    {
      "id": "m-1a2b3c4d",
      "name": "app",
      "label": "feat",
      "state": "ready",
      "agent": "claude",
      "session_id": "8c1f2a90-…",
      "exit_code": null,
      "pending": null,
      "turn_error": null,
      "response": "Fixed the bug: …",
      "response_at": 1763400000
    }
  ],
  "error": null
}
```

`tam merge --json` and `tam split --json`: `task` is the merged task, or the new one; `from` the tasks merged into it (gone now), or the task split from. `members` are the merged task's members, or the member split out followed by those left in its task. `previous_label` is how a member was named before (names that went with it no longer resolve), `moved` whether it changed task, `agent` its running agent (it keeps running), `notified` whether that agent is told at its next prompt or tool call.

```json
{
  "schema_version": 1,
  "task": "backend",
  "from": ["frontend"],
  "members": [
    {
      "id": "m-1a2b3c4d",
      "task": "backend",
      "name": "api",
      "label": "backend/api",
      "previous_label": "backend",
      "moved": false,
      "repo": "api",
      "branch": "backend",
      "dir": "/home/me/worktrees/api--backend",
      "agent": "claude",
      "notified": true
    },
    {
      "id": "m-5e6f7a8b",
      "task": "backend",
      "name": "web",
      "label": "backend/web",
      "previous_label": "frontend",
      "moved": true,
      "repo": "web",
      "branch": "frontend",
      "dir": "/home/me/worktrees/web--frontend",
      "agent": null,
      "notified": false
    }
  ],
  "error": null
}
```

### TUI

Running `tam` with no arguments opens the dashboard:

```
 tam  7 tasks  3 need you                                         by repository
 STATUS          TASK                                    BRANCH AGENT  CTX
 backend ───────────────────────────────────────────────────────────────────────
▌▲ block      3m fix-allotment-rounding                  ↑2 *1  claude  67% ━━━━
 ⠹ run       20s perf                                    =      claude  31% ━━━━
 ◌ stale     45d spike-cache                             ↑5 ↓12 -
 backend + frontend ────────────────────────────────────────────────────────────
                 unit-pricing
 ⠹ run        1m  ├ backend                              ↑1 *3  claude  12% ━━━━
 ? question   5m  └▸frontend                             ↑1     claude  40% ━━━━
 frontend ──────────────────────────────────────────────────────────────────────
 ○ idle       2d nav                                     =      -
 ∅ gone       3d old-banner                                     -
 rustpallet ────────────────────────────────────────────────────────────────────
 ◆ ready     12m offload-explain                         ↑3 ↓1  claude  55% ━━━━
 enter:attach  s:stop  d:drop  n:new  /:filter  ?:help  q:quit
```

Tasks are grouped by repository (a task in several under one header naming them, e.g. `backend + frontend`), the most urgent first in each (blocked/question → error → ready → running → idle → stale → gone), then by name. `t` lists them most urgent first across repositories instead, with a REPO column. A task with several members has a row per member below its own, which shows its name alone: each member's row has its status. A working agent's mark is a spinner, and CTX is how full its context is; `▸` marks the member the keys act on. Next to the status, how long it has been in it (without an agent: since the task's last activity). BRANCH compares each worktree with the default branch: `↑` commits ahead, `↓` behind, `*` paths with uncommitted changes, `=` none of them (`…` until git was asked). Git is asked, read-only and one worktree at a time, about every worktree once a minute and after its agent's state changes, and every 10 seconds about the one the pane shows. When the list is above the pane and at least 100 columns wide, a last column says what each member waits on. Columns the width cannot hold go first (AGENT, then CTX), task names last.

Keys: `j`/`k` navigate, `enter` gives the pane the keyboard (what you type goes to the agent, `ctrl-]` comes back to the list), `f` (or `shift-enter`, in terminals that tell it from `enter`: kitty, and others with its keyboard protocol) opens the agent's terminal on the whole screen, `n` creates a task (`tab` picks several repositories, `ctrl-n`/`ctrl-p` move in the list; an existing task's name adds them to it; a folder outside git, tagged `(folder)`, is borrowed as it is, and with one repository picked, `tab` at the name prompt borrows its folder instead of creating a worktree), `r` runs the agent in its current session, `R` in a new session, `s` stops, `a` answers what its agent asks, `d` drops (then `y` keeps the branch, `b` deletes it; in a task with several members, `m` drops the focused member only, `M` with its branch; work that would be lost is shown for a second confirmation; on a `gone` task, `r` recreates the worktree instead), the pane, always shown, is its agent's screen, which takes the pane's size (`h`/`l` or `←`/`→` move to another member of the task, `PgUp`/`PgDn` or `ctrl-u`/`ctrl-d` scroll through its history, `Home` goes to the oldest line, `End` back to live), `gg`/`G` go to the first/last task, `/` filters, `t` switches between grouped by repository and attention first, `T` switches between the `dark` and `unicorn` themes (until you quit: `theme` in the config is the one it starts with), `?` lists every key (custom commands included), `q` quits. When attached to an agent, `ctrl-]` detaches and returns to the dashboard. The footer shows the keys that fit.

The filter and the project picker match fuzzily: the letters typed, in order, not necessarily together (`unpr` finds `unit-pricing`), the matched letters highlighted. The filter looks at task names and at each member's name, repository and branch (`enter` keeps it, `esc` empties it with the task found still selected); the picker ranks the best matches first. In the filter, the picker and the name and path prompts, `←`/`→`, `home`/`end` and `ctrl-a`/`ctrl-e` move the cursor, `ctrl-←`/`ctrl-→` by word, `ctrl-w` deletes a word, `ctrl-u`/`ctrl-k` to the start/end. `n` opens the picker at once: the projects under `$HOME` are looked for in the background (those found last time show meanwhile).

The mouse: a click selects a task (with the pane open, a member's row shows that member) or a project in the picker; the wheel moves the selection over the list and in the picker, and scrolls the agent's screen over the pane. The dashboard takes the mouse while it is shown (most terminals select text with `shift` held), and lets go of it while attached to an agent.

The pane is the terminal of the selected task's agent, as it is right now. `enter` gives it the keyboard: every key goes to the agent, the dashboard's own keys too (`q`, `d`…), a paste arrives as one paste, `shift-enter` or `alt-enter` is a new line in the agent's prompt, and the pane's border says so until `ctrl-]` (or a click in the list) gives the keyboard back. While you type to an agent, tam sends no notification about it. If the agent exits meanwhile, the keys you were still typing are dropped, not run as dashboard keys. `f` attaches on the whole screen instead, as `tam attach` does. Its bottom border keeps what the terminal may no longer show: why its turn failed, what auto mode refused, and messages still waiting for the agent. Without an agent there is no terminal, and the pane says instead how the last run ended or what is wrong with the worktree, the branch against the default branch with its diff stat, and the latest messages. In a task with several members it shows the one that needs you; `←`/`→` show another, and the member keys (`enter`, `f`, `s`, `r`, `R`, `a`) act on the member shown. From 136 columns the pane is beside the list (wide enough for an agent's 80-column screen), else below it.

Quick replies, without attaching:

- `a` answers what a Claude agent asks: a permission (the tool's whole input is shown; `y` allows it once, `n` says no, with a reason the agent reads and goes on with, or without one, which asks it to stop and wait for you), a plan to approve (`y`: the agent leaves plan mode, and edits keep asking for permission), or its questions (`↑`/`↓` and `enter` pick an option, `space` ticks several, the last row takes an answer of your own; with several questions, `←`/`→` move between them and a last step sends them all). It answers the request it shows and no other: once that request is over (answered in the agent's terminal, or the agent moved on), the dialog says so and nothing is sent. A yes given in the terminal shows late, when the tool has run: an answer sent meanwhile goes to a request Claude has already answered, and changes nothing. Nothing is ever allowed for more than once, and a request too long to show in full (over 16,000 characters) can only be refused from the dashboard. Claude's own dialog stays in the agent's terminal meanwhile, and the first answer wins. Without the dashboard, the request waits in the terminal as usual.
- `m` sends the agent a message, as `tam msg` does: a Claude agent reads it at its next step (at once when idle), other agents get it typed in once idle, a member without an agent gets it when its next one starts.

What the dashboard cannot answer (an MCP server's input request, a request older than a day, Codex and other agents, whose prompts tam does not see) says so: attach to answer it there.

### Setup

```
tam init --agent claude        Remove the Claude hooks older versions installed globally
tam shutdown                   Stop all agents and kill the daemon
tam status                     Check if the daemon is running
```

## Configuration

TAM reads `~/.config/tam/config.toml`:

```toml
[spawn]
default_agent = "claude"
wrapper = ["direnv", "exec", "."]   # run every agent through it, in its worktree

[worktree]
root = "~/worktrees"
auto_init = true              # run .tam.toml after worktree creation

[discovery]
max_depth = 5
ignore = [".*", "node_modules", "target"]

[notify]
command = "notify-send 'tam: {task}' '{kind}: {message}'"
on_states = ["ready", "blocked", "question", "error"]   # default

[session]
finder = "fzf"

[tui]
theme = "dark"                          # 24-bit palette; "unicorn": brighter, with rainbows; "ansi": the terminal's 16 colours. Default: dark when COLORTERM says truecolor
animate = true                          # unicorn: the rainbows move (the logo, the selection's bar, the spinners)

[[tui.commands]]
name = "open in editor"
key = "o"
command = "code {dir}"

[[brief.patterns]]                      # tam new --brief CS-42, tam brief --set CS-42
match = '^CS-(\d+)$'                    # a regular expression: the first one that matches applies
reference = "https://tickets.example.com/CS-{1}"
prompt = "/implement-task {brief}"      # the agents' start prompt for tam new without -p
```

A brief pattern turns what you type into the task's brief (`reference`: `{0}` is the match, `{1}`… its groups, `{brief}` what you typed; without `reference`, the brief stays as typed), and gives the agents of `tam new` their start prompt (`prompt`, where `{brief}` is the brief). A brief that matches no pattern and names an existing file is recorded as its absolute path.

The notify command runs with `sh -c` on state changes, except while you are attached to the agent. Placeholders are escaped for the quoting they sit in:

| Placeholder | Value |
|---|---|
| `{task}`, `{id}` | Task name, agent id (the member id) |
| `{state}` (alias `{status}`), `{old_state}` | New and previous agent state |
| `{kind}` | `question`, `permission`, `done` or `error` |
| `{message}` | The question or permission asked, the agent's last response, or the error |
| `{tool}` | Tool waiting for permission (e.g. `Bash`) |
| `{error}` | Error kind (e.g. `rate_limit`) |

Per-repo worktree initialization is configured via `.tam.toml` at the project root. When `tam new NAME` creates a worktree, it copies files matching `include` globs from the main checkout (useful for untracked secrets or editor config) and runs `commands` inside the new worktree:

```toml
[init]
include = [".env", ".claude/**"]
commands = ["npm install"]
```

## Agent providers

| Provider | State detection | Context tracking | Setup |
|---|---|---|---|
| **Claude Code** | Hook-based (immediate) | Reads the session transcript for token usage | None |
| **Codex** | PTY heuristic (5s idle) | Reads session JSONL | None |
| **Any CLI** | PTY heuristic (5s idle) | None | None |

Claude Code reports state changes through hooks that TAM injects into each agent it spawns (`claude --settings`, nothing is written to your Claude settings): TAM knows the moment an agent finishes, asks a question, needs a permission or fails on an API error. The permission hook also waits, up to a day, for an answer from the dashboard (`a`); Claude shows its own dialog meanwhile, and once the hook gives up the dialog is still there, nothing approved. Claude runs no hooks until a folder is trusted, so a new worktree shows `ready` while Claude's trust prompt waits. An interrupted turn fires no hook; TAM takes a working agent silent for 10s as back at the prompt. If you ran `tam init --agent claude` with an older version, run it again to remove the global hooks it installed. Other providers fall back to a PTY idle-time heuristic. Any command-line program can be used as a provider: `tam run feat -a my-tool`.

An agent runs in the environment of what started it: the shell you ran `tam new` or `tam run` in, or the one the dashboard was launched from (its `PATH`, tools, SSH agent and credentials), not the daemon's, which may come from another shell started long ago. A resumed session gets yours too. It is passed to the agent only, never written down or logged. Left out are what belongs to you rather than the agent: an agent's own `TAM_AGENT_ID` and `TAM_SOCKET` when an agent starts another one, the variables a Claude Code session sets for the commands it runs (a session started with them is taken as nested in it), your terminal pane (`TMUX`, `ZELLIJ`, …), size (`COLUMNS`, `LINES`) and directory (`PWD`). The agent gets `TERM=xterm-256color` when you have no `TERM`, e.g. from a script.

Your shell is rarely the worktree's dev environment, the dashboard's least of all: set `wrapper` in `[spawn]` to a command that loads it, and every agent starts through it, in its worktree, on top of your environment. `["direnv", "exec", "."]` loads the worktree's `.envrc` (and unloads one loaded where you started). Any command that takes the agent's command as its last arguments and `exec`s it fits, such as `nix develop -c` or `mise exec --`. It runs at every start and resume, and what it prints or fails on shows on the agent's screen. direnv runs only an `.envrc` you allowed, and a new worktree's copy is not: add `direnv allow` to the `.tam.toml` init commands, or the agent stops at once. The wrapper is read at each start: no restart needed after a change.

Each task member has a current session per agent, which `tam run` and `r` resume: for Claude Code, the last session the agent had a turn in, as its hooks report it (so a `/clear` followed by a prompt moves it). `tam run --new-session` and `R` start a new one, which becomes current once you use it. A session whose transcript Claude deleted is not resumed: TAM says so and you start a new one. Codex and other agents report no sessions: they start a new session unless given one (`tam run --session ID`, codex only).

Messages (`tam msg`) reach a Claude agent through one more injected hook, which runs in the background and wakes the session (shown as `tam message` in its transcript). Other providers get them typed into their input once idle: as a paste for Codex, as a single line for any other program.

## Install

**Requirements**: git is required at runtime for all worktree and branch operations.

### Pre-built binaries

Download a binary for your platform from [GitHub Releases](https://github.com/ComeBertrand/tam/releases) (Linux x86_64/aarch64, macOS x86_64/aarch64).

### Cargo

```bash
cargo install tam-cli
```

### Nix

```bash
nix run github:ComeBertrand/tam
```

### Shell completions and man page

The build generates completions for bash, zsh, and fish, plus a man page (`tam.1`). When installing via `cargo install`, these are not placed automatically. After building from source, find them under the build output directory:

```bash
cargo build --release
find target/release/build/tam-cli-* -name 'completions' -o -name 'man'
```

## Contributing

Contributions are welcome — open an issue or submit a pull request. For the full design rationale and architecture, see [`tam_manifesto.md`](tam_manifesto.md).

## License

MIT
