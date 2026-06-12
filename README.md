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

TAM manages **tasks** — named units of work that bind a directory to a series of AI agent runs. It unifies git worktree management and agent process supervision into a single tool.

Running multiple AI coding agents means juggling tmux panes, manual worktree creation, and remembering which session to resume. TAM replaces that with a single abstraction: create a task, and it gets a worktree, a branch, and an agent — all under one name. Status is always derived from reality (daemon, git, filesystem), never stored, so it can't drift.

## Quick start

```bash
# One-time: configure Claude Code hooks for state detection
tam init --agent claude

# Create a task — this creates branch "fix-auth" and worktree "myapp--fix-auth"
tam new fix-auth -w

# Start an agent and attach to it (full-screen, like tmux)
tam run fix-auth

# Detach with ctrl-a then b — the agent keeps running in the background
# Check all tasks at a glance
tam ps

# Or open the interactive TUI dashboard
tam
```

## Key concepts

**Task** — a named unit of work binding a directory to agent runs. Tasks have two flavors:

- **Owned**: TAM creates a git worktree for the task. The task name becomes the branch name. `tam drop` cleans up both.
- **Borrowed**: TAM tracks agents in an existing directory without touching the filesystem.

**Status is always derived**, never stored. TAM checks the daemon (is an agent running?), the filesystem (does the worktree exist?), git (does the branch exist?), and activity timestamps (is the task stale?) every time you look.

| Status | Meaning |
|---|---|
| `● run` | Agent producing output |
| `▲ input` | Agent waiting for user prompt |
| `▲ block` | Agent waiting for permission |
| `○ idle` | No agent running, task exists |
| `◌ stale` | No activity for 30 days |
| `✗ gone` | Worktree or branch deleted externally |

## Features

- **Task = worktree + agent** — `tam new feat -w` creates a branch, a worktree, and optionally starts an agent, all under one name
- **Derived status** — never stores lifecycle state; computes it from the daemon, git, and filesystem every time you look
- **TUI dashboard** — real-time task table with peek mode (scrollback preview without attaching)
- **Session resume** — pick up where you left off or start fresh; session history is tracked in an append-only ledger
- **Auto-daemon** — the daemon starts on first command and shuts down after 30s idle; no manual management
- **Per-repo init** — `.tam.toml` copies untracked files and runs setup commands in new worktrees
- **Desktop notifications** — configurable alerts when agents need input or hit permission prompts
- **Custom TUI commands** — bind keys to shell commands (open editor, run tests, etc.)

## Commands

### Task lifecycle

```
tam new NAME                   Create task bound to current directory
tam new NAME -w                Create task with its own worktree
tam new NAME -w -s REF         Worktree branched from a specific ref

tam run NAME                   Start/resume an agent in the task
tam run NAME --new-session     Start a fresh session

tam stop NAME                  Kill the agent (task persists)
tam stop                       Resolve task from current directory

tam attach NAME                Full-screen attach to running agent
tam attach                     Resolve from current directory

tam drop NAME                  Kill agent + remove task (+ delete worktree if owned)
tam drop NAME -b               Also delete the git branch
```

### Observing

```
tam ps                         Task table with computed status
tam ps --json                  Machine-readable output

tam ls                         Discover projects and worktrees
tam ls PATH                    Discover under a specific directory

tam pick                       Fuzzy project picker (prints selected path)
tam pick -F "dmenu -l 20"     Use a custom finder instead of fzf
```

### TUI

Running `tam` with no arguments opens the dashboard:

```
┌────────────────────────────────────────────────────────────────────────────┐
│  tam — 4 tasks (1 needs input)                                             │
├────────────────────────────────────────────────────────────────────────────┤
│  STATUS    REPO     TASK          AGENT   OWN  DIR                   CTX   │
│  ● run     myapp    feat          claude   ✔   ~/wt/myapp--feat      34%   │
│▸ ▲ input   myapp    fix-nav       claude   ✔   ~/wt/myapp--fix-nav   67%   │
│  ○ idle    myapp    refactor      -        ✔   ~/wt/myapp--refac     -     │
│  ◌ stale   other    old-thing     -        ✘   ~/projects/other      -     │
├────────────────────────────────────────────────────────────────────────────┤
│  enter:attach  n:new  r:run  s:stop  d:drop  p:peek  q:quit                │
└────────────────────────────────────────────────────────────────────────────┘
```

Keys: `j`/`k` navigate, `enter` attaches, `n` creates a task, `r` runs an agent, `s` stops, `d` drops, `p` toggles peek (scrollback preview), `/` filters, `q` quits. When attached to an agent, `ctrl-a` then `b` detaches and returns to the dashboard.

