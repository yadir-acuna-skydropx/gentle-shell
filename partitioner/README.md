# gentle-partitioner

A Rust terminal host that **partitions the terminal** and runs `gentle-shell`,
unchanged, inside its own pseudo-terminal. It is the visual/“supervisor” engine
described in the ODD: a larger engine that activates first when you launch the
shell and splits the work across concurrent threads so the user never perceives
a stall (S2). The existing gentle-shell structure is treated as a stable core
and is never modified — the partitioner only launches it as a child process (S1).

Each partition is a render of its own terminal, composed into a single visual
component (S3): when one pane needs to update, the whole terminal is not
repainted — only the changed cells of that fragment are written out.

## What it does

- **Chat mode (default).** Shows the `gentle-shell` chat exactly as it is today,
  plus a lateral **tool bar** (S7). Tools open an overlay on demand: the file
  tree, worktree changes, help, or a quit action.
- **Workspace mode.** A partitioned layout with a **sidebar** (folders, files,
  formats, sizes, git worktree status), a **center viewer** split into the file
  content (left) and the agent’s changes from `git diff` (right, read-only in
  this phase), and the **chat** at the bottom. On small terminals the layout
  degrades back to chat mode so the chat stays usable (S11).

## Build & run

Requires a Rust toolchain (`cargo`). From this directory:

```sh
cargo run --release
```

That launches the default command, `gentle-shell`. To wrap a different command,
pass it after `--`:

```sh
cargo run --release -- bash
cargo run --release -- sh -c 'echo hi; sleep 1'
```

Other flags:

```sh
gentle-partitioner --help      # usage
gentle-partitioner --version   # version
```

The child process inherits the current working directory, with
`TERM=xterm-256color` and `GENTLE_PARTITIONER=1` set. If the command cannot be
found on `PATH`, the partitioner prints a clear error naming it and exits with
status `1` without disturbing the terminal.

## Keybindings

`Ctrl+]` is the prefix chord; the next key runs a partitioner command. Anything
else is forwarded to the chat.

| Key (after `Ctrl+]`) | Action |
| --- | --- |
| `w` | Toggle workspace (files · editor · agent changes · chat) |
| `t` | Files tool (overlay) |
| `c` | Worktree changes tool (overlay) |
| `?` | Help overlay |
| `Tab` | Next pane · `1`-`4` focus sidebar / editor / changes / chat |
| `q` | Quit (stops gentle-shell) |
| `]` | Send a literal `Ctrl+]` to the chat |

- **Sidebar:** `↑↓` (or `k`/`j`) move · `Enter`/`→` open · `←` collapse / go to
  parent · `Esc` back to chat.
- **Editor / changes:** `↑↓` `PgUp` `PgDn` scroll · `Esc` back to chat.
- **Overlays:** `↑↓` move · `Enter` open · `Esc` close.
- **Mouse:** click selects panes, files and tools; the wheel scrolls the chat,
  the viewer, or the sidebar depending on where it is over.

## Architecture

- **`main.rs`** — the binary: terminal setup (raw mode, alternate screen, mouse
  capture, bracketed paste, restored on exit, error and panic), and the event
  loop. Three threads feed a single channel (S2): an **input** thread
  (`crossterm::event::read`), the **PTY reader** thread (inside `PtyPane::spawn`,
  coalescing wake-ups through an `AtomicBool`), and a **git-status** worker that
  runs `git status` off the UI thread. The UI thread blocks on the channel and
  repaints only after an event — it never polls and never repaints while idle
  (S11). PTY output schedules a git refresh at most once every ~800 ms.
- **`app.rs`** — all application state and the pure `handle_key` / `handle_mouse`
  transitions (unit-tested without a terminal), plus `draw`, which renders every
  partition into its own rectangle.
- **`layout.rs`** — the pure layout engine mapping the terminal area to
  independent partitions; degrades to chat mode on small terminals.
- **`pane.rs`** — a partition backed by its own PTY (`portable-pty` +
  `vt100`): spawn, resize, key→bytes write, scrollback, and copying only the
  emulator cells into ratatui’s buffer so render diffing writes just what
  changed (S3).
- **`tree.rs`** — the sidebar model: directory scan, sizes, formats and a
  `git status --porcelain` parser.
- **`viewer.rs`** — the center viewer: file content plus a classified `git diff`
  against `HEAD`.
- **`keys.rs`** — encodes crossterm key events as the bytes a terminal sends to
  a PTY child.

## Roadmap

- **T6 (S5)** — manual editing in the left editor pane, with save.
- **T7 (S8)** — translator engine: a pivot-through-English middleware around Pi,
  so the user always reads their native language while everything flows through
  English.
- **T8 (S9)** — prediction engine: deferred until a concrete use is decided
  (the LLM already predicts the next token).

## Fast development loop

`Cargo.toml` optimizes dependencies once (`[profile.dev.package."*"] opt-level = 3`)
and keeps this crate unoptimized, so an edit rebuilds in under a second while the
UI stays fluid. Linux x86_64 links with `lld` by default since Rust 1.90.

- Iterate with `cargo run` (dev profile); use `cargo run --release` for the final binary.
- Optional: `cargo install bacon` and run `bacon test` for rebuild-on-save.
- Only if linking becomes slow: install `mold` and add `-C link-arg=-fuse-ld=mold` in `.cargo/config.toml`.
