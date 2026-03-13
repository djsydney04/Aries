# codex-mux

Terminal multi-agent Codex supervisor powered by Rust.

`codex-mux` launches multiple Codex sessions in a TUI grid, keeps each session in its own terminal pane, and can isolate panes in per-agent Git worktrees.

## Features

- Multi-pane terminal UI for running several Codex agents at once.
- Command center for creating panes, switching repo paths, and sending shell lines.
- Optional per-agent Git worktrees under `.worktrees/`.
- Alerts when output contains a help signal (default token: `[[NEEDS_HELP]]`).
- Session/event storage in SQLite (`.codex_mux/session.db` by default).

## Prerequisites

- Rust toolchain with `cargo`
- Git
- `codex` CLI available on your `PATH`
- Node.js 18+ (only needed for npm-based usage/publishing)

## Build And Run (Rust)

```bash
cargo run --release -- --repo .
```

Run with worktrees disabled:

```bash
cargo run --release -- --repo . --no-worktree
```

## Build And Run (npm wrapper)

```bash
npm install
npm run build
npm exec codex-mux -- --repo .
```

The npm binary launcher is `bin/codex-mux.js`, which executes the compiled Rust binary from `target/release/`.

## CLI Options

```text
--repo <PATH>             Repository root to run in (default: .)
--base-branch <NAME>      Base branch for new worktrees (default: main)
--worktrees-dir <DIR>     Worktree directory under repo root (default: .worktrees)
--db-path <PATH>          SQLite session DB path (default: .codex_mux/session.db)
--help-token <TOKEN>      Alert token in agent output (default: [[NEEDS_HELP]])
--no-worktree             Disable Git worktree creation
```

## Command Center

In command mode, use:

- `codex` to open a new pane.
- `codex <prompt>` to open a new pane with a prompt.
- `codex <model> :: <prompt>` to set model + prompt.
- `new <model> :: <prompt>` (alias: `n`).
- `repo <path>` to switch repository path (alias: `r`).
- `/help` or `?` for quick guidance.

If a pane is selected, any other command-center line is sent to that pane as shell input.

## Keybindings

- `Ctrl-G`: open command mode
- `Ctrl-Q`: quit
- `Enter` (normal mode): focus selected pane for live typing
- `j`/`k` or arrow keys: move pane selection
- `a`: acknowledge alerts for selected pane
- `x`: stop selected pane
- Mouse click: focus pane or command center

## Development

```bash
cargo test
npm test
```

Release helper:

```bash
npm run release -- patch   # or minor / major
```