Tasks are sorted by repository name, then by status priority (blocked → input → running → idle → stale → gone), then by task name.

### Setup

```
tam init --agent claude        Configure Claude Code hooks (optional — see below)
tam shutdown                   Stop all agents and kill the daemon
tam status                     Check if the daemon is running
```

`tam init` is optional: the daemon installs the Claude state-detection hooks
automatically before the first spawn, so notifications work out of the box.
Run it yourself only if you want the hooks in place ahead of time.

## Remote access (`tam serve`)

`tam serve` exposes the daemon over HTTP + WebSocket so you can list, spawn,
attach to (view *and* drive), and kill agents from a browser — typically your
phone. It connects to the same Unix socket as the CLI and re-exposes the
protocol; all network-facing code lives in this one bridge, leaving the daemon
untouched.

```
tam serve                      Run the bridge (binds your Tailscale IP)
tam serve --port 9000          Use a different port (default 8765)
tam serve --install-service    Install a systemd --user service (auto-start on login)
```

On startup it prints the access URL (`http://<tailscale-ip>:<port>/?token=…`);
open it once on your phone and "Add to Home Screen". With a Slack Incoming
Webhook configured (`--slack-webhook` or `TAM_SLACK_WEBHOOK`) it posts that link
on startup and pings you when an agent needs you — `blocked` (permission prompt)
or `input` (finished, awaiting a prompt) — subject to each agent's per-session
bell toggle.

### Security model

The bridge is **safe by default** because it is designed to live entirely on
your [Tailscale](https://tailscale.com) tailnet, with the token as a second
layer:

- **Tailnet-only by default.** `--bind` defaults to `auto`, which binds your
  Tailscale IP (falling back to `127.0.0.1` if Tailscale isn't up) — **never**
  all interfaces. So only devices signed into *your* Tailscale account can reach
  it; your LAN, Docker bridges, and other VPNs cannot. Any device with Tailscale
  works from anywhere (home wifi, cellular, abroad).
- **Encryption comes from Tailscale.** The bridge speaks plain HTTP/WebSocket —
  it has **no TLS and no SSH of its own**. Transport encryption and device
  authentication are provided by Tailscale's WireGuard tunnel. This is why the
  default bind keeps it on the tailnet.
- **Bearer token.** Every request needs a 128-bit random token (`?token=…`),
  generated from `/dev/urandom` on first run. It's stored in
  `~/.config/tam/serve.env` with `0600` permissions and injected via the systemd
  unit's `EnvironmentFile`, never written into the unit itself. It's defense in
  depth on top of the tailnet, and is stable across restarts.
- **Slack webhook is outbound-only.** tam *posts to* Slack; nothing inbound. The
  webhook URL is a secret kept in the same `0600` env file.

**Exposing it more broadly** (e.g. `--bind 0.0.0.0`) is an explicit, deliberate
choice. Don't do it on an untrusted network without your own TLS and
authentication in front — over plain HTTP the token travels in the URL and can
be sniffed or logged. To confirm what the bridge is actually bound to:

```
ss -tlnp | grep 8765           # should show <tailscale-ip>:8765, not 0.0.0.0
```

## Configuration

TAM reads `~/.config/tam/config.toml`:

```toml
[spawn]
default_agent = "claude"

[worktree]
root = "~/worktrees"
auto_init = true              # run .tam.toml after worktree creation

[discovery]
max_depth = 5
ignore = [".*", "node_modules", "target"]

[daemon]
scrollback = 1048576

[notify]
command = "notify-send 'tam: {task}' '{status}'"
on_states = ["input", "blocked"]

[session]
finder = "fzf"

[[tui.commands]]
name = "open in editor"
key = "o"
command = "code {dir}"
```

Per-repo worktree initialization is configured via `.tam.toml` at the project root. When `tam new NAME -w` creates a worktree, it copies files matching `include` globs from the main checkout (useful for untracked secrets or editor config) and runs `commands` inside the new worktree:

```toml
[init]
include = [".env", ".claude/**"]
commands = ["npm install"]
```

## Agent providers

| Provider | State detection | Context tracking | Setup |
|---|---|---|---|
| **Claude Code** | Hook-based (immediate) | Reads session JSONL for token usage | `tam init --agent claude` |
| **Codex** | PTY heuristic (5s idle) | Reads session JSONL | None |
| **Any CLI** | PTY heuristic (5s idle) | None | None |

Claude Code uses hooks (`~/.claude/settings.json`) to report state changes instantly — TAM knows the moment an agent needs input or hits a permission prompt. Other providers fall back to a PTY idle-time heuristic. Any command-line program can be used as a provider: `tam run feat -a my-tool`.

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
